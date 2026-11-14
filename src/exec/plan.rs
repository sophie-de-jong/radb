//! The physical plan: the JSON a `run` or `trace` is asked to execute (§2).
//!
//! A plan is a tree of operators. The tree is parsed here, before any schema is
//! known. A condition is a typed [`Condition`]: its JSON string form is parsed
//! when the plan is deserialized. Column lists stay strings until the operator
//! tree is built and their schema is known.
//!
//! Every type here reads itself from JSON with `serde`'s `Deserialize` derive.
//! The two shapes a derive cannot describe on its own are the two-element
//! arrays the plan uses for a sort key and an aggregate: a sort key is a tuple
//! struct and an aggregate converts from its `(function, column)` pair, so
//! neither needs a hand-written visitor.
//!
//! The JSON shapes are exactly §2's:
//!
//! ```text
//! { "op": "seqscan", "table": "R" }
//! { "op": "filter", "condition": "R.a > 500 and R.b = 'cs'", "child": { … } }
//! { "op": "project", "columns": ["R.a", "R.c"], "child": { … } }
//! { "op": "distinct", "mode": "sort" | "hash", "child": { … } }
//! { "op": "sort", "keys": [["R.a","asc"],["R.b","desc"]], "child": { … } }
//! { "op": "group_by", "mode": "sort" | "hash", "keys": ["R.dept"],
//!   "aggregates": [["count","*"],["avg","R.salary"]], "child": { … } }
//! { "op": "nested_loop_join" | "sort_merge_join" | "hash_join",
//!   "condition": "R.b = S.b", "left": { … }, "right": { … } }
//! { "op": "union_all" | "union" | "intersect" | "except",
//!   "left": { … }, "right": { … } }
//! { "op": "limit", "n": 10, "child": { … } }
//! ```

use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::sql::ast::Condition;
use crate::storage::{BufferPool, Catalog};

use super::error::ExecError;
use super::executor::{Context, Executor};
use super::operators::distinct::Distinct;
use super::operators::except::Except;
use super::operators::filter::Filter;
use super::operators::group_by::GroupBy;
use super::operators::hash_join::HashJoin;
use super::operators::intersect::Intersect;
use super::operators::limit::Limit;
use super::operators::nested_loop_join::NestedLoopJoin;
use super::operators::project::Project;
use super::operators::seq_scan::SeqScan;
use super::operators::sort::Sort;
use super::operators::sort_merge_join::SortMergeJoin;
use super::operators::union::Union;
use super::operators::union_all::UnionAll;
use super::order::Direction;
use crate::exec::Operator;

/// How a `distinct` or `group_by` is implemented (§2.4, §2.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    /// Sort the input and collapse adjacent equals.
    Sort,
    /// Partition to disk, then collapse each partition.
    Hash,
}

/// One sort key: a column and a direction (§2.5).
///
/// The plan writes a key as the two-element array `["R.a","asc"]`, so this is a
/// tuple struct: the `Deserialize` derive reads the array directly, with no
/// hand-written visitor and no `bool` field to convert.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(from = "(String, Direction)", into = "(String, Direction)")]
pub struct SortKey {
    column_name: String,
    direction: Direction,
}

impl SortKey {
    pub fn new(column_name: String, direction: Direction) -> Self {
        Self {
            column_name,
            direction,
        }
    }
    /// The qualified column name.
    pub fn column(&self) -> &str {
        &self.column_name
    }

    /// The key's direction, `asc` or `desc`.
    pub fn direction(&self) -> Direction {
        self.direction
    }
}

impl From<(String, Direction)> for SortKey {
    fn from((column_name, direction): (String, Direction)) -> Self {
        Self {
            column_name,
            direction,
        }
    }
}

impl From<SortKey> for (String, Direction) {
    fn from(k: SortKey) -> Self {
        (k.column_name, k.direction)
    }
}

/// An aggregate function of a `group_by` (§2.6).
///
/// The column it reads is carried beside it in the plan's `(function, column)`
/// pair; `COUNT(*)` uses `"*"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Aggregate {
    /// `COUNT`, with or without a column.
    Count,
    /// `SUM`, `INT` columns only.
    Sum,
    /// `AVG`, `INT` columns only.
    Avg,
    /// `MIN`.
    Min,
    /// `MAX`.
    Max,
}

