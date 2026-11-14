//! Integration tests for Module 4: the query optimizer.

use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_dir(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("radb_opt_{name}_{stamp}"));
    fs::create_dir_all(&path).unwrap();
    path
}

fn storage_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_radb-storage"))
}

fn opt_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_radb-opt"))
}

#[test]
fn analyze_command_works() {
    let dir = temp_dir("analyze");
    let db = dir.to_str().unwrap();

    // Create table
    let out = storage_cmd()
        .args([
            "create-table",
            "--db",
            db,
            "--name",
            "R",
            "--schema",
            "a:INT,b:INT",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Load data
    let csv = dir.join("r.csv");
    fs::write(&csv, "1,10\n2,20\n3,30\n4,10\n").unwrap();
    let out = storage_cmd()
        .args([
            "load",
            "--db",
            db,
            "--table",
            "R",
            "--file",
            csv.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Analyze
    let out = opt_cmd()
        .args(["analyze", "--db", db, "--table", "R", "--buckets", "10"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn plan_command_produces_physical_plan() {
    let dir = temp_dir("plan");
    let db = dir.to_str().unwrap();

    // Create table
    let out = storage_cmd()
        .args([
            "create-table",
            "--db",
            db,
            "--name",
            "R",
            "--schema",
            "a:INT,b:INT",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Create logical plan
    let logical = dir.join("logical.json");
    fs::write(&logical, r#"{"op":"scan","table":"R"}"#).unwrap();

    // Generate physical plan
    let out = opt_cmd()
        .args([
            "plan",
            "--db",
            db,
            "--logical",
            logical.to_str().unwrap(),
            "--frames",
            "32",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Should produce valid JSON
    let _: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn explain_command_works() {
    let dir = temp_dir("explain");
    let db = dir.to_str().unwrap();

    // Create table
    let out = storage_cmd()
        .args([
            "create-table",
            "--db",
            db,
            "--name",
            "R",
            "--schema",
            "a:INT,b:INT",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    // Create logical plan
    let logical = dir.join("logical.json");
    fs::write(&logical, r#"{"op":"scan","table":"R"}"#).unwrap();

    // Explain
    let out = opt_cmd()
        .args([
            "explain",
            "--db",
            db,
            "--logical",
            logical.to_str().unwrap(),
            "--frames",
            "32",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    // explain command prints the query plan to stdout with an estimate per node
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("seqscan"),
        "expected seqscan in output: {stdout}"
    );
    assert!(
        stdout.contains("est_rows=") && stdout.contains("est_cost="),
        "every explain node carries rows and cost: {stdout}"
    );

    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn dp_table_command_works() {
    let dir = temp_dir("dptable");
    let db = dir.to_str().unwrap();

    // Create tables
    for name in &["A", "B"] {
        let out = storage_cmd()
            .args([
                "create-table",
                "--db",
                db,
                "--name",
                name,
                "--schema",
                "x:INT,y:INT",
            ])
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    // Create logical plan
    let logical = dir.join("logical.json");
    fs::write(&logical, r#"{"op":"scan","table":"A"}"#).unwrap();

    // DP table
    let out = opt_cmd()
        .args([
            "dp-table",
            "--db",
            db,
            "--logical",
            logical.to_str().unwrap(),
            "--frames",
            "32",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stdout).contains("subset"));

    let _ = fs::remove_dir_all(&dir);
}

/// A self-join keeps the two scans distinct by carrying each `rename` as a
/// physical scan alias.
#[test]
fn self_join_keeps_distinct_scan_aliases() {
    let dir = temp_dir("self_join");
    let db = dir.to_str().unwrap();

    let out = storage_cmd()
        .args([
            "create-table",
            "--db",
            db,
            "--name",
            "R",
            "--schema",
            "x:INT,y:INT",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());

    let logical = dir.join("logical.json");
    fs::write(
        &logical,
        r#"{"op":"join","condition":"e.y = m.x",
            "left":{"op":"rename","to":"e","child":{"op":"scan","table":"R"}},
            "right":{"op":"rename","to":"m","child":{"op":"scan","table":"R"}}}"#,
    )
    .unwrap();

    let out = opt_cmd()
        .args([
            "plan",
            "--db",
            db,
            "--logical",
            logical.to_str().unwrap(),
            "--frames",
            "32",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains(r#""alias":"e""#), "{text}");
    assert!(text.contains(r#""alias":"m""#), "{text}");

    let _ = fs::remove_dir_all(&dir);
}

/// A `project` above a join must survive conversion (regression: the DP result
/// used to replace the whole plan, dropping every operator above the scans).
#[test]
fn plan_preserves_a_projection_above_a_join() {
    let dir = temp_dir("project_over_join");
    let db = dir.to_str().unwrap();

    for name in &["A", "B"] {
        let out = storage_cmd()
            .args([
                "create-table",
                "--db",
                db,
                "--name",
                name,
                "--schema",
                "x:INT,y:INT",
            ])
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    let logical = dir.join("logical.json");
    fs::write(
        &logical,
        r#"{"op":"project","columns":["A.y"],
            "child":{"op":"join","condition":"A.x = B.x",
              "left":{"op":"scan","table":"A"},
              "right":{"op":"scan","table":"B"}}}"#,
    )
    .unwrap();

    let out = opt_cmd()
        .args([
            "plan",
            "--db",
            db,
            "--logical",
            logical.to_str().unwrap(),
            "--frames",
            "32",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let plan: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(plan["op"].as_str(), Some("project"));
    assert!(
        plan["child"]["op"]
            .as_str()
            .map(|op| op.ends_with("join"))
            .unwrap_or(false),
        "expected a join under the project: {plan}"
    );

    let _ = fs::remove_dir_all(&dir);
}
