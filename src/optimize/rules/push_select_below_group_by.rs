//! Rule 5: push a select below a group_by when it references only the keys.

use crate::optimize::error::OptError;
use crate::sql::ast::Condition;
use crate::sql::plan::Plan;
use crate::storage::Catalog;

use super::RewriteRule;
use super::util::take;

/// Rule 5: push a select that references only grouping columns below a
/// `group_by`.
pub struct PushSelectBelowGroupBy;

impl RewriteRule for PushSelectBelowGroupBy {
    fn rewrite(&self, plan: &mut Plan, _catalog: &Catalog) -> Result<bool, OptError> {
        Ok(push_below_group_by(plan))
    }
}

fn push_below_group_by(plan: &mut Plan) -> bool {
    if let Plan::Select { condition, child } = &mut *plan {
        let child_changed = push_below_group_by(child);
        let push = matches!(
            child.as_ref(),
            Plan::GroupBy { keys, .. } if references_only(condition, keys)
        );
        if !push {
            return child_changed;
        }
    } else {
        return plan.map_children(push_below_group_by);
    }

    let Plan::Select { condition, child } = take(plan) else {
        unreachable!("just matched a select")
    };
    let Plan::GroupBy {
        keys,
        aggregates,
        child: group_child,
    } = *child
    else {
        unreachable!("the child was checked before taking the plan")
    };
    *plan = Plan::GroupBy {
        keys,
        aggregates,
        child: Box::new(Plan::Select {
            condition,
            child: group_child,
        }),
    };
    true
}

/// Whether a predicate references only `keys`.
fn references_only(condition: &Condition, keys: &[String]) -> bool {
    let columns = condition.columns();
    !columns.is_empty() && columns.iter().all(|column| keys.contains(column))
}