impl Aggregate {
    /// The output name of an aggregate: `count_star`, `sum_R_salary`, ...
    pub fn column_name(self, name: &str) -> String {
        let suffix = if name == "*" {
            "star".to_string()
        } else {
            name.replace('.', "_")
        };
        match self {
            Aggregate::Count => format!("count_{suffix}"),
            Aggregate::Sum => format!("sum_{suffix}"),
            Aggregate::Avg => format!("avg_{suffix}"),
            Aggregate::Min => format!("min_{suffix}"),
            Aggregate::Max => format!("max_{suffix}"),
        }
    }
}

impl fmt::Display for Aggregate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Aggregate::Count => f.write_str("COUNT"),
            Aggregate::Sum => f.write_str("SUM"),
            Aggregate::Avg => f.write_str("AVG"),
            Aggregate::Min => f.write_str("MIN"),
            Aggregate::Max => f.write_str("MAX"),
        }
    }
}

/// A physical plan tree.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Plan {
    /// Every live tuple of a table (§2.1).
    #[serde(rename = "seqscan")]
    SeqScan {
        /// The table name.
        table: String,
        /// The qualifier its output columns carry, when it is not the table
        /// name (`FROM Emp e`). Absent for a plain scan.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        alias: Option<String>,
    },
    /// Keep tuples for which the condition holds (§2.2).
    Filter {
        /// The filter predicate.
        condition: Condition,
        /// The input.
        child: Box<Plan>,
    },
    /// Keep the listed columns, in order (§2.3).
    Project {
        /// The qualified column names.
        columns: Vec<String>,
        /// The input.
        child: Box<Plan>,
    },
    /// Remove duplicate tuples (§2.4).
    Distinct {
        /// `sort` or `hash`.
        mode: Mode,
        /// The input.
        child: Box<Plan>,
    },
    /// External merge sort (§2.5).
    Sort {
        /// The sort keys, most significant first.
        keys: Vec<SortKey>,
        /// The input.
        child: Box<Plan>,
    },
    /// Group and aggregate (§2.6).
    GroupBy {
        /// `sort` or `hash`.
        mode: Mode,
        /// The grouping columns.
        keys: Vec<String>,
        /// The aggregates.
        aggregates: Vec<(Aggregate, String)>,
        /// The input.
        child: Box<Plan>,
    },
    /// Block nested loop join (§2.7).
    NestedLoopJoin {
        /// The join predicate.
        condition: Condition,
        /// The outer input.
        left: Box<Plan>,
        /// The inner input.
        right: Box<Plan>,
    },
    /// Sort merge join (§2.8).
    SortMergeJoin {
        /// The join predicate, equalities only.
        condition: Condition,
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
    },
    /// Grace hash join (§2.9).
    HashJoin {
        /// The join predicate, equalities only.
        condition: Condition,
        /// The build input.
        left: Box<Plan>,
        /// The probe input.
        right: Box<Plan>,
    },
    /// Concatenation (§2.10).
    UnionAll {
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
    },
    /// Concatenation followed by duplicate removal.
    Union {
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
    },
    /// Set intersection.
    Intersect {
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
    },
    /// Set difference.
    Except {
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
    },
    /// The first `n` tuples (§2.10).
    Limit {
        /// How many tuples to return.
        n: u64,
        /// The input.
        child: Box<Plan>,
    },
}

impl fmt::Display for Plan {
    /// The indented tree an `EXPLAIN` prints, one operator per line.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.write_tree(f, 0)
    }
}

