//! The logical plan: Module 3's output and Module 4's input (§5).
//!
//! A logical plan is a tree of the operators §5 names — `scan`, `select`,
//! `project`, `join`, `cross`, `rename`, `group_by`, `distinct`, `sort`,
//! `limit`, `union`, `union_all`, `intersect`, `except` — and it never names a
//! physical algorithm. It says `join`, never `hash_join`; choosing the
//! algorithm is Module 4's job.
//!
//! The JSON is exactly the shape §5's worked examples use. A condition is a
//! string in the condition language of Bonus Project 1 ([`Condition`]), with
//! not-equal spelled `<>` as §1 and §2 do. A project column is a plain string
//! when the output name is the input name, and `[input, output]` when the SQL
//! gave an alias. An aggregate is a three-element array
//! `[function, column, output]`.
//!
//! The types are `serde`-round-trippable, so a plan can be written, read back
//! and compared structurally.

use std::collections::HashSet;

use serde::de::Error as _;
use serde::ser::SerializeSeq;
use serde::{Deserialize, Deserializer, Serializer};

use super::ast::Condition;
use crate::exec::Direction;
use crate::exec::plan::Aggregate;

/// A logical plan tree.
#[derive(Debug, Clone, PartialEq, Deserialize, serde::Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Plan {
    /// A heap-file scan. Its output columns are qualified by the table name.
    Scan {
        /// The table name.
        table: String,
    },
    /// Keep the tuples for which the condition holds (`WHERE`/`HAVING`).
    Select {
        /// The predicate.
        condition: Condition,
        /// The input.
        child: Box<Plan>,
    },
    /// Keep the listed columns, in order (`SELECT`).
    Project {
        /// The columns as `(input, output)` pairs.
        #[serde(
            serialize_with = "serialize_project_columns",
            deserialize_with = "deserialize_project_columns"
        )]
        columns: Vec<(String, String)>,
        /// The input.
        child: Box<Plan>,
    },
    /// An equi-join (`JOIN ... ON`).
    Join {
        /// The join predicate.
        condition: Condition,
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
    },
    /// A cross product (a comma in the `FROM` list).
    Cross {
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
    },
    /// Re-qualify every output column (`FROM Emp e`).
    Rename {
        /// The new qualifier.
        to: String,
        /// The input.
        child: Box<Plan>,
    },
    /// Group and aggregate (`GROUP BY`).
    GroupBy {
        /// The grouping columns, fully qualified.
        keys: Vec<String>,
        /// The aggregates, each a `[function, column, output]` triple.
        aggregates: Vec<(Aggregate, String, String)>,
        /// The input.
        child: Box<Plan>,
    },
    /// Remove duplicate tuples (`DISTINCT`, `UNION`, `INTERSECT`, `EXCEPT`).
    Distinct {
        /// The input.
        child: Box<Plan>,
    },
    /// Order the tuples (`ORDER BY`).
    Sort {
        /// The keys, most significant first.
        keys: Vec<(String, Direction)>,
        /// The input.
        child: Box<Plan>,
    },
    /// The first `n` tuples (`LIMIT`).
    Limit {
        /// How many tuples to return.
        n: u64,
        /// The input.
        child: Box<Plan>,
    },
    /// Bag union (`UNION ALL`).
    UnionAll {
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
    },
    /// Set union (`UNION`): `union_all` then duplicate removal.
    Union {
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
    },
    /// Set intersection (`INTERSECT`).
    Intersect {
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
    },
    /// Set difference (`EXCEPT`).
    Except {
        /// The left input.
        left: Box<Plan>,
        /// The right input.
        right: Box<Plan>,
    },
}

impl Plan {
    /// The tables this plan reads, using aliases where it has them.
    pub fn tables(&self) -> HashSet<String> {
        match self {
            Plan::Scan { table } => HashSet::from([table.clone()]),
            Plan::Rename { to, .. } => HashSet::from([to.clone()]),
            Plan::Join { left, right, .. }
            | Plan::Cross { left, right }
            | Plan::Union { left, right }
            | Plan::UnionAll { left, right }
            | Plan::Intersect { left, right }
            | Plan::Except { left, right } => {
                let mut tables = left.tables();
                tables.extend(right.tables());
                tables
            }
            Plan::Select { child, .. }
            | Plan::Project { child, .. }
            | Plan::GroupBy { child, .. }
            | Plan::Distinct { child }
            | Plan::Sort { child, .. }
            | Plan::Limit { child, .. } => child.tables(),
        }
    }

