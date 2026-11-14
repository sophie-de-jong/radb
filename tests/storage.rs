//! Module 1 integration tests.
//!
//! These drive the public storage API and the real `radb-storage` binary. They
//! cover
//! the checklist in §10 of `docs/M1/M1_INSTRUCTIONS.txt`: the §3.2 page example, the
//! §5.3 policy example, a scan that never pins more than two pages, deterministic
//! counters, the pinned-eviction error, and an empty scan after deleting every
//! record.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use radb::Schema;
use radb::Value;
use radb::storage::buffer::{BufferPool, Policy};
use radb::storage::catalog::{Catalog, TableMeta};
use radb::storage::heap::HeapFile;
use radb::storage::slotted;
use radb::storage::{Bucket, ColumnStats, Page, PageId, RecordId, Stats, StorageError, TableStats};

/// A fresh, empty directory unique to this test.
fn temp_dir(tag: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("radb_m1_it_{}_{}", std::process::id(), tag));
    let _ = fs::remove_dir_all(&p);
    fs::create_dir_all(&p).unwrap();
    p
}

/// A schema of `n` `VARCHAR(255)` columns: each record is 257 bytes per column,
/// so fifteen columns make a record that fills a page by itself.
fn wide_schema(n: usize) -> Schema {
    let spec = (0..n)
        .map(|i| format!("c{i}:VARCHAR(255)"))
        .collect::<Vec<_>>()
        .join(",");
    spec.parse().unwrap()
}

fn wide_row(n: usize) -> Vec<Value> {
    (0..n)
        .map(|i| Value::Varchar(format!("{:0width$}", i, width = 255)))
        .collect()
}

/// §10: a scan of a table twenty times larger than B completes and never pins
/// more than two pages.
#[test]
fn scan_pins_at_most_two_pages() {
    let dir = temp_dir("pin2");
    let mut pool = BufferPool::new(8, Policy::Lru);
    let schema = wide_schema(15); // one record per page
    let row = wide_row(15);
    let mut catalog = Catalog::open(&mut pool, dir.join("catalog.db")).unwrap();
    let file = pool.open_file(dir.join("W.db")).unwrap();
    let heap = HeapFile::new(file, "W", schema.clone());
    heap.create(&mut pool).unwrap();
    catalog.insert(TableMeta::new("W", schema.clone())).unwrap();
    catalog.save(&mut pool).unwrap();
    pool.flush_all().unwrap();

    // 200 rows, each filling a page: ~201 pages, well past 20 * B = 160.
    for _ in 0..200 {
        heap.insert(&mut pool, &row).unwrap();
    }
    assert!(heap.num_pages(&pool) > 160, "table is not 20x B");

    // Counters from the pool as a whole are irrelevant; flush, reset, scan.
    pool.flush_all().unwrap();
    pool.reset();
    pool.reset_stats();
    let mut scan = heap.scan(&mut pool).unwrap();
    let mut rows = 0usize;
    while let Some((_rid, _values)) = scan.next(&mut pool, &schema).unwrap() {
        rows += 1;
    }
    scan.close(&mut pool);
    assert_eq!(rows, 200);
    assert!(
        pool.max_pinned() <= 2,
        "scan pinned {} pages, max is 2",
        pool.max_pinned()
    );
    let _ = fs::remove_dir_all(&dir);
}

/// §10: deleting every record and scanning returns nothing, without error.
#[test]
fn empty_after_deleting_every_record() {
    let dir = temp_dir("delete_all");
    let mut pool = BufferPool::new(4, Policy::Lru);
    let schema: Schema = "a:INT,b:VARCHAR(10)".parse().unwrap();
    let file = pool.open_file(dir.join("D.db")).unwrap();
    let heap = HeapFile::new(file, "D", schema.clone());
    heap.create(&mut pool).unwrap();
    for i in 0..25i32 {
        heap.insert(&mut pool, &[Value::Int(i), Value::Varchar(format!("v{i}"))])
            .unwrap();
    }
    pool.flush_all().unwrap();

    // Collect every rid, then delete each.
    let rids: Vec<RecordId> = {
        let mut scan = heap.scan(&mut pool).unwrap();
        let mut rids = Vec::new();
        while let Some((rid, _values)) = scan.next(&mut pool, &schema).unwrap() {
            rids.push(rid);
        }
        scan.close(&mut pool);
        rids
    };
    assert_eq!(rids.len(), 25);
    for rid in rids {
        heap.delete(&mut pool, rid).unwrap();
    }
    pool.flush_all().unwrap();

    let mut scan = heap.scan(&mut pool).unwrap();
    assert!(scan.next(&mut pool, &schema).unwrap().is_none());
    scan.close(&mut pool);
    let _ = fs::remove_dir_all(&dir);
}

