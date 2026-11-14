//! The rewrite rules (M4 §3), one type per file.

mod util;

pub mod collapse_selects;
pub mod cross_to_join;
pub mod push_projections;
pub mod push_select_below_group_by;
pub mod push_selects;
pub mod push_selects_through;
pub mod split_conjunctions;

use crate::sql::plan::Plan as LogicalPlan;
use crate::storage::Catalog;

use super::error::OptError;

pub use collapse_selects::collapse_selects;
pub use cross_to_join::CrossToJoin;
pub use push_projections::PushProjections;
pub use push_select_below_group_by::PushSelectBelowGroupBy;
pub use push_selects::PushSelects;
pub use push_selects_through::PushSelectsThrough;
pub use split_conjunctions::SplitConjunctions;

/// One logical-plan rewrite.
pub trait RewriteRule {
    /// Rewrite `plan` once. `catalog` is available to rules that need column
    /// names, such as projection pushdown and alias resolution.
    ///
    /// The boolean is true when the rule changed the plan. The optimizer uses it
    /// to stop at a fixed point; rules mutate `plan` in place.
    fn rewrite(&self, plan: &mut LogicalPlan, catalog: &Catalog) -> Result<bool, OptError>;
}

/// The standard rules, in application order.
pub fn default_rules() -> Vec<Box<dyn RewriteRule>> {
    vec![
        Box::new(SplitConjunctions),
        Box::new(PushSelects),
        Box::new(CrossToJoin),
        Box::new(PushSelectsThrough),
        Box::new(PushSelectBelowGroupBy),
        Box::new(PushProjections),
    ]
}
