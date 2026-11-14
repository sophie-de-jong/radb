//! Presentation of compiled statements: logical-plan JSON and the indented
//! tree the `compile` command prints.
//!
//! This lives in the library so the Module 3 binary only parses arguments,
//! compiles, and prints. Both forms are deterministic functions of a
//! [`Statement`]; tests use them without starting a process.

use crate::Value;
use crate::sql::compiler::Statement;
use crate::tree::{IndentTree, TreeLine};

/// One checked statement as JSON: a query's logical plan, or a non-relational
/// `create_table`/`insert` description.
pub fn statement_json(statement: &Statement) -> serde_json::Value {
    match statement {
        Statement::Select(plan) => serde_json::to_value(plan).expect("plans are serialisable"),
        Statement::Create { name, schema } => serde_json::json!({
            "op": "create_table",
            "table": name,
            "columns": schema.json(),
        }),
        Statement::Insert { table_name, values } => serde_json::json!({
            "op": "insert",
            "table": table_name,
            "values": values.iter().map(Value::json).collect::<Vec<_>>(),
        }),
    }
}

/// One compiled statement as an owned tree.
pub fn statement_tree(statement: &Statement) -> TreeLine {
    match statement {
        Statement::Select(plan) => TreeLine::from_tree(plan),
        Statement::Create { name, schema } => {
            let mut root = TreeLine::new(format!("create_table {name}"));
            for column in schema.columns() {
                root.push(TreeLine::new(format!("{} {}", column.name, column.ty)));
            }
            root
        }
        Statement::Insert { table_name, values } => {
            let mut root = TreeLine::new(format!("insert {table_name}"));
            let values: Vec<String> = values.iter().map(render_value).collect();
            root.push(TreeLine::new(format!("({})", values.join(", "))));
            root
        }
    }
}

/// One compiled statement as an indented tree string.
pub fn render_statement(statement: &Statement) -> String {
    IndentTree::new(&statement_tree(statement)).to_string()
}

/// One literal value, as written.
fn render_value(value: &Value) -> String {
    match value {
        Value::Int(value) => value.to_string(),
        Value::Varchar(value) => format!("'{}'", value.replace('\'', "''")),
    }
}
