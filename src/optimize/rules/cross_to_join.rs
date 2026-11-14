//! Rule 3: turn a cross plus a two-sided equality select into a join.

use crate::optimize::error::OptError;
use crate::sql::ast::{CompareOp, Condition, Operand};
use crate::sql::plan::Plan;
use crate::storage::Catalog;

use super::RewriteRule;
use super::util::{prefix, take};

/// Rule 3: turn a cross plus a two-sided equality select into a join.
pub struct CrossToJoin;

impl RewriteRule for CrossToJoin {
    fn rewrite(&self, plan: &mut Plan, _catalog: &Catalog) -> Result<bool, OptError> {
        Ok(cross_to_join(plan))
    }
}

fn cross_to_join(plan: &mut Plan) -> bool {
    if let Plan::Select { condition, child } = &mut *plan {
        let child_changed = cross_to_join(child);
        let converted = match child.as_ref() {
            Plan::Cross { left, right } => joins_both_sides(condition, left, right),
            _ => false,
        };
        if !converted {
            return child_changed;
        }
    } else {
        return plan.map_children(cross_to_join);
    }

    let Plan::Select { condition, child } = take(plan) else {
        unreachable!("just matched a select")
    };
    let Plan::Cross { left, right } = *child else {
        unreachable!("the child was checked before taking the plan")
    };
    *plan = Plan::Join {
        condition,
        left,
        right,
    };
    true
}

/// Whether a predicate is an equality joining the two sides of a cross.
fn joins_both_sides(condition: &Condition, left: &Plan, right: &Plan) -> bool {
    let Condition::Compare(compare) = condition else {
        return false;
    };
    if compare.op != CompareOp::Eq {
        return false;
    }
    let (Operand::Column(c1), Operand::Column(c2)) = (&compare.left, &compare.right) else {
        return false;
    };
    let c1 = c1.to_string();
    let c2 = c2.to_string();
    let left_tables = left.tables();
    let right_tables = right.tables();
    let c1_left = prefix(&c1).is_some_and(|t| left_tables.contains(t));
    let c1_right = prefix(&c1).is_some_and(|t| right_tables.contains(t));
    let c2_left = prefix(&c2).is_some_and(|t| left_tables.contains(t));
    let c2_right = prefix(&c2).is_some_and(|t| right_tables.contains(t));
    (c1_left && c2_right) || (c1_right && c2_left)
}
