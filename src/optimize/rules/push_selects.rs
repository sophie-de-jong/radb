//! Rule 2: push single-side selects below a join or cross.

use crate::optimize::error::OptError;
use crate::sql::ast::Condition;
use crate::sql::plan::Plan;
use crate::storage::Catalog;

use super::RewriteRule;
use super::util::{prefix, take};

/// Rule 2: push a single-side select below a join or cross.
pub struct PushSelects;

impl RewriteRule for PushSelects {
    fn rewrite(&self, plan: &mut Plan, _catalog: &Catalog) -> Result<bool, OptError> {
        Ok(push_selects(plan))
    }
}

fn push_selects(plan: &mut Plan) -> bool {
    if let Plan::Select { condition, child } = &mut *plan {
        let child_changed = push_selects(child);
        let moved = match child.as_ref() {
            Plan::Join { left, right, .. } => side(condition, left, right).is_some(),
            Plan::Cross { left, right } => side(condition, left, right).is_some(),
            _ => false,
        };
        if !moved {
            return child_changed;
        }
    } else {
        return plan.map_children(push_selects);
    }

    let Plan::Select { condition, child } = take(plan) else {
        unreachable!("just matched a select")
    };
    let new_plan = match *child {
        Plan::Join {
            condition: join,
            left,
            right,
        } => match side(&condition, &left, &right) {
            Some(JoinSide::Left) => Plan::Join {
                condition: join,
                left: Box::new(Plan::Select {
                    condition,
                    child: left,
                }),
                right,
            },
            Some(JoinSide::Right) => Plan::Join {
                condition: join,
                left,
                right: Box::new(Plan::Select {
                    condition,
                    child: right,
                }),
            },
            None => unreachable!("the side was checked before taking the plan"),
        },
        Plan::Cross { left, right } => match side(&condition, &left, &right) {
            Some(JoinSide::Left) => Plan::Cross {
                left: Box::new(Plan::Select {
                    condition,
                    child: left,
                }),
                right,
            },
            Some(JoinSide::Right) => Plan::Cross {
                left,
                right: Box::new(Plan::Select {
                    condition,
                    child: right,
                }),
            },
            None => unreachable!("the side was checked before taking the plan"),
        },
        _ => unreachable!("the child was checked before taking the plan"),
    };
    *plan = new_plan;
    true
}

/// Which side of a join a predicate references.
enum JoinSide {
    Left,
    Right,
}

/// The side a predicate references, if it references exactly one.
fn side(condition: &Condition, left: &Plan, right: &Plan) -> Option<JoinSide> {
    let columns = condition.columns();
    if columns.is_empty() {
        return None;
    }
    let left_tables = left.tables();
    let right_tables = right.tables();
    if columns
        .iter()
        .all(|c| prefix(c).is_some_and(|t| left_tables.contains(t)))
    {
        Some(JoinSide::Left)
    } else if columns
        .iter()
        .all(|c| prefix(c).is_some_and(|t| right_tables.contains(t)))
    {
        Some(JoinSide::Right)
    } else {
        None
    }
}
