//! Presentation of compiled statements: logical-plan JSON and the indented
//! tree the `compile` command prints.
//!
//! This lives in the library so the Module 3 binary only parses arguments,
//! compiles, and prints. Both forms are deterministic functions of a
//! [`Statement`]; tests use them without starting a process.

use crate::Value;
use crate::exec::Direction;
use crate::sql::compiler::Statement;
use crate::sql::plan::Plan;

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

/// One compiled statement as an indented tree.
pub fn render_statement(statement: &Statement) -> String {
    let mut out = String::new();
    match statement {
        Statement::Select(plan) => render_plan(&mut out, 0, plan),
        Statement::Create { name, schema } => {
            push_line(&mut out, 0, &format!("create_table {name}"));
            for column in schema.columns() {
                push_line(&mut out, 1, &format!("{} {}", column.name, column.ty));
            }
        }
        Statement::Insert { table_name, values } => {
            push_line(&mut out, 0, &format!("insert {table_name}"));
            let values: Vec<String> = values.iter().map(render_value).collect();
            push_line(&mut out, 1, &format!("({})", values.join(", ")));
        }
    }
    out
}

/// Append `text` as one line at `depth`.
fn push_line(out: &mut String, depth: usize, text: &str) {
    for _ in 0..depth {
        out.push_str("  ");
    }
    out.push_str(text);
    out.push('\n');
}

/// One literal value, as written.
fn render_value(value: &Value) -> String {
    match value {
        Value::Int(value) => value.to_string(),
        Value::Varchar(value) => format!("'{}'", value.replace('\'', "''")),
    }
}

/// Render a logical plan tree, one operator per line.
fn render_plan(out: &mut String, depth: usize, plan: &Plan) {
    match plan {
        Plan::Scan { table } => push_line(out, depth, &format!("scan {table}")),
        Plan::Select { condition, child } => {
            push_line(out, depth, &format!("select \"{condition}\""));
            render_plan(out, depth + 1, child);
        }
        Plan::Project { columns, child } => {
            let columns: Vec<String> = columns
                .iter()
                .map(|(input, output)| {
                    if input == output {
                        input.clone()
                    } else {
                        format!("{input} as {output}")
                    }
                })
                .collect();
            push_line(out, depth, &format!("project [{}]", columns.join(", ")));
            render_plan(out, depth + 1, child);
        }
        Plan::Join {
            condition,
            left,
            right,
        } => {
            push_line(out, depth, &format!("join \"{condition}\""));
            render_plan(out, depth + 1, left);
            render_plan(out, depth + 1, right);
        }
        Plan::Cross { left, right } => {
            push_line(out, depth, "cross");
            render_plan(out, depth + 1, left);
            render_plan(out, depth + 1, right);
        }
        Plan::Rename { to, child } => {
            push_line(out, depth, &format!("rename {to}"));
            render_plan(out, depth + 1, child);
        }
        Plan::GroupBy {
            keys,
            aggregates,
            child,
        } => {
            let aggregates: Vec<String> = aggregates
                .iter()
                .map(|spec| format!("[{},{},{}]", spec.0, spec.1, spec.2))
                .collect();
            push_line(
                out,
                depth,
                &format!(
                    "group_by keys [{}] aggregates [{}]",
                    keys.join(", "),
                    aggregates.join(", ")
                ),
            );
            render_plan(out, depth + 1, child);
        }
        Plan::Distinct { child } => {
            push_line(out, depth, "distinct");
            render_plan(out, depth + 1, child);
        }
        Plan::Sort { keys, child } => {
            let keys: Vec<String> = keys
                .iter()
                .map(|(name, direction)| format!("[{name},{}]", direction_name(*direction)))
                .collect();
            push_line(out, depth, &format!("sort [{}]", keys.join(", ")));
            render_plan(out, depth + 1, child);
        }
        Plan::Limit { n, child } => {
            push_line(out, depth, &format!("limit {n}"));
            render_plan(out, depth + 1, child);
        }
        Plan::UnionAll { left, right } => render_binary(out, depth, "union_all", left, right),
        Plan::Union { left, right } => render_binary(out, depth, "union", left, right),
        Plan::Intersect { left, right } => render_binary(out, depth, "intersect", left, right),
        Plan::Except { left, right } => render_binary(out, depth, "except", left, right),
    }
}

/// A two-input operator.
fn render_binary(out: &mut String, depth: usize, name: &str, left: &Plan, right: &Plan) {
    push_line(out, depth, name);
    render_plan(out, depth + 1, left);
    render_plan(out, depth + 1, right);
}

/// The plan's spelling of a sort direction.
fn direction_name(direction: Direction) -> &'static str {
    match direction {
        Direction::Asc => "asc",
        Direction::Desc => "desc",
    }
}
