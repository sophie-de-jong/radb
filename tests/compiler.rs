//! Integration test for the SQL compiler.

use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use radb::{Compiler, Database, Row, Statement, Value};

fn temp_dir(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("radb_compiler_{name}_{stamp}"));
    fs::create_dir_all(&path).unwrap();
    path
}

/// A later statement sees a table an earlier one created, and the compiler
/// returns one checked statement at a time.
#[test]
fn streams_create_insert_and_select() {
    let dir = temp_dir("stream");
    let mut db = Database::open(&dir).unwrap();
    let rows = db
        .execute_sql(
            "CREATE TABLE T (a INT);\n\
             INSERT INTO T VALUES (1);\n\
             INSERT INTO T VALUES (2);\n\
             SELECT a FROM T;",
        )
        .unwrap();
    assert_eq!(
        rows,
        vec![
            Row::from(vec![Value::Int(1)]),
            Row::from(vec![Value::Int(2)]),
        ]
    );

    let mut compiler = Compiler::new("INSERT INTO T VALUES (3); SELECT a FROM T;").unwrap();
    let first = compiler.compile(db.catalog()).unwrap().unwrap();
    assert!(matches!(first, Statement::Insert { .. }));
    let second = compiler.compile(db.catalog()).unwrap().unwrap();
    assert!(matches!(second, Statement::Select(_)));
    assert!(compiler.compile(db.catalog()).unwrap().is_none());

    let _ = fs::remove_dir_all(&dir);
}

/// A script's later statements see a table created earlier in the same script,
/// both when compiling the script and when executing it.
#[test]
fn scripts_see_earlier_create_statements() {
    let dir = temp_dir("script");
    let mut db = Database::open(&dir).unwrap();
    let script = "CREATE TABLE X (a INT);\n\
                  INSERT INTO X VALUES (1);\n\
                  INSERT INTO X VALUES (2);\n\
                  SELECT a FROM X;";

    let mut compiler = Compiler::new(script).unwrap();
    let statements = compiler.compile_all(db.catalog()).unwrap();
    assert_eq!(statements.len(), 4);
    assert!(matches!(statements[0], Statement::Create { .. }));
    assert!(matches!(statements[1], Statement::Insert { .. }));
    assert!(matches!(statements[3], Statement::Select(_)));

    let rows = db.execute_sql(script).unwrap();
    assert_eq!(
        rows,
        vec![
            Row::from(vec![Value::Int(1)]),
            Row::from(vec![Value::Int(2)]),
        ]
    );

    let _ = fs::remove_dir_all(&dir);
}

/// `compile_create`, `compile_select`, and `compile_insert` check the
/// statement kind and return the compiled form.
#[test]
fn typed_compile_methods_check_the_statement_kind() {
    let dir = temp_dir("typed");
    let mut db = Database::open(&dir).unwrap();

    let mut create = Compiler::new("CREATE TABLE T (a INT);").unwrap();
    let (name, schema) = create.compile_create(db.catalog()).unwrap();
    assert_eq!(name, "T");
    assert_eq!(schema.columns().len(), 1);
    db.execute_sql("CREATE TABLE T (a INT);").unwrap();

    let mut insert = Compiler::new("INSERT INTO T VALUES (7);").unwrap();
    let (table, values) = insert.compile_insert(db.catalog()).unwrap();
    assert_eq!(table, "T");
    assert_eq!(values, vec![Value::Int(7)]);
    db.execute_sql("INSERT INTO T VALUES (7);").unwrap();

    let mut select = Compiler::new("SELECT a FROM T;").unwrap();
    let plan = select.compile_select(db.catalog()).unwrap();
    assert!(matches!(plan, radb::sql::plan::Plan::Project { .. }));
    drop(select);

    let mut wrong = Compiler::new("SELECT a FROM T;").unwrap();
    assert!(wrong.compile_create(db.catalog()).is_err());

    let _ = fs::remove_dir_all(&dir);
}
