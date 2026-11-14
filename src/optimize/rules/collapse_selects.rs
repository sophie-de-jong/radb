//! Rule 7's second half: collapse adjacent selects.
//!
//! Rule 1 splits a conjunction into stacked selects so the single-table
//! predicates can be pushed; at the fixed point, two selects that ended up
//! adjacent on the same subtree are folded back into one `AND`. This runs once
//! after the rewrite loop, not inside it, so it cannot fight rule 1 forever.

use crate::sql::ast::Condition;
use crate::sql::plan::Plan;

use super::util::take;

/// Collapse every adjacent pair of selects in `plan`.
///
/// Returns whether anything changed; the optimizer repeats until it returns
/// false, which handles a chain of three or more.
pub fn collapse_selects(plan: &mut Plan) -> bool {
    let child_changed = if let Plan::Select { child, .. } = &mut *plan {
        collapse_selects(child)
    } else {
        return plan.map_children(collapse_selects);
    };

    let adjacent = matches!(
        plan,
        Plan::Select { child, .. } if matches!(child.as_ref(), Plan::Select { .. })
    );
    if !adjacent {
        return child_changed;
    }

    let Plan::Select { condition, child } = take(plan) else {
        unreachable!("just matched a select")
    };
    let Plan::Select {
        condition: inner,
        child: grandchild,
    } = *child
    else {
        unreachable!("just matched two adjacent selects")
    };
    *plan = Plan::Select {
        condition: Condition::And(Box::new(condition), Box::new(inner)),
        child: grandchild,
    };
    true
}
