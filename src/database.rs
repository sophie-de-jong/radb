//! Database integration API.
//!
//! [`Database`] owns a buffer pool and catalog and drives the four modules:
//! SQL text, `EXPLAIN`, `ANALYZE` and the bulk loader. It never prints; each
//! operation returns its result for the caller to present.

use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};

use crate::exec::Executor;
use crate::exec::plan::Plan as PhysicalPlan;
use crate::optimize::analyze::analyze_table;
use crate::optimize::{ExplainNode, Optimizer};
use crate::sql::compiler::{Compiler, Statement};
use crate::storage::report::{PageLayout, ScanReport, SlotLayout};
use crate::storage::{
    BufferPool, Catalog, HeapFile, PageId, Policy, StorageError, TableMeta, slotted,
    validate_table_name,
};
use crate::{Row, Schema};
use crate::{Value, csv};

/// A database: a buffer pool, its catalog, and the four modules wired together.
pub struct Database {
    path: PathBuf,
    frames: usize,
    policy: Policy,
    pool: BufferPool,
    catalog: Catalog,
}

impl Database {
    /// Open a database with the default 16-frame LRU pool.
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self, StorageError> {
        Self::open_with(path, 16, Policy::Lru)
    }

    /// Open a database with `frames` frames and a replacement policy.
    pub fn open_with<P: AsRef<Path>>(
        path: P,
        frames: usize,
        policy: Policy,
    ) -> Result<Self, StorageError> {
        let path = path.as_ref().to_path_buf();
        fs::create_dir_all(&path).ok();
        let frames = frames.max(1);
        let mut pool = BufferPool::new(frames, policy);
        let catalog = Catalog::open(&mut pool, path.join("catalog.db"))?;
        Ok(Database {
            path,
            frames,
            policy,
            pool,
            catalog,
        })
    }

    /// Resize the buffer pool, reopening the catalog in it.
    ///
    /// The old pool is flushed before it is replaced; if the catalog cannot be
    /// reopened the error is returned and the database keeps its old pool rather
    /// than referring to a stale file id.
    pub fn set_frames(&mut self, frames: usize) -> Result<(), StorageError> {
        let frames = frames.max(1);
        if frames == self.frames {
            return Ok(());
        }
        self.pool.flush_all()?;
        let mut pool = BufferPool::new(frames, self.policy);
        let catalog = Catalog::open(&mut pool, self.path.join("catalog.db"))?;
        self.frames = frames;
        self.pool = pool;
        self.catalog = catalog;
        Ok(())
    }

    /// The database directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The buffer pool.
    pub fn pool(&self) -> &BufferPool {
        &self.pool
    }

    /// The buffer pool, mutably.
    pub fn pool_mut(&mut self) -> &mut BufferPool {
        &mut self.pool
    }

    /// The catalog.
    pub fn catalog(&self) -> &Catalog {
        &self.catalog
    }

    /// The catalog, mutably.
    pub fn catalog_mut(&mut self) -> &mut Catalog {
        &mut self.catalog
    }

    /// Run every statement in `sql`, returning the rows any queries produced.
    ///
    /// Statements are compiled and executed one at a time, so a later statement
    /// sees a table an earlier one just created. The shell splits a script on
    /// semicolons and calls this once per statement.
    pub fn execute_sql(&mut self, sql: &str) -> Result<Vec<Row>, Box<dyn Error>> {
        let mut compiler = Compiler::new(sql)?;
        let mut rows = Vec::new();
        while let Some(statement) = compiler.compile(&self.catalog)? {
            match statement {
                Statement::Select(plan) => {
                    let mut optimizer = Optimizer::new(plan, &self.catalog, self.frames);
                    optimizer.optimize()?;
                    let plan = optimizer.finish()?;
                    let new_rows = self
                        .executor(&plan)?
                        .by_ref()
                        .collect::<Result<Vec<_>, _>>()?;
                    rows.extend(new_rows);
                }
                Statement::Create { name, schema } => self.create_table(&name, schema)?,
                Statement::Insert { table_name, values } => self.insert(&table_name, &values)?,
            }
        }
        Ok(rows)
    }

    /// Create an empty table.
    pub fn create_table(&mut self, name: &str, schema: Schema) -> Result<(), Box<dyn Error>> {
        validate_table_name(name)?;
        if self.catalog.get(name).is_some() {
            return Err(StorageError::DuplicateTable {
                name: name.to_string(),
            }
            .into());
        }
        let mut meta = TableMeta::new(name, schema);
        let file = self.pool.open_file(self.path.join(&meta.file_name))?;
        let heap = HeapFile::new(file, name, meta.schema.clone());
        heap.create(&mut self.pool)?;
        meta.num_pages = heap.num_pages(&self.pool);
        self.catalog.insert(meta)?;
        self.save()?;
        Ok(())
    }

    /// Insert a single row into `table`.
    pub fn insert(&mut self, table: &str, values: &[Value]) -> Result<(), Box<dyn Error>> {
        let meta = self.catalog.require_mut(table)?;
        let file = self.pool.open_file(self.path.join(&meta.file_name))?;
        let heap = HeapFile::new(file, table, meta.schema.clone());
        heap.insert(&mut self.pool, values)?;
        meta.num_tuples += 1;
        meta.num_pages = heap.num_pages(&self.pool);
        self.save()?;
        Ok(())
    }

