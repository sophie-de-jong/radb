//! Rule 4: push a select below a rename, union or union_all.
//!
//! A predicate above a `rename` talks about the alias (`e.age`); below the
//! rename it must talk about the name the child produces (`Emp.age`). For a
//! union, the predicate runs on the output names of the left branch, so the
//! right branch's copy is translated position by position.

use std::collections::HashMap;

use crate::optimize::error::OptError;
use crate::sql::plan::Plan;
use crate::storage::Catalog;

use super::RewriteRule;
use super::util::{output_names, take};

/// Rule 4: push a select below a rename, union or union_all.
pub struct PushSelectsThrough;

impl RewriteRule for PushSelectsThrough {
    fn rewrite(&self, plan: &mut Plan, catalog: &Catalog) -> Result<bool, OptError> {
        Ok(push(plan, catalog))
    }
}

/// Push one select through a rename or set union, recursively.
fn push(plan: &mut Plan, catalog: &Catalog) -> bool {
    let Plan::Select { condition, child } = &mut *plan else {
        return plan.map_children(|child| push(child, catalog));
    };
    let child_changed = push(child, catalog);
    let target = match child.as_ref() {
        Plan::Rename { to, child }
            if !matches!(child.as_ref(), Plan::Scan { .. })
                && condition.columns().iter().all(|column| {
                    column
                        .split_once('.')
                        .is_some_and(|(prefix, _)| prefix == to)
                }) =>
        {
            Some(Target::Rename)
        }
        Plan::Union { left, right } | Plan::UnionAll { left, right }
            if union_pushable(condition, left, right, catalog) =>
        {
            Some(Target::Union)
        }
        _ => None,
    };
    let Some(target) = target else {
        return child_changed;
    };

    let Plan::Select { condition, child } = take(plan) else {
        unreachable!("just matched a select")
    };
    match (*child, target) {
        (Plan::Rename { to, child }, Target::Rename) => {
            let mut mapping = HashMap::new();
            for name in output_names(&child, catalog) {
                let short = name.rsplit('.').next().unwrap_or(&name);
                mapping.insert(format!("{to}.{short}"), name);
            }
            let condition = condition.rename_columns(&mapping);
            *plan = Plan::Rename {
                to,
                child: Box::new(Plan::Select { condition, child }),
            };
        }
        (Plan::Union { left, right }, Target::Union) => {
            let right_condition = translate_to_right(&condition, &left, &right, catalog);
            *plan = Plan::Union {
                left: Box::new(Plan::Select {
                    condition,
                    child: left,
                }),
                right: Box::new(Plan::Select {
                    condition: right_condition,
                    child: right,
                }),
            };
        }
        (Plan::UnionAll { left, right }, Target::Union) => {
            let right_condition = translate_to_right(&condition, &left, &right, catalog);
            *plan = Plan::UnionAll {
                left: Box::new(Plan::Select {
                    condition,
                    child: left,
                }),
                right: Box::new(Plan::Select {
                    condition: right_condition,
                    child: right,
                }),
            };
        }
        _ => unreachable!("the target was identified before taking the select"),
    }
    true
}

/// The kind of node a select can be pushed through.
#[derive(Clone, Copy)]
enum Target {
    /// A `rename` whose qualifier every predicate column uses.
    Rename,
    /// A `union`/`union_all` whose left output names the predicate uses.
    Union,
}

/// Rule 4's union precondition: every column the condition names is an output
/// name of the left branch, and both branches have the same arity.
fn union_pushable(
    condition: &crate::Condition,
    left: &Plan,
    right: &Plan,
    catalog: &Catalog,
) -> bool {
    let left_names = output_names(left, catalog);
    let right_names = output_names(right, catalog);
    left_names.len() == right_names.len()
        && condition
            .columns()
            .iter()
            .all(|column| left_names.contains(column))
}

/// Translate a predicate on the union's left output names to the right branch's
/// names at the same positions.
fn translate_to_right(
    condition: &crate::Condition,
    left: &Plan,
    right: &Plan,
    catalog: &Catalog,
) -> crate::Condition {
    let left_names = output_names(left, catalog);
    let right_names = output_names(right, catalog);
    let mapping: HashMap<String, String> = left_names.into_iter().zip(right_names).collect();
    condition.rename_columns(&mapping)
}