    /// Apply `f` to every child in place, left to right, reporting whether any
    /// child changed. A leaf is returned unchanged.
    pub fn map_children(&mut self, mut f: impl FnMut(&mut Plan) -> bool) -> bool {
        match self {
            Plan::Scan { .. } => false,
            Plan::Select { child, .. } => f(child),
            Plan::Project { child, .. } => f(child),
            Plan::Join { left, right, .. } => f(left) || f(right),
            Plan::Cross { left, right } => f(left) || f(right),
            Plan::Rename { child, .. } => f(child),
            Plan::GroupBy { child, .. } => f(child),
            Plan::Distinct { child } => f(child),
            Plan::Sort { child, .. } => f(child),
            Plan::Limit { child, .. } => f(child),
            Plan::UnionAll { left, right } => f(left) || f(right),
            Plan::Union { left, right } => f(left) || f(right),
            Plan::Intersect { left, right } => f(left) || f(right),
            Plan::Except { left, right } => f(left) || f(right),
        }
    }
}

/// Serialize project columns as strings when the names match, otherwise pairs.
fn serialize_project_columns<S>(
    columns: &[(String, String)],
    serializer: S,
) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let mut seq = serializer.serialize_seq(Some(columns.len()))?;
    for (input, output) in columns {
        if input == output {
            seq.serialize_element(input)?;
        } else {
            seq.serialize_element(&(input, output))?;
        }
    }
    seq.end()
}

/// Read project columns from strings or `[input, output]` pairs.
fn deserialize_project_columns<'de, D>(deserializer: D) -> Result<Vec<(String, String)>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<serde_json::Value>::deserialize(deserializer)?;
    values
        .into_iter()
        .map(|value| match value {
            serde_json::Value::String(name) => Ok((name.clone(), name)),
            serde_json::Value::Array(pair) if pair.len() == 2 => {
                let output = pair[1]
                    .as_str()
                    .ok_or_else(|| D::Error::custom("project output name must be a string"))?;
                let input = pair[0]
                    .as_str()
                    .ok_or_else(|| D::Error::custom("project input name must be a string"))?;
                Ok((input.to_string(), output.to_string()))
            }
            other => Err(D::Error::custom(format!(
                "unexpected project column {other}"
            ))),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::Direction;

    /// The aggregate triple is `[function, column, output]`.
    #[test]
    fn aggregate_triples_serialise_like_the_spec() {
        let spec = (Aggregate::Count, "*".to_string(), "n".to_string());
        assert_eq!(
            serde_json::to_value(&spec).unwrap(),
            serde_json::json!(["count", "*", "n"])
        );
    }

    /// A condition renders in the shared language, and parses back to the same
    /// tree through the Module 2 entry point.
    #[test]
    fn rendered_conditions_reparse_to_the_same_tree() {
        let source = "Emp.a > 30 and not (Emp.b = 'x' or Emp.c != 4)";
        let parsed: Condition = source.parse().unwrap();
        let rendered = parsed.to_string();
        let reparsed: Condition = rendered.parse().unwrap();
        assert_eq!(parsed, reparsed, "rendered: {rendered}");
    }

    /// A not-equal renders as the spec's `<>`.
    #[test]
    fn not_equal_renders_as_angle_brackets() {
        let parsed: Condition = "R.a <> R.b".parse().unwrap();
        assert_eq!(parsed.to_string(), "R.a <> R.b");
    }

    /// Sort keys are `[name, direction]` pairs.
    #[test]
    fn sort_keys_serialise_like_the_spec() {
        let key = ("n".to_string(), Direction::Desc);
        assert_eq!(
            serde_json::to_value(&key).unwrap(),
            serde_json::json!(["n", "desc"])
        );
    }
}
