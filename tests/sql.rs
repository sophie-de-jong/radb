//! Integration tests for Module 3: the SQL front end through `radb-sql` (§7).
//!
//! Every test builds a throwaway catalog with the tables the required cases
//! need, writes a `.sql` file, and drives the `radb-sql` binary exactly as the
//! grader will: `compile --json` for a logical plan and `check` for the first
//! error and its exit code. Plans are compared structurally with
//! [`serde_json::Value`], so key order and whitespace never matter — only the
//! operators, their nesting and their field values.
//!
//! The tests are named after the fifteen required cases of §7, and the file
//! ends with the worked examples of §5 and a handful of cross-cutting checks
//! (bag semantics, name qualification, the error format).

use std::fs;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

/// A unique scratch directory for one test.
fn temp_dir(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("radb_sql_{name}_{stamp}"));
    fs::create_dir_all(&path).unwrap();
    path
}

/// The Module 1 tool, used only to build the catalog.
fn storage_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_radb-storage"))
}

/// The Module 3 tool under test.
fn sql_cmd() -> Command {
    Command::new(env!("CARGO_BIN_EXE_radb-sql"))
}

/// The tables every fixture creates, and their schemas.
///
/// The column names are deliberately shared across tables — `did` is in all
/// four, `name` in two — so that cases 2, 3 and 12 exercise name resolution
/// rather than a made-up collision.
const TABLES: &[(&str, &str)] = &[
    (
        "Emp",
        "eid:INT,name:VARCHAR(20),age:INT,did:INT,salary:INT,mgr:INT",
    ),
    (
        "Dept",
        "did:INT,title:VARCHAR(20),budget:INT,city:VARCHAR(20)",
    ),
    ("Proj", "did:INT,name:VARCHAR(20)"),
    ("Loc", "did:INT,addr:VARCHAR(30)"),
];

/// A scratch database plus the four tables, and the `radb-sql` invocations the
/// tests need.
struct Fixture {
    dir: PathBuf,
}

impl Fixture {
    /// Create the directory and the four tables.
    fn new(name: &str) -> Fixture {
        let dir = temp_dir(name);
        for (table, schema) in TABLES {
            let out = storage_cmd()
                .args([
                    "create-table",
                    "--db",
                    dir.to_str().unwrap(),
                    "--name",
                    table,
                    "--schema",
                    schema,
                ])
                .output()
                .expect("run radb-storage");
            assert!(out.status.success(), "create {table}: {}", stderr(&out));
        }
        Fixture { dir }
    }

    /// The database directory, as a `&str`.
    fn dir(&self) -> &str {
        self.dir.to_str().unwrap()
    }

    /// Write `sql` to the fixture's query file.
    fn write(&self, sql: &str) -> PathBuf {
        let file = self.dir.join("query.sql");
        fs::write(&file, sql).expect("write the query file");
        file
    }

    /// Run `compile` on `sql`, with `--json` when `json` is set.
    fn compile_raw(&self, sql: &str, json: bool) -> Output {
        let file = self.write(sql);
        let mut cmd = sql_cmd();
        cmd.args([
            "compile",
            "--db",
            self.dir(),
            "--file",
            file.to_str().unwrap(),
        ]);
        if json {
            cmd.arg("--json");
        }
        cmd.output().expect("run radb-sql")
    }

    /// `compile --json`, asserting success, returning the first plan.
    fn compile(&self, sql: &str) -> Value {
        let plans = self.compile_all(sql);
        assert_eq!(plans.len(), 1, "expected exactly one statement");
        plans.into_iter().next().unwrap()
    }