impl Plan {
    /// Write this node's line and its children's, indented by `depth`.
    fn write_tree(&self, f: &mut fmt::Formatter<'_>, depth: usize) -> fmt::Result {
        write!(f, "{}", "  ".repeat(depth))?;
        match self {
            Plan::SeqScan { table, alias } => match alias {
                Some(alias) => writeln!(f, "seqscan {table} as {alias}"),
                None => writeln!(f, "seqscan {table}"),
            },
            Plan::Filter { condition, child } => {
                writeln!(f, "filter {condition}")?;
                child.write_tree(f, depth + 1)
            }
            Plan::Project { columns, child } => {
                writeln!(f, "project [{}]", columns.join(", "))?;
                child.write_tree(f, depth + 1)
            }
            Plan::Sort { keys, child } => {
                let keys: Vec<String> = keys
                    .iter()
                    .map(|key| format!("{}:{:?}", key.column(), key.direction()))
                    .collect();
                writeln!(f, "sort [{}]", keys.join(", "))?;
                child.write_tree(f, depth + 1)
            }
            Plan::GroupBy {
                mode,
                keys,
                aggregates,
                child,
            } => {
                writeln!(
                    f,
                    "group_by {mode:?} [{}] [{aggregates:?}]",
                    keys.join(", ")
                )?;
                child.write_tree(f, depth + 1)
            }
            Plan::Distinct { mode, child } => {
                writeln!(f, "distinct {mode:?}")?;
                child.write_tree(f, depth + 1)
            }
            Plan::Limit { n, child } => {
                writeln!(f, "limit {n}")?;
                child.write_tree(f, depth + 1)
            }
            Plan::HashJoin {
                condition,
                left,
                right,
            } => {
                writeln!(f, "hash_join {condition}")?;
                left.write_tree(f, depth + 1)?;
                right.write_tree(f, depth + 1)
            }
            Plan::NestedLoopJoin {
                condition,
                left,
                right,
            } => {
                writeln!(f, "nested_loop_join {condition}")?;
                left.write_tree(f, depth + 1)?;
                right.write_tree(f, depth + 1)
            }
            Plan::SortMergeJoin {
                condition,
                left,
                right,
            } => {
                writeln!(f, "sort_merge_join {condition}")?;
                left.write_tree(f, depth + 1)?;
                right.write_tree(f, depth + 1)
            }
            Plan::UnionAll { left, right } => {
                writeln!(f, "union_all")?;
                left.write_tree(f, depth + 1)?;
                right.write_tree(f, depth + 1)
            }
            Plan::Union { left, right } => {
                writeln!(f, "union")?;
                left.write_tree(f, depth + 1)?;
                right.write_tree(f, depth + 1)
            }
            Plan::Intersect { left, right } => {
                writeln!(f, "intersect")?;
                left.write_tree(f, depth + 1)?;
                right.write_tree(f, depth + 1)
            }
            Plan::Except { left, right } => {
                writeln!(f, "except")?;
                left.write_tree(f, depth + 1)?;
                right.write_tree(f, depth + 1)
            }
        }
    }
}

impl Plan {
    /// A compact one-line description, e.g. `hash_join (R.b = S.b) [scan R, scan S]`.
    pub fn one_line(&self) -> String {
        match self {
            Plan::SeqScan { table, alias } => match alias {
                Some(alias) => format!("scan {table} as {alias}"),
                None => format!("scan {table}"),
            },
            Plan::Filter { condition, child } => {
                format!("filter ({condition}) -> {}", child.one_line())
            }
            Plan::Project { columns, child } => {
                format!("project [{}] -> {}", columns.join(", "), child.one_line())
            }
            Plan::Sort { keys, child } => {
                let keys: Vec<String> = keys
                    .iter()
                    .map(|key| format!("{}:{:?}", key.column(), key.direction()))
                    .collect();
                format!("sort [{}] -> {}", keys.join(", "), child.one_line())
            }
            Plan::GroupBy {
                mode,
                keys,
                aggregates,
                child,
            } => format!(
                "group_by {mode:?} [{}] [{aggregates:?}] -> {}",
                keys.join(", "),
                child.one_line()
            ),
            Plan::Distinct { mode, child } => {
                format!("distinct {mode:?} -> {}", child.one_line())
            }
            Plan::Limit { n, child } => format!("limit {n} -> {}", child.one_line()),
            Plan::HashJoin {
                condition,
                left,
                right,
            } => format!(
                "hash_join ({condition}) [{}, {}]",
                left.one_line(),
                right.one_line()
            ),
            Plan::NestedLoopJoin {
                condition,
                left,
                right,
            } => format!(
                "nested_loop_join ({condition}) [{}, {}]",
                left.one_line(),
                right.one_line()
            ),
            Plan::SortMergeJoin {
                condition,
                left,
                right,
            } => format!(
                "sort_merge_join ({condition}) [{}, {}]",
                left.one_line(),
                right.one_line()
            ),
            Plan::UnionAll { left, right } => {
                format!("union_all [{}, {}]", left.one_line(), right.one_line())
            }
            Plan::Union { left, right } => {
                format!("union [{}, {}]", left.one_line(), right.one_line())
            }
            Plan::Intersect { left, right } => {
                format!("intersect [{}, {}]", left.one_line(), right.one_line())
            }
            Plan::Except { left, right } => {
                format!("except [{}, {}]", left.one_line(), right.one_line())
            }
        }
    }
}

