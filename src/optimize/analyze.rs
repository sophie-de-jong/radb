//! Table statistics for `ANALYZE` (M4 §1).
//!
//! The statistics are exact and reproducible — no sampling — and the heap is
//! scanned exactly once. That scan copies the live rows to one temporary run;
//! each column is then sorted with Module 2's external sort and folded into
//! `ndv`, `min`, `max` and an equi-depth histogram in one streaming pass. No
//! column is ever held in memory.

use std::error::Error;
use std::path::Path;

use crate::exec::Context;
use crate::exec::external_sort::ExternalSort;
use crate::exec::order::SortOrder;
use crate::exec::store::RunWriter;
use crate::storage::stats::{Bucket, ColumnStats, Stats, TableStats};
use crate::storage::{BufferPool, HeapFile, TableMeta};
use crate::value::{Row, Value};

/// The default number of histogram buckets (`--buckets`), M4 §1.
pub const DEFAULT_BUCKETS: usize = 20;

/// Compute the statistics of `meta`'s table, reading its rows through `pool`.
pub fn analyze_table(
    pool: &mut BufferPool,
    db_dir: &Path,
    meta: &TableMeta,
    buckets: usize,
) -> Result<Stats, Box<dyn Error>> {
    let file = pool.open_file(db_dir.join(&meta.file_name))?;
    let heap = HeapFile::new(file, &meta.name, meta.schema.clone());
    let mut ctx = Context::new(pool, db_dir.to_path_buf());

    // One heap scan produces one temporary run of full rows. This is the only
    // pass over the table; every later pass is over that run.
    let mut writer = RunWriter::new(ctx.temp_dir(), meta.schema.column_types())?;
    let mut cursor = heap.scan(ctx.pool)?;
    let mut n_tuples = 0u64;
    let mut width_sum = 0u64;
    while let Some((_rid, values)) = cursor.next(ctx.pool, &meta.schema)? {
        for value in &values {
            width_sum += match value {
                Value::Int(_) => 4,
                Value::Varchar(s) => s.len() as u64 + 2,
            };
        }
        writer.push(&Row::from(values))?;
        n_tuples += 1;
    }
    cursor.close(ctx.pool);
    let (mut rows, _writes) = writer.finish()?;

    let width = u32::try_from(width_sum.checked_div(n_tuples).unwrap_or(0)).unwrap_or(0);
    let n_pages = heap.num_data_pages(ctx.pool)? as u64;
    let buckets = buckets.clamp(1, 256);

    let mut columns = Vec::with_capacity(meta.schema.arity());
    for (index, _column) in meta.schema.columns().iter().enumerate() {
        rows.rewind();
        let mut sort = ExternalSort::by(&meta.schema, SortOrder::ascending(vec![index]));
        sort.sort_with(&mut |_ctx| rows.next(), &mut ctx)?;
        columns.push(fold_column(&mut sort, index, n_tuples, buckets)?);
        sort.close(&mut ctx);
    }

    Ok(Stats {
        table: TableStats {
            n_tuples,
            n_pages,
            width,
        },
        columns,
    })
}

/// Fold one sorted column into its exact statistics.
fn fold_column(
    sort: &mut ExternalSort,
    index: usize,
    n_tuples: u64,
    k: usize,
) -> Result<ColumnStats, Box<dyn Error>> {
    let mut min: Option<Value> = None;
    let mut max: Option<Value> = None;
    let mut ndv = 0u64;
    let mut prev: Option<Value> = None;
    let mut buckets = vec![Bucket::default(); k];
    let mut bucket_prev: Vec<Option<Value>> = vec![None; k];
    let mut rank = 0u64;

    while let Some(row) = sort.next()? {
        let Some(value) = row.get(index).cloned() else {
            continue;
        };
        if min.as_ref().is_none_or(|min| &value < min) {
            min = Some(value.clone());
        }
        if max.as_ref().is_none_or(|max| &value > max) {
            max = Some(value.clone());
        }
        if prev.as_ref() != Some(&value) {
            ndv += 1;
            prev = Some(value.clone());
        }
        // Equi-depth: rank `i` belongs in bucket `i * k / n`.
        let bucket = if n_tuples == 0 {
            0
        } else {
            ((rank as u128 * k as u128) / n_tuples as u128) as usize
        }
        .min(k - 1);
        if bucket_prev[bucket].as_ref() != Some(&value) {
            buckets[bucket].ndv += 1;
            bucket_prev[bucket] = Some(value.clone());
        }
        if buckets[bucket].count == 0 {
            buckets[bucket].min = value.clone();
        }
        buckets[bucket].max = value.clone();
        buckets[bucket].count += 1;
        rank += 1;
    }

    if ndv == 0 {
        return Ok(ColumnStats::default());
    }
    Ok(ColumnStats {
        ndv,
        min: min.unwrap_or_default(),
        max: max.unwrap_or_default(),
        buckets: buckets
            .into_iter()
            .filter(|bucket| bucket.count > 0)
            .collect(),
    })
}
