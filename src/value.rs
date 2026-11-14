//! Values and tuples.
//!
//! The value model is deliberately small: a [`Value`] is a signed integer or a
//! string (Project 2's `INT` and `VARCHAR`), and a [`Row`] is an immutable,
//! cheap-to-clone tuple of values.
//!
//! Nothing here touches a file or prints: these types are pure data.

use std::cmp::Ordering;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::schema::SemanticError;

/// A column value: an integer or a string.
///
/// Project 2 has only these two types. Module 2's `AVG` is not a third: §2.6
/// returns it as an `INT` scaled by 10<sup>4</sup> (the value times 10,000), so
/// it keeps `Eq`/`Ord` and never has to hash a float.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Value {
    /// An integer value.
    Int(i32),
    /// A string value.
    Varchar(String),
}

impl Default for Value {
    /// The default value is an integer zero; used only by empty statistics.
    fn default() -> Self {
        Value::Int(0)
    }
}

impl Value {
    /// The type name of this value.
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Value::Int(_) => "INT",
            Value::Varchar(_) => "VARCHAR",
        }
    }

    /// Order `self` against `other`, same type only (§1.3).
    ///
    /// Errors: [`SemanticError::TypeError`] for an int against a string, never
    /// a silent answer (§4.3, case #22).
    pub(crate) fn compare(&self, other: &Value) -> Result<Ordering, SemanticError> {
        match (self, other) {
            (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y)),
            (Value::Varchar(x), Value::Varchar(y)) => Ok(x.as_bytes().cmp(y.as_bytes())),
            (a, b) => Err(SemanticError::TypeError {
                detail: format!("cannot compare {} with {}", a.kind(), b.kind()),
            }),
        }
    }

    /// The value as JSON: a number for `INT`, a string for `VARCHAR`.
    pub fn json(&self) -> serde_json::Value {
        match self {
            Value::Int(value) => serde_json::json!(value),
            Value::Varchar(value) => serde_json::Value::String(value.clone()),
        }
    }
}

impl fmt::Display for Value {
    /// Renders the value as it is written in a relation file.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(i) => write!(f, "{i}"),
            Value::Varchar(s) => write!(f, "{s}"),
        }
    }
}

/// One immutable tuple of [`Value`]s in schema order, cheap to clone and
/// comparable by value.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Row(Arc<[Value]>);

impl Deref for Row {
    type Target = [Value];

    /// The row's values, in schema order.
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<[Value]> for Row {
    /// The row's values, in schema order.
    fn as_ref(&self) -> &[Value] {
        &self.0
    }
}

impl Row {
    /// The two tuples as one, left first.
    pub fn concat(left: &Row, right: &Row) -> Row {
        Row::from(left.iter().chain(right.iter()).cloned().collect::<Vec<_>>())
    }

    /// The key of this row under `keys`: the value of each key column, by index.
    ///
    /// Errors: [`SemanticError::TypeError`] if a key column is not in the row.
    pub(crate) fn key(&self, keys: &[usize]) -> Result<Vec<Value>, SemanticError> {
        keys.iter()
            .map(|&index| {
                self.get(index)
                    .cloned()
                    .ok_or_else(|| SemanticError::TypeError {
                        detail: format!("tuple is missing key column {}", index + 1),
                    })
            })
            .collect()
    }

    pub fn json(&self) -> serde_json::Value {
        serde_json::Value::Array(self.iter().map(Value::json).collect())
    }
}

impl From<Vec<Value>> for Row {
    /// A row holding a copy of `values`.
    fn from(values: Vec<Value>) -> Self {
        Row(Arc::from(values))
    }
}

impl<const N: usize> From<[Value; N]> for Row {
    /// A row holding a copy of `values`.
    fn from(values: [Value; N]) -> Self {
        Row(Arc::from(values))
    }
}

impl From<&[Value]> for Row {
    /// A row holding a copy of `values`.
    fn from(values: &[Value]) -> Self {
        Row(values.iter().cloned().collect())
    }
}

impl FromIterator<Value> for Row {
    /// A row holding a copy of the collected values.
    fn from_iter<T: IntoIterator<Item = Value>>(iter: T) -> Self {
        Row(Arc::from(iter.into_iter().collect::<Vec<_>>()))
    }
}
