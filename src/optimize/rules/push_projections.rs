//! Rules 6 and 7: push projections down and drop no-op ones.
//!
//! Rule 6 keeps only the columns an operator's ancestors still need. Rule 7
//! drops a projection that drops nothing.

use std::collections::HashSet;

use crate::optimize::error::OptError;
use crate::sql::plan::Plan;
use crate::storage::Catalog;

use super::RewriteRule;
use super::util::{output_names, take};

/// Rules 6 and 7: push projections down and drop no-op ones.
pub struct PushProjections;

impl RewriteRule for PushProjections {
    fn rewrite(&self, plan: &mut Plan, catalog: &Catalog) -> Result<bool, OptError> {
        let needed: HashSet<String> = output_names(plan, catalog).into_iter().collect();
        Ok(push(plan, &needed, catalog))
    }
}

fn all_names(plan: &Plan, catalog: &Catalog) -> HashSet<String> {
    output_names(plan, catalog).into_iter().collect()
}

fn push(plan: &mut Plan, needed: &HashSet<String>, catalog: &Catalog) -> bool {
    let mut changed = match plan {
        // A scan cannot carry a column list, and keeping it a leaf lets the join
        // enumerator still see a base relation.
        Plan::Scan { .. } => false,
        // A rename produces alias-qualified names; project above it if fewer are
        // needed, but never push through to the scan.
        Plan::Rename { .. } => false,
        Plan::Select { condition, child } => {
            let mut child_needed = needed.clone();
            child_needed.extend(condition.columns());
            push(child, &child_needed, catalog)
        }
        Plan::Join {
            condition,
            left,
            right,
        } => {
            let mut both = needed.clone();
            both.extend(condition.columns());
            let left_needed: HashSet<String> = all_names(left, catalog)
                .intersection(&both)
                .cloned()
                .collect();
            let right_needed: HashSet<String> = all_names(right, catalog)
                .intersection(&both)
                .cloned()
                .collect();
            let left_changed = push(left, &left_needed, catalog);
            let right_changed = push(right, &right_needed, catalog);
            left_changed || right_changed
        }
        Plan::Cross { left, right } => {
            let left_needed: HashSet<String> = all_names(left, catalog)
                .intersection(needed)
                .cloned()
                .collect();
            let right_needed: HashSet<String> = all_names(right, catalog)
                .intersection(needed)
                .cloned()
                .collect();
            let left_changed = push(left, &left_needed, catalog);
            let right_changed = push(right, &right_needed, catalog);
            left_changed || right_changed
        }
        Plan::Project { columns, child } => {
            let child_needed: HashSet<String> =
                columns.iter().map(|(input, _)| input.clone()).collect();
            push(child, &child_needed, catalog)
        }
        Plan::GroupBy {
            keys,
            aggregates,
            child,
        } => {
            let mut child_needed: HashSet<String> = keys.iter().cloned().collect();
            for spec in aggregates {
                if spec.1 != "*" {
                    child_needed.insert(spec.1.clone());
                }
            }
            push(child, &child_needed, catalog)
        }
        Plan::Sort { keys, child } => {
            let mut child_needed = needed.clone();
            child_needed.extend(keys.iter().map(|key| key.0.clone()));
            push(child, &child_needed, catalog)
        }
        Plan::Distinct { child } => push(child, needed, catalog),
        Plan::Limit { child, .. } => push(child, needed, catalog),
        // Set operators take the left side's names; do not push a projection
        // across them.
        Plan::Union { left, right } => {
            let l = all_names(left, catalog);
            let r = all_names(right, catalog);
            let left_changed = push(left, &l, catalog);
            let right_changed = push(right, &r, catalog);
            left_changed || right_changed
        }
        Plan::UnionAll { left, right } => {
            let l = all_names(left, catalog);
            let r = all_names(right, catalog);
            let left_changed = push(left, &l, catalog);
            let right_changed = push(right, &r, catalog);
            left_changed || right_changed
        }
        Plan::Intersect { left, right } => {
            let l = all_names(left, catalog);
            let r = all_names(right, catalog);
            let left_changed = push(left, &l, catalog);
            let right_changed = push(right, &r, catalog);
            left_changed || right_changed
        }
        Plan::Except { left, right } => {
            let l = all_names(left, catalog);
            let r = all_names(right, catalog);
            let left_changed = push(left, &l, catalog);
            let right_changed = push(right, &r, catalog);
            left_changed || right_changed
        }
    };

    // Rule 7: a project that keeps exactly what its child already produced.
    let drop_project = if let Plan::Project { columns, child } = &*plan {
        let names: Vec<String> = columns.iter().map(|(_, name)| name.clone()).collect();
        names == output_names(child, catalog)
    } else {
        false
    };
    if drop_project {
        let Plan::Project { child, .. } = take(plan) else {
            unreachable!("just matched a project")
        };
        *plan = *child;
        changed = true;
    }

    // Rule 6: insert a projection above a non-leaf operator that produces more
    // than `needed`.
    let leaf = matches!(&*plan, Plan::Scan { .. } | Plan::Rename { .. });
    let outs = output_names(plan, catalog);
    if !leaf && !needed.is_empty() && needed.len() < outs.len() {
        let columns: Vec<(String, String)> = outs
            .iter()
            .filter(|name| needed.contains(*name))
            .cloned()
            .map(|name| (name.clone(), name))
            .collect();
        if !columns.is_empty() && columns.len() < outs.len() {
            let old = take(plan);
            *plan = Plan::Project {
                columns,
                child: Box::new(old),
            };
            changed = true;
        }
    }
    changed
}
