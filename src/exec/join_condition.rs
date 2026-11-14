//! The compiled condition the join operators share.
//!
//! A hash or sort-merge join can only serve a conjunction of equalities, one
//! column of each input. [`JoinCondition`] compiles such a condition once and
//! then evaluates it against tuples, in the same spirit as
//! [`FilterCondition`](crate::exec::operators::filter::FilterCondition). It also
//! exposes the key column pairs, which the hash join partitions on and the
//! sort-merge join sorts on.

use std::cmp::Ordering;

use crate::schema::{Schema, SemanticError};
use crate::sql::ast::{CompareOp, Condition, Operand};
use crate::value::{Row, Value};

/// A compiled equi-join condition: the `(left, right)` key column pairs.
#[derive(Debug, Clone)]
pub struct JoinCondition {
    keys: Vec<(usize, usize)>,
}

impl JoinCondition {
    /// Compile `condition` into the equality key pairs of a join of `left` and
    /// `right`.
    ///
    /// Errors: [`SemanticError::TypeError`] if a conjunct is not an equality of
    /// one column of each input with matching types, and
    /// [`SemanticError::UnknownAttribute`] for a column of neither input.
    pub fn compile(
        condition: &Condition,
        left: &Schema,
        right: &Schema,
    ) -> Result<Self, SemanticError> {
        let mut keys = Vec::new();
        for conjunct in Self::conjuncts(condition) {
            let Condition::Compare(compare) = conjunct else {
                return Err(SemanticError::TypeError {
                    detail: "join keys must be equalities (a conjunction of L.c = R.d)".to_string(),
                });
            };
            if compare.op != CompareOp::Eq {
                return Err(SemanticError::TypeError {
                    detail: "join keys must be equalities (a conjunction of L.c = R.d)".to_string(),
                });
            }
            let (l, r) = match (
                Self::side(&compare.left, left, right)?,
                Self::side(&compare.right, left, right)?,
            ) {
                (Side::Left(a), Side::Right(b)) | (Side::Right(b), Side::Left(a)) => (a, b),
                _ => {
                    return Err(SemanticError::TypeError {
                        detail: "each join key must compare one column of each input".to_string(),
                    });
                }
            };
            if left.columns()[l].ty != right.columns()[r].ty {
                return Err(SemanticError::TypeError {
                    detail: format!(
                        "join key compares {} ({} on the left) with {} ({} on the right)",
                        left.columns()[l].name,
                        left.columns()[l].ty,
                        right.columns()[r].name,
                        right.columns()[r].ty,
                    ),
                });
            }
            keys.push((l, r));
        }
        if keys.is_empty() {
            return Err(SemanticError::TypeError {
                detail: "a join needs at least one equality key".to_string(),
            });
        }
        Ok(JoinCondition { keys })
    }

    /// The `(left, right)` key column pairs.
    pub fn keys(&self) -> &[(usize, usize)] {
        &self.keys
    }

    /// The left tuple's key values, in key order.
    pub fn left_key(&self, row: &Row) -> Result<Vec<Value>, SemanticError> {
        self.keys
            .iter()
            .map(|&(left, _)| column(row, left).cloned())
            .collect()
    }

    /// The right tuple's key values, in key order.
    pub fn right_key(&self, row: &Row) -> Result<Vec<Value>, SemanticError> {
        self.keys
            .iter()
            .map(|&(_, right)| column(row, right).cloned())
            .collect()
    }

