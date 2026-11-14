//! Rule 1: split conjunctions.

use crate::optimize::error::OptError;
use crate::sql::ast::Condition;
use crate::sql::plan::Plan;
use crate::storage::Catalog;

use super::RewriteRule;
use super::util::take;

/// Rule 1: split `p AND q` into two stacked selects.
pub struct SplitConjunctions;

impl SplitConjunctions {
    fn apply(plan: &mut Plan) -> bool {
        let parts = if let Plan::Select { condition, child } = &mut *plan {
            let child_changed = SplitConjunctions::apply(child);
            let parts = split_conjuncts(condition);
            if parts.len() < 2 {
                return child_changed;
            }
            parts
        } else {
            return plan.map_children(SplitConjunctions::apply);
        };

        let Plan::Select { child, .. } = take(plan) else {
            unreachable!("just matched a select")
        };
        let mut new_plan = *child;
        for part in parts.into_iter().rev() {
            new_plan = Plan::Select {
                condition: part,
                child: Box::new(new_plan),
            };
        }
        *plan = new_plan;
        true
    }
}

impl RewriteRule for SplitConjunctions {
    fn rewrite(&self, plan: &mut Plan, _catalog: &Catalog) -> Result<bool, OptError> {
        Ok(SplitConjunctions::apply(plan))
    }
}

/// The conjuncts of a conjunction, in order.
fn split_conjuncts(condition: &Condition) -> Vec<Condition> {
    match condition {
        Condition::And(a, b) => {
            let mut parts = split_conjuncts(a);
            parts.extend(split_conjuncts(b));
            parts
        }
        _ => vec![condition.clone()],
    }
}
