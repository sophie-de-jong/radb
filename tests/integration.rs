//! End-to-end integration: SQL text through M3, M4, M2 and M1.
//!
//! These tests use the library facade (`Database`) so they exercise the same
//! path as the shell: parse/compile, optimize, execute, read pages. They focus
//! on names that exist only in the logical plan — select aliases, aggregate
//! aliases and a `FROM` subquery alias — because that is where the module
//! boundary is easiest to get wrong.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use radb::storage::Policy;
use radb::{Database, Schema, Value};

/// A unique scratch database directory.
fn temp_dir(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("radb_integration_{name}_{stamp}"));
    path
}

/// The two tables these tests use, with the fixture data from `tests/sql.rs`.
fn database(name: &str) -> (Database, PathBuf) {
    let dir = temp_dir(name);
    let mut db = Database::open_with(&dir, 16, Policy::Lru).unwrap();

    let emp: Schema = "eid:INT,name:VARCHAR(20),age:INT,did:INT,salary:INT,mgr:INT"
        .parse()
        .unwrap();
    db.create_table("Emp", emp).unwrap();
    for (eid, name, age, did, salary, mgr) in [
        (1, "alice", 30, 1, 100, 0),
        (2, "bob", 40, 2, 200, 1),
        (3, "amy", 25, 1, 300, 1),
        (4, "carol", 35, 2, 400, 2),
    ] {
        db.insert(
            "Emp",
            &[
                Value::Int(eid),
                Value::Varchar(name.to_string()),
                Value::Int(age),
                Value::Int(did),
                Value::Int(salary),
                Value::Int(mgr),
            ],
        )
        .unwrap();
    }

    let dept: Schema = "did:INT,title:VARCHAR(20),budget:INT,city:VARCHAR(20)"
        .parse()
        .unwrap();
    db.create_table("Dept", dept).unwrap();
    for (did, title, budget, city) in [(1, "eng", 1000, "NY"), (2, "sales", 2000, "LA")] {
        db.insert(
            "Dept",
            &[
                Value::Int(did),
                Value::Varchar(title.to_string()),
                Value::Int(budget),
                Value::Varchar(city.to_string()),
            ],
        )
        .unwrap();
    }

    (db, dir)
}

#[test]
fn aggregate_alias_and_having_run_through_every_module() {
    let (mut db, dir) = database("aggregate_alias");
    let rows = db
        .execute_sql(
            "SELECT did, COUNT(*) AS n FROM Emp \
             GROUP BY did HAVING COUNT(*) > 1 ORDER BY n DESC;",
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].as_ref(),
        &[Value::Int(2), Value::Int(2)][..],
        "first group"
    );
    assert_eq!(
        rows[1].as_ref(),
        &[Value::Int(1), Value::Int(2)][..],
        "second group"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn subquery_alias_joins_a_base_table() {
    let (mut db, dir) = database("subquery_alias");
    let rows = db
        .execute_sql(
            "SELECT t.n, d.title \
               FROM (SELECT did, COUNT(*) AS n FROM Emp GROUP BY did) AS t \
               JOIN Dept d ON t.did = d.did \
              ORDER BY t.n DESC, d.title;",
        )
        .unwrap();
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[0].as_ref(),
        &[Value::Int(2), Value::Varchar("eng".to_string())][..]
    );
    assert_eq!(
        rows[1].as_ref(),
        &[Value::Int(2), Value::Varchar("sales".to_string())][..]
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn self_join_keeps_both_aliases_distinct() {
    let (mut db, dir) = database("self_join");
    let rows = db
        .execute_sql("SELECT e.name, m.name FROM Emp e JOIN Emp m ON e.did = m.did;")
        .unwrap();
    assert_eq!(rows.len(), 8, "every pair in each two-row department");
    let _ = std::fs::remove_dir_all(dir);
}