/// §9: evicting a pinned page never happens; the fetch fails instead.
#[test]
fn evicting_a_pinned_page_fails() {
    let mut pool = BufferPool::new(1, Policy::Lru);
    let f = pool.open_null();
    pool.fetch_page(f, PageId(0)).unwrap(); // page 0 is now pinned
    match pool.fetch_page(f, PageId(1)) {
        Err(StorageError::NoUnpinnedFrame) => {}
        other => panic!("expected NoUnpinnedFrame, got {other:?}"),
    }
}

/// §9: unpinning a page at pin count 0 is an error.
#[test]
fn unpinning_at_pin_count_zero_fails() {
    let mut pool = BufferPool::new(2, Policy::Lru);
    let f = pool.open_null();
    pool.fetch_page(f, PageId(7)).unwrap();
    pool.unpin(f, PageId(7), false).unwrap();
    match pool.unpin(f, PageId(7), false) {
        Err(StorageError::UnpinZeroPin {
            file: _,
            page: PageId(7),
        }) => {}
        other => panic!("expected UnpinZeroPin, got {other:?}"),
    }
}

/// §9: reading a dead slot is an error naming the rid.
#[test]
fn reading_a_dead_slot_names_the_rid() {
    let mut page = Box::new(Page::zeroed());
    slotted::init(&mut page, PageId(42));
    slotted::insert(&mut page, &[1, 2, 3]).unwrap();
    slotted::delete(&mut page, 0).unwrap();
    match slotted::get(&page, 0) {
        Err(StorageError::DeadSlot {
            page: PageId(42),
            slot: 0,
        }) => {}
        other => panic!("expected DeadSlot, got {other:?}"),
    }
}

/// §3.3: `floor(4088 / (L + 4))` fixed-length records per page.
#[test]
fn capacity_formula_matches_a_measured_page() {
    // Three INT columns: L = 12, capacity = 4088 / 16 = 255 (floor).
    let formula = 4088 / (12 + 4);
    assert_eq!(formula, 255);

    let dir = temp_dir("capacity");
    let mut pool = BufferPool::new(4, Policy::Lru);
    let schema: Schema = "a:INT,b:INT,c:INT".parse().unwrap();
    let file = pool.open_file(dir.join("C.db")).unwrap();
    let heap = HeapFile::new(file, "C", schema.clone());
    heap.create(&mut pool).unwrap();
    for i in 0..formula {
        heap.insert(&mut pool, &[Value::Int(i), Value::Int(i), Value::Int(i)])
            .unwrap();
    }
    // Filling a page exactly should not have spilled onto a second data page.
    assert_eq!(
        heap.num_pages(&pool),
        2,
        "one directory page + one data page"
    );
    pool.fetch_page(file, PageId(1)).unwrap();
    assert_eq!(
        slotted::slot_count(pool.page(file, PageId(1))),
        formula as u16
    );
    // 255 records consume 255 * 16 = 4080 of the 4088 bytes; 8 are unusable.
    assert_eq!(slotted::free_space(pool.page(file, PageId(1))), 8);
    pool.unpin(file, PageId(1), false).unwrap();
    let _ = fs::remove_dir_all(&dir);
}

