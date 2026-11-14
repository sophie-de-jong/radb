//! Sort order: the key columns an operator orders by, each with its direction.
//!
//! The engine hands one [`SortOrder`] around — [`ExternalSort`], the merge heap,
//! and every operator that keys an in-memory table by the same columns — rather
//! than a column vector and a parallel vector of `bool`s. A direction is a
//! [`Direction`], whose default is ascending; the plan's `"asc"` / `"desc"`
//! strings deserialize straight into it, so no `bool` represents a direction
//! anywhere in the engine.
//!
//! [`ExternalSort`]: crate::exec::external_sort::ExternalSort

use std::cmp::Ordering;

use crate::schema::Schema;
use crate::value::{Row, Value};

use crate::exec::ExecError;

/// The direction one sort key orders in (§2.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Direction {
    /// Smallest first — the default, so a key that names no direction gets it.
    #[default]
    Asc,
    /// Largest first.
    Desc,
}

impl Direction {
    /// Apply this direction to a comparison: ascending keeps it, descending
    /// reverses it.
    fn apply(self, ord: Ordering) -> Ordering {
        match self {
            Direction::Asc => ord,
            Direction::Desc => ord.reverse(),
        }
    }
}

/// The order a sort imposes: the key columns, as schema indices, each with its
/// direction, most significant first.
///
/// One value carries both, so the engine passes a single `&SortOrder` where it
/// used to pass a key slice and a parallel direction slice. The key columns are
/// still available alone through [`SortOrder::keys`], for the operators that key
/// an in-memory table by the same columns.
#[derive(Debug, Clone)]
pub(crate) struct SortOrder {
    /// The key columns, most significant first.
    keys: Vec<usize>,
    /// The direction of each key, aligned with `keys`.
    directions: Vec<Direction>,
}

impl SortOrder {
    /// Order by `keys`, each with its own direction in `directions`.
    pub(crate) fn new(keys: Vec<usize>, directions: Vec<Direction>) -> Self {
        debug_assert_eq!(keys.len(), directions.len());
        SortOrder { keys, directions }
    }

    /// Order by `keys`, all ascending.
    pub(crate) fn ascending(keys: Vec<usize>) -> Self {
        let directions = vec![Direction::default(); keys.len()];
        SortOrder::new(keys, directions)
    }

    /// Order by every column of `shape`, ascending — the whole-tuple order
    /// `distinct`, `union`, `intersect` and `except` collapse by.
    pub(crate) fn whole(shape: &Schema) -> Self {
        Self::ascending((0..shape.arity()).collect())
    }

    /// The key columns, most significant first.
    pub(crate) fn keys(&self) -> &[usize] {
        &self.keys
    }

    /// Compare two tuples by this order.
    ///
    /// Errors: [`ExecError::Type`] if a tuple is missing a key column, or two
    /// key values are of different kinds.
    pub(crate) fn compare_rows(&self, a: &Row, b: &Row) -> Result<Ordering, ExecError> {
        for (&index, &direction) in self.keys.iter().zip(&self.directions) {
            let (x, y) = match (a.get(index), b.get(index)) {
                (Some(x), Some(y)) => (x, y),
                _ => {
                    return Err(ExecError::Type {
                        detail: format!("tuple is missing key column {}", index + 1),
                    });
                }
            };
            let ord = direction.apply(x.compare(y)?);
            if ord != Ordering::Equal {
                return Ok(ord);
            }
        }
        Ok(Ordering::Equal)
    }

    /// Compare two extracted key vectors position by position.
    ///
    /// A key column has one declared type, so the parts at a position always
    /// share a kind and the total [`Value`] order is never observed across
    /// kinds.
    pub(crate) fn compare_parts(&self, a: &[Value], b: &[Value]) -> Ordering {
        for ((x, y), &direction) in a.iter().zip(b).zip(&self.directions) {
            let ord = direction.apply(x.cmp(y));
            if ord != Ordering::Equal {
                return ord;
            }
        }
        Ordering::Equal
    }
}