    /// `compile --json`, asserting success, returning every statement's plan.
    fn compile_all(&self, sql: &str) -> Vec<Value> {
        let out = self.compile_raw(sql, true);
        assert!(out.status.success(), "compile failed: {}", stderr(&out));
        String::from_utf8(out.stdout)
            .expect("stdout is UTF-8")
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_str(line).expect("a plan is JSON"))
            .collect()
    }

    /// `compile` without `--json`, returning the indented tree.
    fn tree(&self, sql: &str) -> String {
        let out = self.compile_raw(sql, false);
        assert!(out.status.success(), "compile failed: {}", stderr(&out));
        String::from_utf8(out.stdout).expect("stdout is UTF-8")
    }

    /// `check`, which prints the first error and exits 1.
    fn check(&self, sql: &str) -> Output {
        let file = self.write(sql);
        sql_cmd()
            .args([
                "check",
                "--db",
                self.dir(),
                "--file",
                file.to_str().unwrap(),
            ])
            .output()
            .expect("run radb-sql")
    }
}

/// The standard error of an invocation, as a `String`.
fn stderr(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// The single error line a failed `check` printed.
fn error_of(out: &Output) -> String {
    assert!(!out.status.success(), "expected a failure, got success");
    let text = stderr(out);
    let line = text.trim();
    assert_eq!(line.lines().count(), 1, "one error and no more: {text}");
    line.to_string()
}

/// The `op` of a plan node.
fn op(node: &Value) -> &str {
    node["op"].as_str().expect("every node has an op")
}

/// The `columns` of a `project`, as owned strings.
fn columns(plan: &Value) -> Vec<String> {
    plan["columns"]
        .as_array()
        .expect("a project has columns")
        .iter()
        .map(|column| match column {
            Value::String(name) => name.clone(),
            Value::Array(pair) => pair[0].as_str().unwrap().to_string(),
            other => panic!("unexpected project column {other}"),
        })
        .collect()
}

/// The `keys` of a `group_by`.
fn group_keys(plan: &Value) -> Vec<String> {
    plan["keys"]
        .as_array()
        .unwrap()
        .iter()
        .map(|key| key.as_str().unwrap().to_string())
        .collect()
}

// =====================================================================
// The fifteen required cases of §7
// =====================================================================

/// Case 1 / §5.5: a self join is one table scanned twice under two renames.
#[test]
fn case_01_self_join_scans_the_table_once_under_two_names() {
    let db = Fixture::new("case01");
    let plan = db.compile("SELECT e.name, m.name FROM Emp e JOIN Emp m ON e.mgr = m.eid;");
    assert_eq!(
        plan,
        json!({
            "op": "project",
            "columns": ["e.name", "m.name"],
            "child": {
                "op": "join",
                "condition": "e.mgr = m.eid",
                "left": {
                    "op": "rename", "to": "e",
                    "child": { "op": "scan", "table": "Emp" }
                },
                "right": {
                    "op": "rename", "to": "m",
                    "child": { "op": "scan", "table": "Emp" }
                }
            }
        })
    );
}

/// Case 2: `SELECT *` over three tables where two share a column name expands
/// every column, each qualified, and raises no error.
#[test]
fn case_02_star_qualifies_every_shared_column() {
    let db = Fixture::new("case02");
    let plan =
        db.compile("SELECT * FROM Emp e, Dept d, Proj p WHERE e.did = d.did AND d.did = p.did;");
    assert_eq!(op(&plan), "project");
    assert_eq!(
        columns(&plan),
        vec![
            "e.eid", "e.name", "e.age", "e.did", "e.salary", "e.mgr", "d.did", "d.title",
            "d.budget", "d.city", "p.did", "p.name",
        ]
    );
    // `did` is in all three entries and `name` in two; the expansion keeps them
    // all apart rather than reporting the ambiguity an unqualified name would.
    assert_eq!(op(&plan["child"]), "select");
    assert_eq!(op(&plan["child"]["child"]), "cross");
}

/// Case 3: an unqualified column present in two tables is an ambiguity error
/// naming both candidates.
#[test]
fn case_03_ambiguous_unqualified_column_names_both_candidates() {
    let db = Fixture::new("case03");
    let error = error_of(&db.check("SELECT did FROM Emp, Dept;"));
    assert!(error.starts_with("error: ambiguity: "), "{error}");
    assert!(error.contains("Emp.did"), "{error}");
    assert!(error.contains("Dept.did"), "{error}");
}

/// Case 4 / §5.7: `UNION` and `UNION ALL` on identical inputs differ in exactly
/// one node, `union` versus `union_all`.
#[test]
fn case_04_union_and_union_all_differ_in_one_node() {
    let db = Fixture::new("case04");
    let all = db.compile("SELECT did FROM Emp UNION ALL SELECT did FROM Dept;");
    let distinct = db.compile("SELECT did FROM Emp UNION SELECT did FROM Dept;");
    assert_eq!(op(&all), "union_all");
    assert_eq!(op(&distinct), "union");
    // The two inputs are identical; only the operator differs.
    assert_eq!(all["left"], distinct["left"]);
    assert_eq!(all["right"], distinct["right"]);
}

/// Case 5: `WHERE` and `HAVING` land on opposite sides of the grouping.
#[test]
fn case_05_where_and_having_straddle_the_grouping() {
    let db = Fixture::new("case05");
    let plan = db.compile(
        "SELECT did, COUNT(*) AS n FROM Emp WHERE age > 25 GROUP BY did HAVING COUNT(*) > 5;",
    );
    assert_eq!(op(&plan), "project");
    let having = &plan["child"];
    assert_eq!(op(having), "select");
    assert_eq!(having["condition"].as_str(), Some("n > 5"));
    let group = &having["child"];
    assert_eq!(op(group), "group_by");
    let where_ = &group["child"];
    assert_eq!(op(where_), "select");
    assert_eq!(where_["condition"].as_str(), Some("Emp.age > 25"));
    assert_eq!(op(&where_["child"]), "scan");
}

/// Case 6: a bare column of a grouping query that is not a grouping column is
/// an aggregate error.
#[test]
fn case_06_non_grouped_column_is_an_aggregate_error() {
    let db = Fixture::new("case06");
    let error = error_of(&db.check("SELECT name, COUNT(*) FROM Emp GROUP BY did;"));
    assert!(error.starts_with("error: aggregate: "), "{error}");
    assert!(error.contains("name"), "{error}");
}

/// Case 7: an aggregate in `WHERE` is an aggregate error, because `WHERE` runs
/// before grouping.
#[test]
fn case_07_aggregate_in_where_is_an_aggregate_error() {
    let db = Fixture::new("case07");
    let error = error_of(&db.check("SELECT * FROM Emp WHERE COUNT(*) > 3;"));
    assert!(error.starts_with("error: aggregate: "), "{error}");
    assert!(error.to_lowercase().contains("where"), "{error}");
}

/// Case 8: a subquery in `FROM` is translated on its own and renamed, and can
/// be joined to a base table.
#[test]
fn case_08_subquery_in_from_joined_to_a_base_table() {
    let db = Fixture::new("case08");
    let plan = db.compile(
        "SELECT t.n, d.title \
           FROM (SELECT did, COUNT(*) AS n FROM Emp GROUP BY did) AS t \
           JOIN Dept d ON t.did = d.did;",
    );
    assert_eq!(op(&plan), "project");
    assert_eq!(columns(&plan), vec!["t.n", "d.title"]);
    let join = &plan["child"];
    assert_eq!(op(join), "join");
    assert_eq!(join["condition"].as_str(), Some("t.did = d.did"));
    // The left side is the subquery, renamed t: rename over project over
    // group_by over scan.
    let rename = &join["left"];
    assert_eq!(op(rename), "rename");
    assert_eq!(rename["to"].as_str(), Some("t"));
    let project = &rename["child"];
    assert_eq!(op(project), "project");
    assert_eq!(columns(project), vec!["Emp.did", "n"]);
    assert_eq!(op(&project["child"]), "group_by");
    // The right side is the base table under its alias.
    let right = &join["right"];
    assert_eq!(op(right), "rename");
    assert_eq!(op(&right["child"]), "scan");
}

/// Case 9: `ORDER BY` may name a column outside the select list; the sort sits
/// above the projection and keys on the fully qualified input column.
#[test]
fn case_09_order_by_a_column_not_in_the_select_list() {
    let db = Fixture::new("case09");
    let plan = db.compile("SELECT name FROM Emp ORDER BY age;");
    assert_eq!(op(&plan), "sort");
    assert_eq!(
        plan["keys"],
        json!([["Emp.age", "asc"]]),
        "the key is the fully qualified FROM-scope column"
    );
    let project = &plan["child"];
    assert_eq!(op(project), "project");
    assert_eq!(columns(project), vec!["Emp.name"]);
    assert_eq!(op(&project["child"]), "scan");
}

/// Case 10: comparing an `INT` column with a string is a type error.
#[test]
fn case_10_int_against_string_is_a_type_error() {
    let db = Fixture::new("case10");
    let error = error_of(&db.check("SELECT name FROM Emp WHERE age > '30';"));
    assert!(error.starts_with("error: type: "), "{error}");
    assert!(error.contains("INT"), "{error}");
    assert!(error.contains("VARCHAR"), "{error}");
}

/// Case 11: an unknown table and an unknown column are reported with different
/// messages, both under the `name` category.
#[test]
fn case_11_unknown_table_and_unknown_column_are_different_errors() {
    let db = Fixture::new("case11");
    let table = error_of(&db.check("SELECT a FROM Nope;"));
    let column = error_of(&db.check("SELECT zzz FROM Emp;"));
    assert!(table.starts_with("error: name: "), "{table}");
    assert!(column.starts_with("error: name: "), "{column}");
    assert!(table.contains("table") && table.contains("Nope"), "{table}");
    assert!(
        column.contains("column") && column.contains("zzz"),
        "{column}"
    );
    assert_ne!(table, column, "the two failures must read differently");
}

/// Case 12: `FROM Emp, Emp` without aliases is a duplicate-alias error.
#[test]
fn case_12_duplicate_table_name_is_an_error() {
    let db = Fixture::new("case12");
    let error = error_of(&db.check("SELECT * FROM Emp, Emp;"));
    assert!(error.starts_with("error: name: "), "{error}");
    assert!(error.contains("duplicate"), "{error}");
    assert!(error.contains("Emp"), "{error}");
    // The same table twice under two aliases is fine.
    let plan = db.compile("SELECT e.name, m.name FROM Emp e, Emp m;");
    assert_eq!(op(&plan), "project");
    assert_eq!(op(&plan["child"]), "cross");
}

/// Case 13: `COUNT(*)` with no `GROUP BY` still produces one row, which the
/// plan expresses as a `group_by` with no keys.
#[test]
fn case_13_count_without_group_by_groups_over_nothing() {
    let db = Fixture::new("case13");
    let plan = db.compile("SELECT COUNT(*) FROM Emp;");
    assert_eq!(op(&plan), "project");
    assert_eq!(columns(&plan), vec!["count_star"]);
    let group = &plan["child"];
    assert_eq!(op(group), "group_by");
    assert!(group_keys(group).is_empty(), "one group, no keys");
    assert_eq!(group["aggregates"], json!([["count", "*", "count_star"]]));
    assert_eq!(op(&group["child"]), "scan");
}

/// Case 14: a `UNION` of two blocks whose column types do not match is a schema
/// error.
#[test]
fn case_14_union_of_mismatched_types_is_a_schema_error() {
    let db = Fixture::new("case14");
    let error = error_of(&db.check("SELECT name FROM Emp UNION SELECT did FROM Dept;"));
    assert!(error.starts_with("error: schema: "), "{error}");
    assert!(error.contains("VARCHAR"), "{error}");
    assert!(error.contains("INT"), "{error}");
}

/// Case 15: a four-table join chain written with commas and with `JOIN ... ON`
/// produces the same output shape, differing in where the predicates sit.
#[test]
fn case_15_four_table_join_chain_two_ways() {
    let db = Fixture::new("case15");
    let comma = db.compile(
        "SELECT * FROM Emp e, Dept d, Proj p, Loc l \
         WHERE e.did = d.did AND d.did = p.did AND p.did = l.did;",
    );
    let joins = db.compile(
        "SELECT * FROM Emp e \
         JOIN Dept d ON e.did = d.did \
         JOIN Proj p ON d.did = p.did \
         JOIN Loc l ON p.did = l.did;",
    );

    assert_eq!(columns(&comma), columns(&joins), "same output columns");

    // The comma form: one `select` holding all three predicates, above a
    // left-deep chain of three `cross` nodes.
    assert_eq!(op(&comma), "project");
    let select = &comma["child"];
    assert_eq!(op(select), "select");
    assert_eq!(
        select["condition"].as_str(),
        Some("e.did = d.did and d.did = p.did and p.did = l.did")
    );
    assert_eq!(depth_of(&select["child"], "cross"), 3, "three crosses");
    assert_eq!(op(&select["child"]["left"]["left"]["left"]), "rename");

    // The JOIN form: a left-deep chain of three `join` nodes, each carrying its
    // own predicate, and no `select` and no `cross` anywhere.
    assert_eq!(op(&joins), "project");
    assert_eq!(depth_of(&joins["child"], "join"), 3, "three joins");
    let inner = &joins["child"]["left"]["left"];
    assert_eq!(op(inner), "join");
    assert_eq!(inner["condition"].as_str(), Some("e.did = d.did"));
    assert_eq!(op(&inner["left"]), "rename");
}

// =====================================================================
// The worked examples of §5
// =====================================================================

/// §5.1 reproduces exactly: a filter over a scan, no distinct.
#[test]
fn spec_example_5_1_filter_and_projection() {
    let db = Fixture::new("spec51");
    let plan = db.compile("SELECT name, age FROM Emp WHERE age > 30;");
    assert_eq!(
        plan,
        json!({
            "op": "project",
            "columns": ["Emp.name", "Emp.age"],
            "child": {
                "op": "select",
                "condition": "Emp.age > 30",
                "child": { "op": "scan", "table": "Emp" }
            }
        })
    );
}

/// A non-aggregate select alias is an `[input, output]` project column.
#[test]
fn select_alias_is_a_project_column() {
    let db = Fixture::new("select_alias");
    let plan = db.compile("SELECT name AS n FROM Emp ORDER BY n;");
    assert_eq!(
        plan,
        json!({
            "op": "sort",
            "keys": [["n", "asc"]],
            "child": {
                "op": "project",
                "columns": [["Emp.name", "n"]],
                "child": { "op": "scan", "table": "Emp" }
            }
        })
    );
}

/// §5.2 reproduces exactly: the `WHERE` predicate stays above the join.
#[test]
fn spec_example_5_2_join_with_aliases() {
    let db = Fixture::new("spec52");
    let plan = db.compile(
        "SELECT e.name, d.title \
           FROM Emp e JOIN Dept d ON e.did = d.did \
          WHERE d.budget > 1000;",
    );
    assert_eq!(
        plan,
        json!({
            "op": "project",
            "columns": ["e.name", "d.title"],
            "child": {
                "op": "select",
                "condition": "d.budget > 1000",
                "child": {
                    "op": "join",
                    "condition": "e.did = d.did",
                    "left": {
                        "op": "rename", "to": "e",
                        "child": { "op": "scan", "table": "Emp" }
                    },
                    "right": {
                        "op": "rename", "to": "d",
                        "child": { "op": "scan", "table": "Dept" }
                    }
                }
            }
        })
    );
}

/// §5.3: a comma in `FROM` becomes `cross`, and the predicate stays in the
/// `select` above it.
#[test]
fn spec_example_5_3_comma_join_is_a_cross() {
    let db = Fixture::new("spec53");
    let plan = db.compile("SELECT * FROM Emp e, Dept d WHERE e.did = d.did;");
    assert_eq!(op(&plan["child"]), "select");
    assert_eq!(op(&plan["child"]["child"]), "cross");
}

/// §5.4 reproduces exactly: `LIMIT` over `SORT` over `PROJECT` over `HAVING`
/// over `GROUP BY` over `WHERE` over `SCAN`.
#[test]
fn spec_example_5_4_grouping_having_order_limit() {
    let db = Fixture::new("spec54");
    let plan = db.compile(
        "SELECT did, COUNT(*) AS n, AVG(salary) AS avg_sal \
           FROM Emp WHERE age > 25 GROUP BY did \
         HAVING COUNT(*) > 5 ORDER BY n DESC LIMIT 10;",
    );
    assert_eq!(
        plan,
        json!({
            "op": "limit",
            "n": 10,
            "child": {
                "op": "sort",
                "keys": [["n", "desc"]],
                "child": {
                    "op": "project",
                    "columns": ["Emp.did", "n", "avg_sal"],
                    "child": {
                        "op": "select",
                        "condition": "n > 5",
                        "child": {
                            "op": "group_by",
                            "keys": ["Emp.did"],
                            "aggregates": [
                                ["count", "*", "n"],
                                ["avg", "Emp.salary", "avg_sal"]
                            ],
                            "child": {
                                "op": "select",
                                "condition": "Emp.age > 25",
                                "child": { "op": "scan", "table": "Emp" }
                            }
                        }
                    }
                }
            }
        })
    );
}

/// §5.6 reproduces exactly: a subquery in `FROM` translated on its own and then
/// renamed.
#[test]
fn spec_example_5_6_subquery_in_from() {
    let db = Fixture::new("spec56");
    let plan = db.compile(
        "SELECT t.did, t.n \
           FROM (SELECT did, COUNT(*) AS n FROM Emp GROUP BY did) AS t \
          WHERE t.n > 3;",
    );
    assert_eq!(
        plan,
        json!({
            "op": "project",
            "columns": ["t.did", "t.n"],
            "child": {
                "op": "select",
                "condition": "t.n > 3",
                "child": {
                    "op": "rename",
                    "to": "t",
                    "child": {
                        "op": "project",
                        "columns": ["Emp.did", "n"],
                        "child": {
                            "op": "group_by",
                            "keys": ["Emp.did"],
                            "aggregates": [["count", "*", "n"]],
                            "child": { "op": "scan", "table": "Emp" }
                        }
                    }
                }
            }
        })
    );
}

/// §5.7: the explain output makes the difference between the two unions visible.
#[test]
fn spec_example_5_7_union_explain_shows_the_difference() {
    let db = Fixture::new("spec57");
    let all = db.tree("SELECT did FROM Emp UNION ALL SELECT did FROM Dept;");
    let distinct = db.tree("SELECT did FROM Emp UNION SELECT did FROM Dept;");
    assert!(all.starts_with("union_all\n"), "{all}");
    assert!(distinct.starts_with("union\n"), "{distinct}");
    // Everything below the operator is the same.
    assert_eq!(
        all.trim_start_matches("union_all\n"),
        distinct.trim_start_matches("union\n")
    );
}

// =====================================================================
// Cross-cutting checks
// =====================================================================

/// Bag semantics: only `DISTINCT` adds a `distinct` node above a projection; a
/// plain projection never deduplicates (§4).
#[test]
fn only_distinct_adds_a_distinct_node() {
    let db = Fixture::new("bag_distinct");
    let plain = db.compile("SELECT did FROM Emp;");
    assert_eq!(op(&plain), "project", "no distinct without DISTINCT");
    let distinct = db.compile("SELECT DISTINCT did FROM Emp;");
    assert_eq!(op(&distinct), "distinct");
    assert_eq!(op(&distinct["child"]), "project");
}

/// Bag semantics: a `GROUP BY` already returns one row per group, so `DISTINCT`
/// above it is redundant and not emitted (§4).
#[test]
fn group_by_never_adds_a_distinct_node() {
    let db = Fixture::new("bag_group");
    let plan = db.compile("SELECT DISTINCT did, COUNT(*) FROM Emp GROUP BY did;");
    assert_eq!(op(&plan), "project");
    assert_eq!(op(&plan["child"]), "group_by");
}

/// Name resolution: after the resolving pass no unqualified column reference
/// survives in the plan; only select-list aliases stay bare.
#[test]
fn resolved_columns_are_fully_qualified() {
    let db = Fixture::new("qualified");
    let plan = db.compile(
        "SELECT e.name, d.title \
           FROM Emp e JOIN Dept d ON e.did = d.did \
          WHERE d.budget > 100 ORDER BY e.name;",
    );
    assert_eq!(op(&plan), "sort");
    assert_eq!(columns(&plan["child"]), vec!["e.name", "d.title"]);
    // The projection under the sort keeps its qualifier, the sort key too, and
    // so does every attribute of the condition below it.
    let project = &plan["child"];
    assert_eq!(op(project), "project");
    for column in columns(project) {
        assert!(column.contains('.'), "unqualified column {column}");
    }
    assert_eq!(plan["keys"], json!([["e.name", "asc"]]));
    let condition = plan["child"]["child"]["condition"].as_str().unwrap();
    for attribute in condition.split_whitespace() {
        if attribute == ">" || attribute.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        assert!(attribute.contains('.'), "unqualified attribute {attribute}");
    }
}

/// The one error format of §6, with a 0-based column, for a lexical failure.
#[test]
fn lexical_errors_use_the_one_format() {
    let db = Fixture::new("lexical");
    let out = db.check("SELECT name FROM Emp WHERE name = 'oops;");
    let error = error_of(&out);
    assert!(error.starts_with("error: lexical: "), "{error}");
    assert!(error.contains("at line 1, column "), "{error}");
}

/// `parse --explain` prints the syntax tree, without a catalog.
#[test]
fn parse_explain_prints_the_syntax_tree() {
    let out = sql_cmd()
        .args([
            "parse",
            "--query",
            "SELECT did, COUNT(*) AS n FROM Emp GROUP BY did HAVING COUNT(*) > 5 ORDER BY n DESC LIMIT 3;",
            "--explain",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", stderr(&out));
    let tree = String::from_utf8(out.stdout).unwrap();
    for needle in [
        "QUERY",
        "SELECT",
        "COUNT(*) AS n",
        "FROM",
        "TABLE Emp",
        "GROUP BY did",
        "HAVING COUNT(*) > 5",
        "ORDER BY n desc",
        "LIMIT 3",
    ] {
        assert!(tree.contains(needle), "missing {needle} in:\n{tree}");
    }
}

/// A file may hold several statements; each compiles on its own, in order.
#[test]
fn multiple_statements_compile_in_order() {
    let db = Fixture::new("multi");
    let plans = db.compile_all("SELECT did FROM Emp; SELECT title FROM Dept;");
    assert_eq!(plans.len(), 2);
    assert_eq!(columns(&plans[0]), vec!["Emp.did"]);
    assert_eq!(columns(&plans[1]), vec!["Dept.title"]);
}

/// `CREATE TABLE` and `INSERT` parse, check and translate, even though §5 gives
/// them no relational operator.
#[test]
fn create_and_insert_are_accepted() {
    let db = Fixture::new("ddl");
    let out = db.check("CREATE TABLE T (a INT, b VARCHAR(10));");
    assert!(out.status.success(), "{}", stderr(&out));
    let out = db.check("INSERT INTO Emp VALUES (1, 'x', 30, 1, 100, 0);");
    assert!(out.status.success(), "{}", stderr(&out));

    let all = db.compile_all(
        "CREATE TABLE T (a INT, b VARCHAR(10));\nINSERT INTO Emp VALUES (1, 'x', 30, 1, 100, 0);",
    );
    assert_eq!(all[0]["op"].as_str(), Some("create_table"));
    assert_eq!(all[0]["table"].as_str(), Some("T"));
    assert_eq!(all[1]["op"].as_str(), Some("insert"));
    assert_eq!(all[1]["table"].as_str(), Some("Emp"));
}

/// The default aggregate name is `<function>_<argument>`, with `.` as `_` and
/// `*` as `star` (§5.4).
#[test]
fn aggregate_default_names_follow_the_documented_rule() {
    let db = Fixture::new("agg_names");
    let plan = db.compile("SELECT COUNT(*), AVG(salary), SUM(age), MIN(name) FROM Emp;");
    assert_eq!(
        columns(&plan),
        vec![
            "count_star",
            "avg_Emp_salary",
            "sum_Emp_age",
            "min_Emp_name",
        ]
    );
    let group = &plan["child"];
    assert_eq!(
        group["aggregates"],
        json!([
            ["count", "*", "count_star"],
            ["avg", "Emp.salary", "avg_Emp_salary"],
            ["sum", "Emp.age", "sum_Emp_age"],
            ["min", "Emp.name", "min_Emp_name"]
        ])
    );
}

/// A `HAVING` aggregate that the select list does not mention is still computed
/// by the `group_by`, and the condition names its output.
#[test]
fn having_may_introduce_its_own_aggregate() {
    let db = Fixture::new("having_agg");
    let plan = db.compile("SELECT did FROM Emp GROUP BY did HAVING SUM(salary) > 100;");
    let having = &plan["child"];
    assert_eq!(op(having), "select");
    assert_eq!(having["condition"].as_str(), Some("sum_Emp_salary > 100"));
    assert_eq!(
        having["child"]["aggregates"],
        json!([["sum", "Emp.salary", "sum_Emp_salary"]])
    );
}

/// `SUM` and `AVG` over a `VARCHAR` column is a type error (§3.2).
#[test]
fn sum_over_a_string_is_a_type_error() {
    let db = Fixture::new("sum_str");
    let error = error_of(&db.check("SELECT SUM(name) FROM Emp;"));
    assert!(error.starts_with("error: type: "), "{error}");
    assert!(error.contains("SUM"), "{error}");
    assert!(error.contains("VARCHAR"), "{error}");
}

/// The three set operators other than union are recognised and produce their
/// own node.
#[test]
fn intersect_and_except_produce_their_own_nodes() {
    let db = Fixture::new("setops");
    assert_eq!(
        op(&db.compile("SELECT did FROM Emp INTERSECT SELECT did FROM Dept;")),
        "intersect"
    );
    assert_eq!(
        op(&db.compile("SELECT did FROM Emp EXCEPT SELECT did FROM Dept;")),
        "except"
    );
}

/// Set operators are left-associative and share one precedence, so
/// `A UNION B EXCEPT C` is `(A UNION B) EXCEPT C`.
#[test]
fn set_operators_fold_to_the_left() {
    let db = Fixture::new("setops_fold");
    let plan =
        db.compile("SELECT did FROM Emp UNION SELECT did FROM Dept EXCEPT SELECT did FROM Proj;");
    assert_eq!(op(&plan), "except");
    assert_eq!(op(&plan["left"]), "union");
    assert_eq!(op(&plan["right"]), "project");
}

/// `NOT`, `AND` and `OR` keep their precedence in the rendered condition, which
/// re-parses to the same tree through the Module 2 entry point.
#[test]
fn condition_precedence_survives_rendering() {
    let db = Fixture::new("precedence");
    let plan = db.compile("SELECT name FROM Emp WHERE NOT (age > 30 OR salary < 100) AND did = 1;");
    let condition = plan["child"]["condition"].as_str().unwrap();
    assert_eq!(
        condition,
        "not (Emp.age > 30 or Emp.salary < 100) and Emp.did = 1"
    );
    // The rendered string re-parses through the shared Module 2 entry point and
    // re-renders identically, so Module 4 can read the plan's condition back.
    let parsed: radb::Condition = condition.parse().expect("the plan condition re-parses");
    assert_eq!(parsed.to_string(), condition);
}

// =====================================================================
// Small helpers used by a couple of structural checks
// =====================================================================

/// How many times a unary/binary node name appears in the left spine of `node`.
fn depth_of(mut node: &Value, name: &str) -> usize {
    let mut count = 0;
    while op(node) == name {
        count += 1;
        node = &node["left"];
    }
    count
}
