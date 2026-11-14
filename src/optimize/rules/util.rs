//! Helpers shared by more than one rule.

use crate::sql::plan::Plan;
use crate::storage::Catalog;

/// Temporarily take a plan node, leaving a never-observed placeholder.
pub(super) fn take(plan: &mut Plan) -> Plan {
    std::mem::replace(
        plan,
        Plan::Scan {
            table: String::new(),
        },
    )
}

/// The table part of a qualified column name.
pub(super) fn prefix(name: &str) -> Option<&str> {
    name.split_once('.').map(|(table, _)| table)
}

/// The output column names of a logical plan, fully qualified.
pub(super) fn output_names(plan: &Plan, catalog: &Catalog) -> Vec<String> {
    match plan {
        Plan::Scan { table } => catalog
            .get(table)
            .map(|meta| {
                meta.schema
                    .columns()
                    .iter()
                    .map(|column| format!("{table}.{}", column.name))
                    .collect()
            })
            .unwrap_or_default(),
        Plan::Select { child, .. } => output_names(child, catalog),
        Plan::Project { columns, .. } => columns.iter().map(|(_, name)| name.clone()).collect(),
        Plan::Rename { to, child } => output_names(child, catalog)
            .iter()
            .map(|name| {
                let short = name.rsplit('.').next().unwrap_or(name);
                format!("{to}.{short}")
            })
            .collect(),
        Plan::Join { left, right, .. } | Plan::Cross { left, right } => {
            let mut names = output_names(left, catalog);
            names.extend(output_names(right, catalog));
            names
        }
        Plan::GroupBy {
            keys, aggregates, ..
        } => {
            let mut names = keys.clone();
            names.extend(aggregates.iter().map(|aggregate| aggregate.2.clone()));
            names
        }
        Plan::Distinct { child } | Plan::Sort { child, .. } | Plan::Limit { child, .. } => {
            output_names(child, catalog)
        }
        Plan::Union { left, .. }
        | Plan::UnionAll { left, .. }
        | Plan::Intersect { left, .. }
        | Plan::Except { left, .. } => output_names(left, catalog),
    }
}
