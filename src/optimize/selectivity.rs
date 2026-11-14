//! Selectivity estimation (M4 §2).

use std::collections::HashMap;

use crate::Value;
use crate::sql::ast::{CompareOp, Condition, Operand};
use crate::storage::stats::ColumnStats;

impl ColumnStats {
    /// The selectivity of `column = value`.
    ///
    /// With a histogram, find the bucket holding `value` and use
    /// `count(bucket) / (n_tuples × ndv(bucket))` (M4 §2). Without one, fall
    /// back to `1 / ndv`.
    pub fn selectivity_eq(&self, value: Option<&Value>, n_tuples: u64) -> f64 {
        if let (Some(value), false) = (value, self.buckets.is_empty())
            && n_tuples > 0
        {
            for bucket in &self.buckets {
                if &bucket.min <= value && value <= &bucket.max {
                    if bucket.ndv > 0 {
                        return (bucket.count as f64 / n_tuples as f64) / bucket.ndv as f64;
                    }
                    break;
                }
            }
        }
        if self.ndv == 0 {
            Selectivity::DEFAULT
        } else {
            1.0 / self.ndv as f64
        }
    }

    /// The selectivity of `column < value`.
    pub fn selectivity_lt(&self, value: &Value, n_tuples: u64) -> f64 {
        if n_tuples == 0 {
            return 0.0;
        }
        if self.buckets.is_empty() {
            // No histogram: interpolate over an INT min/max range. Strings
            // have no arithmetic midpoint, so they use a half-range estimate.
            if self.max <= self.min {
                return 0.33;
            }
            if let (Value::Int(value), Value::Int(min), Value::Int(max)) =
                (value, &self.min, &self.max)
            {
                let frac = ((*value as i64) - (*min as i64)) as f64
                    / ((*max as i64) - (*min as i64)) as f64;
                return frac.clamp(0.0, 1.0);
            }
            return if value < &self.min {
                0.0
            } else if value > &self.max {
                1.0
            } else {
                0.5
            };
        }
        let mut sel = 0.0f64;
        for bucket in &self.buckets {
            if &bucket.max < value {
                sel += (bucket.count as f64) / (n_tuples as f64);
            } else if &bucket.min < value && value <= &bucket.max {
                // A linear fraction of the bucket holding `value`; strings use
                // the bucket's midpoint because their range has no arithmetic.
                let frac = match (value, &bucket.min, &bucket.max) {
                    (Value::Int(value), Value::Int(min), Value::Int(max)) if min < max => {
                        ((*value as i64) - (*min as i64)) as f64
                            / ((*max as i64) - (*min as i64)) as f64
                    }
                    _ => 0.5,
                };
                sel += (bucket.count as f64) / (n_tuples as f64) * frac.clamp(0.0, 1.0);
                break;
            } else {
                break;
            }
        }
        sel.clamp(0.0, 1.0)
    }

    /// The selectivity of `column > value`.
    pub fn selectivity_gt(&self, value: &Value, n_tuples: u64) -> f64 {
        1.0 - self.selectivity_lt(value, n_tuples).max(0.0)
    }
}

/// Estimates the fraction of tuples a predicate keeps.
pub struct Selectivity<'a> {
    columns: &'a HashMap<String, ColumnStats>,
    n_tuples: u64,
}

impl<'a> Selectivity<'a> {
    /// The default selectivity of a predicate the formulas cannot classify
    /// (§2: "Any other join predicate — Selectivity 0.1 by default").
    pub const DEFAULT: f64 = 0.1;

    /// Estimates over `columns` for a table of `n_tuples` tuples.
    pub fn new(columns: &'a HashMap<String, ColumnStats>, n_tuples: u64) -> Self {
        Selectivity { columns, n_tuples }
    }

    /// The fraction of tuples `condition` keeps.
    pub fn estimate(&self, condition: &Condition) -> f64 {
        match condition {
            Condition::Compare(compare) => {
                let (column, value) = match (&compare.left, &compare.right) {
                    (Operand::Column(col), Operand::Literal(value)) => {
                        (col.to_string(), value.clone())
                    }
                    (Operand::Literal(value), Operand::Column(col)) => {
                        (col.to_string(), value.clone())
                    }
                    _ => return Self::DEFAULT,
                };
                let stats = self.columns.get(&column);
                match compare.op {
                    CompareOp::Eq => stats.map_or(Self::DEFAULT, |s| {
                        s.selectivity_eq(Some(&value), self.n_tuples)
                    }),
                    CompareOp::Ne => stats.map_or(Self::DEFAULT, |s| {
                        1.0 - s.selectivity_eq(Some(&value), self.n_tuples)
                    }),
                    CompareOp::Lt | CompareOp::Le => {
                        stats.map_or(Self::DEFAULT, |s| s.selectivity_lt(&value, self.n_tuples))
                    }
                    CompareOp::Gt | CompareOp::Ge => {
                        stats.map_or(Self::DEFAULT, |s| s.selectivity_gt(&value, self.n_tuples))
                    }
                }
            }
            Condition::And(a, b) => self.estimate(a) * self.estimate(b),
            Condition::Or(a, b) => {
                let p = self.estimate(a);
                let q = self.estimate(b);
                (p + q - p * q).clamp(0.0, 1.0)
            }
            Condition::Not(inner) => 1.0 - self.estimate(inner),
        }
    }
}