    /// Bulk-load a CSV file into `table`, returning the number of rows inserted.
    pub fn load_table(&mut self, table: &str, file: &Path) -> Result<u64, Box<dyn Error>> {
        let meta = self.catalog.require_mut(table)?;
        let table_file = self.pool.open_file(self.path.join(&meta.file_name))?;
        let heap = HeapFile::new(table_file, table, meta.schema.clone());

        let text = fs::read_to_string(file).map_err(|e| format!("{}: {e}", file.display()))?;
        let mut inserted = 0u64;
        for row in csv::CsvReader::new(&meta.schema, &text) {
            heap.insert_at(&mut self.pool, &row?, inserted + 1)?;
            inserted += 1;
        }
        meta.num_tuples += inserted;
        meta.num_pages = heap.num_pages(&self.pool);
        self.save()?;
        Ok(inserted)
    }

    /// Scan `table`, stopping after `limit` rows when one is given.
    ///
    /// The returned report includes the catalog read's counters, as the
    /// documented `scan --json` behaviour does.
    pub fn scan_table(
        &mut self,
        table: &str,
        limit: Option<usize>,
    ) -> Result<ScanReport, Box<dyn Error>> {
        let meta = self.catalog.require(table)?.clone();
        let file = self.pool.open_file(self.path.join(&meta.file_name))?;
        let heap = HeapFile::new(file, table, meta.schema.clone());
        let mut scan = heap.scan(&mut self.pool)?;
        let mut rows = Vec::new();
        while !limit.is_some_and(|limit| rows.len() >= limit) {
            match scan.next(&mut self.pool, &meta.schema)? {
                Some((_rid, values)) => rows.push(Row::from(values)),
                None => break,
            }
        }
        scan.close(&mut self.pool);
        Ok(ScanReport {
            table: table.to_string(),
            schema: meta.schema,
            rows,
            stats: self.pool.stats(),
        })
    }

    /// Read one page of `table` and describe its slotted layout.
    pub fn page_layout(&mut self, table: &str, page: u32) -> Result<PageLayout, Box<dyn Error>> {
        let meta = self.catalog.require(table)?.clone();
        let file = self.pool.open_file(self.path.join(&meta.file_name))?;
        let total = self.pool.num_pages(file);
        if page >= total {
            return Err(StorageError::BadArg {
                detail: format!("page {page} is past the end of table '{table}' ({total} pages)"),
            }
            .into());
        }
        let pinned = self.pool.fetch_page(file, PageId(page))?;
        let data = self.pool.page(file, pinned);
        let slot_count = slotted::slot_count(data);
        let slots = (0..slot_count)
            .map(|slot| {
                let (offset, length) = slotted::slot(data, slot);
                SlotLayout {
                    slot,
                    offset,
                    length,
                }
            })
            .collect();
        let layout = PageLayout {
            table: table.to_string(),
            file_name: meta.file_name.display().to_string(),
            page,
            page_id: slotted::page_id(data).0,
            slot_count,
            free_end: slotted::free_end(data),
            free_space: slotted::free_space(data) as u32,
            slots,
        };
        self.pool.unpin(file, pinned, false)?;
        Ok(layout)
    }

    /// Compute and store `table`'s statistics with `buckets` histogram buckets.
    pub fn analyze(&mut self, table: &str, buckets: usize) -> Result<(), Box<dyn Error>> {
        let meta = self.catalog.require_mut(table)?;
        let stats = analyze_table(&mut self.pool, &self.path, meta, buckets)?;
        meta.stats = Some(stats);
        self.save()?;
        Ok(())
    }

    /// Explain `query`; when `analyze`, also run it and attach its counters.
    pub fn explain(&mut self, query: &str, analyze: bool) -> Result<ExplainNode, Box<dyn Error>> {
        let mut compiler = Compiler::new(query)?;
        let plan = compiler.compile_select(&self.catalog)?;
        let mut optimizer = Optimizer::new(plan, &self.catalog, self.frames);
        optimizer.optimize()?;
        let plan = optimizer.finish()?;
        let mut explain = crate::optimize::explain(&plan, &self.catalog, self.frames)?;
        if analyze {
            let mut exec = self.executor(&plan)?;
            let _rows = exec.by_ref().collect::<Result<Vec<_>, _>>()?;
            let mut counters = exec.walk().map(|(_, node)| node.counters());
            explain.attach_counters(&mut counters);
        }
        Ok(explain)
    }

    /// Build and open `plan` over this database, ready to pull rows.
    pub fn executor<'a>(&'a mut self, plan: &PhysicalPlan) -> Result<Executor<'a>, Box<dyn Error>> {
        self.pool.reset_stats();
        Ok(plan.execute(&self.catalog, &mut self.pool, self.path.clone())?)
    }

    /// Persist the catalog and flush every dirty frame.
    fn save(&mut self) -> Result<(), StorageError> {
        self.catalog.save(&mut self.pool)?;
        self.pool.flush_all()
    }
}