impl Plan {
    /// Build the plan's operator tree and open it over `pool`, returning an
    /// [`Executor`] the caller pulls rows from.
    ///
    /// A context is created over `pool`, with data files under `db_dir`.
    /// Building resolves tables against `catalog` and opens their data files;
    /// a missing table or column, a bad type, or a shape mismatch fails here,
    /// before any page is read. Opening then prepares every operator,
    /// acquiring buffers and doing any blocking work; if building or opening
    /// fails, the partially built tree is closed before the error returns.
    ///
    /// Errors: [`ExecError::UnknownTable`], [`ExecError::UnknownColumn`],
    /// [`ExecError::Type`] or [`ExecError::Arity`], or whatever `open` raises.
    pub fn execute<'a>(
        &self,
        catalog: &Catalog,
        pool: &'a mut BufferPool,
        db_dir: PathBuf,
    ) -> Result<Executor<'a>, ExecError> {
        let mut ctx = Context::new(pool, db_dir);
        let mut root = self.build(catalog, &mut ctx)?;
        if let Err(err) = root.open(&mut ctx) {
            let _ = root.close(&mut ctx);
            return Err(err);
        }
        Ok(Executor::new(root, ctx))
    }

    /// Construct the operator tree for this plan.
    ///
    /// This resolves and validates the plan against `catalog` but does not
    /// open the operators; [`Plan::execute`] builds and opens in one step.
    fn build(&self, catalog: &Catalog, ctx: &mut Context) -> Result<Box<dyn Operator>, ExecError> {
        match self {
            Plan::SeqScan { table, alias } => {
                let meta = catalog.require(table)?;
                let file = ctx.file_for(meta)?;
                let mut schema = meta.schema.clone();
                if let Some(qualifier) = alias {
                    schema.qualify(qualifier)
                } else {
                    schema.qualify(table);
                }
                Ok(Box::new(SeqScan::new(table.clone(), file, schema)?))
            }
            Plan::Filter { condition, child } => {
                let child = child.build(catalog, ctx)?;
                Ok(Box::new(Filter::new(child, condition)?))
            }
            Plan::Project { columns, child } => {
                let child = child.build(catalog, ctx)?;
                Ok(Box::new(Project::new(child, columns)?))
            }
            Plan::Limit { n, child } => {
                let child = child.build(catalog, ctx)?;
                Ok(Box::new(Limit::new(child, *n)))
            }
            Plan::Sort { keys, child } => {
                let child = child.build(catalog, ctx)?;
                Ok(Box::new(Sort::new(child, keys)?))
            }
            Plan::UnionAll { left, right } => {
                let left = left.build(catalog, ctx)?;
                let right = right.build(catalog, ctx)?;
                Ok(Box::new(UnionAll::new(left, right)?))
            }
            Plan::NestedLoopJoin {
                condition,
                left,
                right,
            } => {
                let left = left.build(catalog, ctx)?;
                let right = right.build(catalog, ctx)?;
                Ok(Box::new(NestedLoopJoin::new(left, right, condition)?))
            }
            Plan::SortMergeJoin {
                condition,
                left,
                right,
            } => {
                let left = left.build(catalog, ctx)?;
                let right = right.build(catalog, ctx)?;
                Ok(Box::new(SortMergeJoin::new(left, right, condition)?))
            }
            Plan::HashJoin {
                condition,
                left,
                right,
            } => {
                let left = left.build(catalog, ctx)?;
                let right = right.build(catalog, ctx)?;
                Ok(Box::new(HashJoin::new(left, right, condition)?))
            }
            Plan::Distinct { mode, child } => {
                let child = child.build(catalog, ctx)?;
                Ok(Box::new(Distinct::new(child, *mode)))
            }
            Plan::GroupBy {
                mode,
                keys,
                aggregates,
                child,
            } => {
                let child = child.build(catalog, ctx)?;
                Ok(Box::new(GroupBy::new(child, *mode, keys, aggregates)?))
            }
            Plan::Union { left, right } => {
                let left = left.build(catalog, ctx)?;
                let right = right.build(catalog, ctx)?;
                Ok(Box::new(Union::new(left, right)?))
            }
            Plan::Intersect { left, right } => {
                let left = left.build(catalog, ctx)?;
                let right = right.build(catalog, ctx)?;
                Ok(Box::new(Intersect::new(left, right)?))
            }
            Plan::Except { left, right } => {
                let left = left.build(catalog, ctx)?;
                let right = right.build(catalog, ctx)?;
                Ok(Box::new(Except::new(left, right)?))
            }
        }
    }
}

