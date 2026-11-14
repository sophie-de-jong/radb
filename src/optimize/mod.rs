//! Module 4: the query optimizer.
//!
//! [`Optimizer`] applies an ordered set of [`RewriteRule`]s to a logical plan
//! and converts the result to a physical plan. The rules themselves live in
//! [`rules`]; statistics (owned by the catalog in [`crate::storage::stats`]),
//! selectivity, costing and join enumeration are the supporting pieces.

pub mod analyze;
pub mod cost;
pub mod dp;
pub mod error;
pub mod explain;
pub mod optimizer;
pub mod rules;
pub mod selectivity;

pub use explain::{explain, render_dp_tables};
pub use optimizer::{DEFAULT_PASSES, Optimizer};
pub use rules::RewriteRule;