    /// Whether two tuples satisfy the condition: every key pair is equal.
    pub fn eval(&self, left: &Row, right: &Row) -> Result<bool, SemanticError> {
        for &(l, r) in &self.keys {
            if column(left, l)? != column(right, r)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// Compare two tuples lexicographically by the join keys.
    pub fn compare(&self, left: &Row, right: &Row) -> Result<Ordering, SemanticError> {
        for &(l, r) in &self.keys {
            let ord = column(left, l)?.compare(column(right, r)?)?;
            if ord != Ordering::Equal {
                return Ok(ord);
            }
        }
        Ok(Ordering::Equal)
    }

    /// The conjuncts of a conjunction, in order.
    fn conjuncts(condition: &Condition) -> Vec<&Condition> {
        match condition {
            Condition::And(a, b) => {
                let mut out = Self::conjuncts(a);
                out.extend(Self::conjuncts(b));
                out
            }
            _ => vec![condition],
        }
    }

    /// Which input the operand names a column of.
    ///
    /// The engine's schemas are already qualified — a `seqscan` of `R` yields
    /// `R.a`, `R.b`, … — so a name matches exactly, left searched first.
    fn side(operand: &Operand, left: &Schema, right: &Schema) -> Result<Side, SemanticError> {
        let name = match operand {
            Operand::Column(col) => match &col.qualifier {
                Some(q) => format!("{}.{}", q, col.name),
                None => col.name.clone(),
            },
            _ => {
                return Err(SemanticError::TypeError {
                    detail: "each join key must compare one column of each input".to_string(),
                });
            }
        };
        if let Ok(i) = left.resolve(&name) {
            return Ok(Side::Left(i));
        }
        match right.resolve(&name) {
            Ok(i) => Ok(Side::Right(i)),
            Err(_) => Err(SemanticError::UnknownAttribute { name }),
        }
    }
}

/// One column of a tuple.
fn column(row: &Row, index: usize) -> Result<&Value, SemanticError> {
    row.get(index).ok_or_else(|| SemanticError::TypeError {
        detail: format!("a join row is missing column {}", index + 1),
    })
}

/// One side of an equi-join key conjunct.
enum Side {
    /// A column of the left input, by index.
    Left(usize),
    /// A column of the right input, by index.
    Right(usize),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::{Column, ColumnType};

    fn condition(source: &str) -> crate::Condition {
        source.parse().expect("valid condition")
    }

    fn schema(names: &[(&str, ColumnType)]) -> Schema {
        Schema::new(
            names
                .iter()
                .map(|(name, ty)| Column {
                    name: (*name).to_string(),
                    ty: *ty,
                })
                .collect(),
        )
        .unwrap()
    }

    fn join_condition() -> JoinCondition {
        let left = schema(&[("R.b", ColumnType::Int), ("R.c", ColumnType::Int)]);
        let right = schema(&[("S.c", ColumnType::Int), ("S.b", ColumnType::Int)]);
        JoinCondition::compile(&condition("R.b = S.b and S.c = R.c"), &left, &right).unwrap()
    }

    #[test]
    fn compiles_the_key_pairs() {
        assert_eq!(join_condition().keys(), &[(0, 1), (1, 0)]);
    }

    #[test]
    fn evaluates_and_compares_the_keys() {
        let condition = join_condition();
        let left = Row::from(vec![Value::Int(7), Value::Int(9)]);
        let same = Row::from(vec![Value::Int(9), Value::Int(7)]);
        let less = Row::from(vec![Value::Int(9), Value::Int(8)]);
        let greater = Row::from(vec![Value::Int(9), Value::Int(6)]);
        assert!(condition.eval(&left, &same).unwrap());
        assert!(!condition.eval(&left, &less).unwrap());
        assert_eq!(condition.compare(&left, &same).unwrap(), Ordering::Equal);
        assert_eq!(condition.compare(&left, &less).unwrap(), Ordering::Less);
        assert_eq!(
            condition.compare(&left, &greater).unwrap(),
            Ordering::Greater
        );
    }

    /// A join key comparing an int column with a string column is a type error.
    #[test]
    fn comparing_int_to_str_is_a_type_error() {
        let left = schema(&[("R.b", ColumnType::Int)]);
        let right = schema(&[("S.b", ColumnType::Varchar(5))]);
        match JoinCondition::compile(&condition("R.b = S.b"), &left, &right) {
            Err(SemanticError::TypeError { detail }) => assert_eq!(
                detail,
                "join key compares R.b (INT on the left) with S.b (VARCHAR(5) on the right)"
            ),
            other => panic!("expected TypeError, got {other:?}"),
        }
    }
}