impl FromStr for Plan {
    type Err = ExecError;

    /// Parse a plan from its JSON text.
    ///
    /// Errors: [`ExecError::Plan`], carrying serde's message.
    fn from_str(text: &str) -> Result<Plan, ExecError> {
        serde_json::from_str(text).map_err(|error| ExecError::Plan {
            detail: error.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A seqscan reads its `"op"` tag (the variant is renamed to `seqscan`).
    #[test]
    fn parses_seqscan() {
        let plan = r#"{"op":"seqscan","table":"R"}"#.parse::<Plan>().unwrap();
        assert_eq!(
            plan,
            Plan::SeqScan {
                table: "R".to_string(),
                alias: None
            }
        );
    }

    /// A sort key is the two-element array `[column, "asc" | "desc"]`.
    #[test]
    fn parses_sort_keys() {
        let plan = r#"{"op":"sort","keys":[["R.a","asc"],["R.b","desc"]],
                "child":{"op":"seqscan","table":"R"}}"#
            .parse::<Plan>()
            .unwrap();
        let Plan::Sort { keys, .. } = plan else {
            panic!("expected a sort");
        };
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].column(), "R.a");
        assert_eq!(keys[0].direction(), Direction::Asc);
        assert_eq!(keys[1].column(), "R.b");
        assert_eq!(keys[1].direction(), Direction::Desc);
    }

    /// An aggregate is the two-element array `[function, column]`; `count` is
    /// the only function that may take `*`.
    #[test]
    fn parses_aggregates() {
        let plan = r#"{"op":"group_by","mode":"hash","keys":["R.dept"],
                "aggregates":[["count","*"],["avg","R.salary"]],
                "child":{"op":"seqscan","table":"R"}}"#
            .parse::<Plan>()
            .unwrap();
        let Plan::GroupBy { aggregates, .. } = plan else {
            panic!("expected a group_by");
        };
        assert_eq!(
            aggregates,
            vec![
                (Aggregate::Count, "*".to_string()),
                (Aggregate::Avg, "R.salary".to_string())
            ]
        );
    }

    /// An unknown aggregate function is rejected while parsing.
    #[test]
    fn rejects_unknown_aggregate() {
        let err = r#"{"op":"group_by","mode":"sort","keys":[],
                "aggregates":[["median","R.a"]],
                "child":{"op":"seqscan","table":"R"}}"#
            .parse::<Plan>()
            .unwrap_err();
        assert!(err.to_string().contains("median"), "{err}");
    }

    /// Malformed JSON, and trailing characters after a plan, are rejected.
    #[test]
    fn rejects_malformed_json() {
        assert!(r#"{"op":"seqscan","table":"R"}}"#.parse::<Plan>().is_err());
        assert!("not json".parse::<Plan>().is_err());
    }
}
