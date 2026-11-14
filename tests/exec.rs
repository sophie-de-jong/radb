use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use radb::exec::Plan;
use radb::storage::{BufferPool, Catalog, Policy};

fn temp_dir(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .subsec_nanos();
    p.push(format!("radb_{name}_{ts}"));
    fs::create_dir_all(&p).unwrap();
    p
}

fn exec_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_radb-exec"))
}

fn storage_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_radb-storage"))
}

#[test]
fn basic_seqscan() {
    let dir = temp_dir("exec_seq");
    let db = dir.clone();
    let db_str = db.to_str().unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "create-table",
        "--db",
        db_str,
        "--name",
        "R",
        "--schema",
        "a:INT,b:VARCHAR(10)",
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "create: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let csv = dir.join("r.csv");
    fs::write(&csv, "1,foo\n2,bar\n").unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "load",
        "--db",
        db_str,
        "--table",
        "R",
        "--file",
        csv.to_str().unwrap(),
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "load: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let plan = dir.join("plan.json");
    fs::write(&plan, r#"{"op":"seqscan","table":"R"}"#).unwrap();
    let mut cmd = exec_cmd();
    cmd.args([
        "run",
        "--db",
        db_str,
        "--plan",
        plan.to_str().unwrap(),
        "--frames",
        "8",
        "--json",
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "run: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn project_filter() {
    let dir = temp_dir("exec_pf");
    let db = dir.clone();
    let db_str = db.to_str().unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "create-table",
        "--db",
        db_str,
        "--name",
        "R",
        "--schema",
        "a:INT,b:VARCHAR(10),c:INT",
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "create: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let csv = dir.join("r.csv");
    fs::write(&csv, "1,foo,10\n2,bar,20\n3,baz,30\n").unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "load",
        "--db",
        db_str,
        "--table",
        "R",
        "--file",
        csv.to_str().unwrap(),
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "load: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let plan = r#"{"op":"project","columns":["R.a","R.c"],"child":{"op":"filter","condition":"R.a > 1","child":{"op":"seqscan","table":"R"}}}"#;
    let plan_file = dir.join("plan.json");
    fs::write(&plan_file, plan).unwrap();
    let mut cmd = exec_cmd();
    cmd.args([
        "run",
        "--db",
        db_str,
        "--plan",
        plan_file.to_str().unwrap(),
        "--frames",
        "4",
        "--json",
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "run: {} stderr: {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn trace_format() {
    let dir = temp_dir("exec_trace");
    let db = dir.clone();
    let db_str = db.to_str().unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "create-table",
        "--db",
        db_str,
        "--name",
        "R",
        "--schema",
        "a:INT",
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let csv = dir.join("r.csv");
    fs::write(&csv, "1\n2\n").unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "load",
        "--db",
        db_str,
        "--table",
        "R",
        "--file",
        csv.to_str().unwrap(),
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let plan_file = dir.join("plan.json");
    fs::write(&plan_file, r#"{"op":"seqscan","table":"R"}"#).unwrap();
    let mut cmd = exec_cmd();
    cmd.args([
        "trace",
        "--db",
        db_str,
        "--plan",
        plan_file.to_str().unwrap(),
        "--frames",
        "8",
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "trace: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("seqscan R"),
        "trace missing label: {}",
        stdout
    );
}

#[test]
fn sort_small() {
    let dir = temp_dir("exec_sort");
    let db = dir.clone();
    let db_str = db.to_str().unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "create-table",
        "--db",
        db_str,
        "--name",
        "R",
        "--schema",
        "a:INT,b:VARCHAR(10)",
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let csv = dir.join("r.csv");
    fs::write(&csv, "3,foo\n1,bar\n2,baz\n").unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "load",
        "--db",
        db_str,
        "--table",
        "R",
        "--file",
        csv.to_str().unwrap(),
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let plan = r#"{"op":"sort","keys":[["R.a","asc"]],"child":{"op":"seqscan","table":"R"}}"#;
    let plan_file = dir.join("plan.json");
    fs::write(&plan_file, plan).unwrap();
    let mut cmd = exec_cmd();
    cmd.args([
        "run",
        "--db",
        db_str,
        "--plan",
        plan_file.to_str().unwrap(),
        "--frames",
        "4",
        "--json",
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn distinct_sort() {
    let dir = temp_dir("exec_dist");
    let db = dir.clone();
    let db_str = db.to_str().unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "create-table",
        "--db",
        db_str,
        "--name",
        "R",
        "--schema",
        "a:INT",
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let csv = dir.join("r.csv");
    fs::write(&csv, "1\n2\n1\n").unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "load",
        "--db",
        db_str,
        "--table",
        "R",
        "--file",
        csv.to_str().unwrap(),
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let plan_file = dir.join("plan.json");
    fs::write(
        &plan_file,
        r#"{"op":"distinct","mode":"sort","child":{"op":"seqscan","table":"R"}}"#,
    )
    .unwrap();
    let mut cmd = exec_cmd();
    cmd.args([
        "run",
        "--db",
        db_str,
        "--plan",
        plan_file.to_str().unwrap(),
        "--frames",
        "4",
        "--json",
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn bnl_ba() {
    let dir = temp_dir("exec_bnl");
    let db = dir.clone();
    let db_str = db.to_str().unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "create-table",
        "--db",
        db_str,
        "--name",
        "R",
        "--schema",
        "a:INT,b:VARCHAR(5)",
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let mut cmd = storage_cmd();
    cmd.args([
        "create-table",
        "--db",
        db_str,
        "--name",
        "S",
        "--schema",
        "b:VARCHAR(5),c:INT",
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    fs::write(dir.join("r.csv"), "1,aa\n2,aa\n").unwrap();
    fs::write(dir.join("s.csv"), "aa,10\naa,20\n").unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "load",
        "--db",
        db_str,
        "--table",
        "R",
        "--file",
        dir.join("r.csv").to_str().unwrap(),
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let mut cmd = storage_cmd();
    cmd.args([
        "load",
        "--db",
        db_str,
        "--table",
        "S",
        "--file",
        dir.join("s.csv").to_str().unwrap(),
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let plan_file = dir.join("plan.json");
    fs::write(&plan_file, r#"{"op":"nested_loop_join","condition":"R.b = S.b","left":{"op":"seqscan","table":"R"},"right":{"op":"seqscan","table":"S"}}"#).unwrap();
    let mut cmd = exec_cmd();
    cmd.args([
        "run",
        "--db",
        db_str,
        "--plan",
        plan_file.to_str().unwrap(),
        "--frames",
        "4",
        "--json",
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("rows"), "{}", stdout);
}

#[test]
fn union_all() {
    let dir = temp_dir("exec_unionall");
    let db = dir.clone();
    let db_str = db.to_str().unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "create-table",
        "--db",
        db_str,
        "--name",
        "R",
        "--schema",
        "a:INT",
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let mut cmd = storage_cmd();
    cmd.args([
        "create-table",
        "--db",
        db_str,
        "--name",
        "S",
        "--schema",
        "a:INT",
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    fs::write(dir.join("r.csv"), "1\n2\n").unwrap();
    fs::write(dir.join("s.csv"), "2\n3\n").unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "load",
        "--db",
        db_str,
        "--table",
        "R",
        "--file",
        dir.join("r.csv").to_str().unwrap(),
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let mut cmd = storage_cmd();
    cmd.args([
        "load",
        "--db",
        db_str,
        "--table",
        "S",
        "--file",
        dir.join("s.csv").to_str().unwrap(),
    ]);
    let out = cmd.output().unwrap();
    assert!(out.status.success());
    let plan_file = dir.join("plan.json");
    fs::write(&plan_file, r#"{"op":"union_all","left":{"op":"seqscan","table":"R"},"right":{"op":"seqscan","table":"S"}}"#).unwrap();
    let mut cmd = exec_cmd();
    cmd.args([
        "run",
        "--db",
        db_str,
        "--plan",
        plan_file.to_str().unwrap(),
        "--frames",
        "4",
        "--json",
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "run: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

/// §1.1: a finished plan leaves no page pinned. `intersect` used to open each
/// child before sorting it, so the child's first directory page was pinned twice
/// and never fully released.
#[test]
fn intersect_finishes_with_no_pages_pinned() {
    let dir = temp_dir("exec_intersect_pins");
    let db = dir.clone();
    let db_str = db.to_str().unwrap();

    let mut cmd = storage_cmd();
    cmd.args([
        "create-table",
        "--db",
        db_str,
        "--name",
        "R",
        "--schema",
        "a:INT,b:VARCHAR(10)",
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "create: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let csv = dir.join("r.csv");
    fs::write(&csv, "1,foo\n2,bar\n1,foo\n").unwrap();
    let mut cmd = storage_cmd();
    cmd.args([
        "load",
        "--db",
        db_str,
        "--table",
        "R",
        "--file",
        csv.to_str().unwrap(),
    ]);
    let out = cmd.output().unwrap();
    assert!(
        out.status.success(),
        "load: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let plan: Plan = r#"{"op":"intersect","left":{"op":"seqscan","table":"R"},"right":{"op":"seqscan","table":"R"}}"#
        .parse()
        .unwrap();

    let mut pool = BufferPool::new(4, Policy::Lru);
    let catalog = Catalog::open(&mut pool, db.join("catalog.db")).unwrap();
    {
        let mut exec = plan.execute(&catalog, &mut pool, db.clone()).unwrap();
        let rows: Vec<_> = exec.by_ref().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(rows.len(), 2, "distinct tuples present on both sides");
    }
    assert_eq!(
        pool.pinned(),
        0,
        "a finished plan must leave no page pinned"
    );
}