/// §8: scanning a 500-page table reads about 500 pages at every B.
#[test]
fn scan_of_500_pages_reads_about_500_pages() {
    let dir = temp_dir("scan_cost");
    let schema = wide_schema(15); // one record per page
    let row = wide_row(15);

    // Build the table once with a generous pool.
    let mut build = BufferPool::new(16, Policy::Lru);
    let mut catalog = Catalog::open(&mut build, dir.join("catalog.db")).unwrap();
    let file = build.open_file(dir.join("Big.db")).unwrap();
    let heap = HeapFile::new(file, "Big", schema.clone());
    heap.create(&mut build).unwrap();
    catalog
        .insert(TableMeta::new("Big", schema.clone()))
        .unwrap();
    catalog.save(&mut build).unwrap();
    // 499 rows -> 499 data pages + 1 directory page = 500 pages.
    for _ in 0..499 {
        heap.insert(&mut build, &row).unwrap();
    }
    build.flush_all().unwrap();
    assert_eq!(heap.num_pages(&build), 500);

    for b in [2usize, 8, 64] {
        let mut pool = BufferPool::new(b, Policy::Lru);
        let file = pool.open_file(dir.join("Big.db")).unwrap();
        let heap = HeapFile::new(file, "Big", schema.clone());
        let mut scan = heap.scan(&mut pool).unwrap();
        let mut rows = 0;
        while let Some((_rid, _values)) = scan.next(&mut pool, &schema).unwrap() {
            rows += 1;
        }
        scan.close(&mut pool);
        assert_eq!(rows, 499);
        let s = pool.stats();
        assert_eq!(s.reads, 500, "B={b}: every page read exactly once");
        assert_eq!(s.misses, 500, "B={b}");
        assert!(pool.max_pinned() <= 2, "B={b} pinned too many");
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The catalog survives a pool restart, and reopening the same path is
/// idempotent.
#[test]
fn catalog_round_trips_through_the_pool() {
    let dir = temp_dir("catalog");
    let schema: Schema = "a:INT,b:VARCHAR(20),c:INT".parse().unwrap();
    let stats = Stats {
        table: TableStats {
            n_tuples: 7,
            n_pages: 3,
            width: 13,
        },
        columns: vec![ColumnStats {
            ndv: 4,
            min: radb::Value::Int(1),
            max: radb::Value::Int(4),
            buckets: vec![Bucket {
                min: radb::Value::Int(1),
                max: radb::Value::Int(4),
                count: 7,
                ndv: 4,
            }],
        }],
    };
    {
        let mut pool = BufferPool::new(4, Policy::Lru);
        let mut catalog = Catalog::open(&mut pool, dir.join("catalog.db")).unwrap();
        let mut meta = TableMeta::new("R", schema.clone());
        meta.num_pages = 3;
        meta.num_tuples = 7;
        meta.stats = Some(stats.clone());
        catalog.insert(meta).unwrap();
        catalog.save(&mut pool).unwrap();
        pool.flush_all().unwrap();
    }
    {
        let mut pool = BufferPool::new(4, Policy::Clock);
        let catalog = Catalog::open(&mut pool, dir.join("catalog.db")).unwrap();
        let meta = catalog.require("R").unwrap();
        assert_eq!(meta.schema, schema);
        assert_eq!(meta.num_pages, 3);
        assert_eq!(meta.num_tuples, 7);
        assert_eq!(meta.file_name.as_path(), "R.db");
        assert_eq!(meta.stats, Some(stats));
    }
    let _ = fs::remove_dir_all(&dir);
}

/// An empty catalog still writes and reads a valid page.
#[test]
fn empty_catalog_round_trips() {
    let dir = temp_dir("catalog_empty");
    {
        let mut pool = BufferPool::new(2, Policy::Lru);
        let catalog = Catalog::open(&mut pool, dir.join("catalog.db")).unwrap();
        catalog.save(&mut pool).unwrap();
        pool.flush_all().unwrap();
    }
    {
        let mut pool = BufferPool::new(2, Policy::Lru);
        let catalog = Catalog::open(&mut pool, dir.join("catalog.db")).unwrap();
        assert!(catalog.tables().is_empty());
    }
    let _ = fs::remove_dir_all(&dir);
}

/// A catalog with enough tables spans more than one page and still round-trips.
#[test]
fn catalog_spans_pages_for_many_tables() {
    let dir = temp_dir("catalog_pages");
    let schema: Schema = "a:INT".parse().unwrap();
    {
        let mut pool = BufferPool::new(8, Policy::Lru);
        let mut catalog = Catalog::open(&mut pool, dir.join("catalog.db")).unwrap();
        for i in 0..200 {
            catalog
                .insert(TableMeta::new(format!("T{i}"), schema.clone()))
                .unwrap();
        }
        catalog.save(&mut pool).unwrap();
        pool.flush_all().unwrap();
        assert!(
            pool.num_pages(catalog.file) > 1,
            "the catalog should span pages"
        );
    }
    {
        let mut pool = BufferPool::new(8, Policy::Lru);
        let catalog = Catalog::open(&mut pool, dir.join("catalog.db")).unwrap();
        assert_eq!(catalog.tables().len(), 200);
        assert_eq!(catalog.require("T199").unwrap().schema, schema);
    }
    let _ = fs::remove_dir_all(&dir);
}

/// The heap grows a second directory page once the first holds 681 entries,
/// and a scan still reaches every record.
#[test]
fn directory_pages_chain_past_capacity() {
    let dir = temp_dir("dirchain");
    let schema = wide_schema(15); // one record per page
    let row = wide_row(15);
    let mut pool = BufferPool::new(16, Policy::Lru);
    let file = pool.open_file(dir.join("Chain.db")).unwrap();
    let heap = HeapFile::new(file, "Chain", schema.clone());
    heap.create(&mut pool).unwrap();

    for _ in 0..690 {
        heap.insert(&mut pool, &row).unwrap();
    }
    pool.flush_all().unwrap();

    // One first directory page + 690 data pages + one second directory page.
    assert_eq!(heap.num_pages(&pool), 692);

    let mut scan = heap.scan(&mut pool).unwrap();
    let mut rows = 0;
    while let Some((_rid, _values)) = scan.next(&mut pool, &schema).unwrap() {
        rows += 1;
    }
    scan.close(&mut pool);
    assert_eq!(rows, 690, "the second directory page must be reached");
    let _ = fs::remove_dir_all(&dir);
}

// ---------------------------------------------------------------------------
// End-to-end tests of the real binary.
// ---------------------------------------------------------------------------

fn storage() -> Command {
    Command::new(env!("CARGO_BIN_EXE_radb-storage"))
}

fn run(args: &[&str]) -> std::process::Output {
    let mut cmd = storage();
    cmd.args(args);
    cmd.output().expect("failed to run storage")
}

#[test]
fn cli_create_load_scan_layout() {
    let dir = temp_dir("cli_basic");
    let db = dir.to_str().unwrap();

    let out = run(&[
        "create-table",
        "--db",
        db,
        "--name",
        "R",
        "--schema",
        "a:INT,b:VARCHAR(20),c:INT",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let csv = dir.join("r.csv");
    fs::write(&csv, "42,bob,7\n43,amy,9\n1,\"hello, world\",2\n").unwrap();
    let out = run(&[
        "load",
        "--db",
        db,
        "--table",
        "R",
        "--file",
        csv.to_str().unwrap(),
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let out = run(&[
        "scan", "--db", db, "--table", "R", "--frames", "8", "--json",
    ]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json["table"], "R");
    assert_eq!(
        json["schema"],
        serde_json::json!([["a", "INT"], ["b", "VARCHAR", 20], ["c", "INT"]])
    );
    assert_eq!(
        json["rows"],
        serde_json::json!([[42, "bob", 7], [43, "amy", 9], [1, "hello, world", 2]])
    );
    // The catalog page read, then the directory page and one data page.
    assert_eq!(json["stats"]["misses"], 3);
    assert_eq!(json["stats"]["reads"], 3);
    assert_eq!(json["stats"]["evictions"], 0);

    let out = run(&["layout", "--db", db, "--table", "R", "--page", "1"]);
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("page_id     = 1"), "{text}");
    assert!(text.contains("slot directory (3 slots)"), "{text}");

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_scan_is_deterministic() {
    let dir = temp_dir("cli_det");
    let db = dir.to_str().unwrap();
    run(&[
        "create-table",
        "--db",
        db,
        "--name",
        "R",
        "--schema",
        "a:INT",
    ]);
    let csv = dir.join("r.csv");
    let body: String = (0..100).map(|i| format!("{i}\n")).collect();
    fs::write(&csv, body).unwrap();
    run(&[
        "load",
        "--db",
        db,
        "--table",
        "R",
        "--file",
        csv.to_str().unwrap(),
    ]);

    let a = run(&[
        "scan", "--db", db, "--table", "R", "--frames", "8", "--json",
    ]);
    let b = run(&[
        "scan", "--db", db, "--table", "R", "--frames", "8", "--json",
    ]);
    assert_eq!(a.stdout, b.stdout);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_trace_reproduces_the_worked_example() {
    let dir = temp_dir("cli_trace");
    let db = dir.to_str().unwrap();
    let trace = dir.join("t.txt");
    fs::write(&trace, "1\n2\n3\n1\n4\n2\n").unwrap();

    let lru = run(&[
        "trace",
        "--db",
        db,
        "--file",
        trace.to_str().unwrap(),
        "--frames",
        "3",
        "--policy",
        "lru",
        "--json",
    ]);
    let json: serde_json::Value = serde_json::from_slice(&lru.stdout).unwrap();
    assert_eq!(
        (
            json["hits"].as_u64(),
            json["misses"].as_u64(),
            json["evictions"].as_u64()
        ),
        (Some(1), Some(5), Some(2))
    );

    let clock = run(&[
        "trace",
        "--db",
        db,
        "--file",
        trace.to_str().unwrap(),
        "--frames",
        "3",
        "--policy",
        "clock",
        "--json",
    ]);
    let json: serde_json::Value = serde_json::from_slice(&clock.stdout).unwrap();
    assert_eq!(
        (
            json["hits"].as_u64(),
            json["misses"].as_u64(),
            json["evictions"].as_u64()
        ),
        (Some(2), Some(4), Some(1))
    );
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_bad_csv_names_the_line() {
    let dir = temp_dir("cli_badcsv");
    let db = dir.to_str().unwrap();
    run(&[
        "create-table",
        "--db",
        db,
        "--name",
        "T",
        "--schema",
        "a:INT,b:VARCHAR(3)",
    ]);
    let csv = dir.join("bad.csv");
    fs::write(&csv, "1,ok\n2,abcd\n").unwrap();
    let out = run(&[
        "load",
        "--db",
        db,
        "--table",
        "T",
        "--file",
        csv.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("line 2"), "{err}");
    assert!(err.contains("column 2"), "{err}");
    assert!(err.contains('b'), "{err}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_record_too_long_names_table_and_row() {
    let dir = temp_dir("cli_toolong");
    let db = dir.to_str().unwrap();
    let schema = (0..16)
        .map(|i| format!("c{i}:VARCHAR(255)"))
        .collect::<Vec<_>>()
        .join(",");
    run(&[
        "create-table",
        "--db",
        db,
        "--name",
        "X",
        "--schema",
        &schema,
    ]);
    let csv = dir.join("big.csv");
    let field = "a".repeat(255);
    let line = (0..16)
        .map(|_| field.as_str())
        .collect::<Vec<_>>()
        .join(",");
    fs::write(&csv, format!("{line}\n")).unwrap();
    let out = run(&[
        "load",
        "--db",
        db,
        "--table",
        "X",
        "--file",
        csv.to_str().unwrap(),
    ]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("table 'X'"), "{err}");
    assert!(err.contains("row 1"), "{err}");
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn cli_unknown_table_and_duplicate_are_errors() {
    let dir = temp_dir("cli_errs");
    let db = dir.to_str().unwrap();
    let dup = run(&[
        "create-table",
        "--db",
        db,
        "--name",
        "R",
        "--schema",
        "a:INT",
    ]);
    assert!(dup.status.success());
    let dup = run(&[
        "create-table",
        "--db",
        db,
        "--name",
        "R",
        "--schema",
        "a:INT",
    ]);
    assert!(!dup.status.success());

    let missing = run(&[
        "scan", "--db", db, "--table", "Nope", "--frames", "8", "--json",
    ]);
    assert!(!missing.status.success());
    let err = String::from_utf8_lossy(&missing.stderr);
    assert!(err.contains("unknown table 'Nope'"), "{err}");
    let _ = fs::remove_dir_all(&dir);
}
