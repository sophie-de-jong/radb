//! Schemas: a relation's ordered columns, each a name and a type.
//!
//! One [`Schema`] serves the whole system. Module 1 stores one in the catalog
//! and walks it in the record codec (§2 of the M1 spec); Module 2's operators
//! carry one as their tuple shape (§1.3 of the M2 spec). There is no separate
//! "logical" and "physical" schema: a column is a [`Column`], a name plus a
//! [`ColumnType`], and a name is matched exactly, so a qualified name such as
//! `R.a` only ever matches `R.a`.
//!
//! [`SemanticError`] lives here because every schema-derived problem (an
//! unknown column, a duplicate column, two incompatible inputs) is reported
//! through it, and both the value model and the compiled conditions need to
//! name it.

use std::collections::HashSet;
use std::error::Error as StdError;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::storage::StorageError;

/// Why a schema-level operation failed.
#[derive(Debug, Clone, PartialEq)]
pub enum SemanticError {
    /// Two input schemas cannot be combined: different arity, or a column of a
    /// different type at the same position.
    SchemaMismatch { detail: String },
    /// A comparison crossed types, e.g. an int against a string.
    TypeError { detail: String },
    /// No column in scope is named `name`.
    UnknownAttribute { name: String },
    /// One schema lists `name` twice, which makes it impossible to address.
    DuplicateColumn { name: String },
}

impl fmt::Display for SemanticError {
    /// Renders the error as a one-line message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SemanticError::SchemaMismatch { detail } => write!(f, "schema mismatch: {detail}"),
            SemanticError::TypeError { detail } => write!(f, "type error: {detail}"),
            SemanticError::UnknownAttribute { name } => write!(f, "unknown attribute '{name}'"),
            SemanticError::DuplicateColumn { name } => write!(f, "duplicate column '{name}'"),
        }
    }
}
impl StdError for SemanticError {}

/// A column's type. `INT` is always 4 bytes; `VARCHAR(n)` carries its declared
/// maximum length in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ColumnType {
    /// A signed 32-bit integer, encoded little-endian, two's complement.
    Int,
    /// A UTF-8 string of at most `n` bytes.
    Varchar(u8),
}

impl ColumnType {
    pub fn type_compatible(self, other: ColumnType) -> bool {
        match (self, other) {
            (ColumnType::Int, ColumnType::Int) => true,
            (ColumnType::Varchar(_), ColumnType::Varchar(_)) => true,
            (_, _) => false,
        }
    }
}

impl fmt::Display for ColumnType {
    /// Renders the type as it is written in a schema: `INT` or `VARCHAR(n)`.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ColumnType::Int => write!(f, "INT"),
            ColumnType::Varchar(n) => write!(f, "VARCHAR({n})"),
        }
    }
}

impl FromStr for ColumnType {
    type Err = StorageError;

    /// Parse a column type: `INT` or `VARCHAR(n)`, `1 <= n <= 255`.
    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let upper = text.to_ascii_uppercase();
        if upper == "INT" {
            return Ok(ColumnType::Int);
        }
        let inner = upper
            .strip_prefix("VARCHAR(")
            .and_then(|rest| rest.strip_suffix(')'))
            .ok_or_else(|| StorageError::BadType {
                detail: format!("'{text}' is not INT or VARCHAR(n)"),
            })?;
        let n: u64 = inner.trim().parse().map_err(|_| StorageError::BadType {
            detail: format!("'{text}' has a non-numeric length"),
        })?;
        if n == 0 || n > u8::MAX as u64 {
            return Err(StorageError::BadType {
                detail: format!("VARCHAR length {n} is outside 1..={}", u8::MAX),
            });
        }
        Ok(ColumnType::Varchar(n as u8))
    }
}

/// One column: a name and a type.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Column {
    /// The column name, for example `a` or, qualified, `R.a`.
    pub name: String,
    /// The column type.
    pub ty: ColumnType,
}

/// Ordered columns: a table's stored shape, or an operator's output shape.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Schema {
    columns: Vec<Column>,
}

impl Schema {
    /// A schema from `columns`.
    ///
    /// Errors: [`SemanticError::DuplicateColumn`] if two columns share a name.
    pub fn new(columns: Vec<Column>) -> Result<Self, SemanticError> {
        let mut seen = HashSet::new();
        for column in columns.iter() {
            if !seen.insert(column.name.as_str()) {
                return Err(SemanticError::DuplicateColumn {
                    name: column.name.clone(),
                });
            }
        }
        Ok(Schema { columns })
    }

    /// The columns, in order.
    pub fn columns(&self) -> &[Column] {
        &self.columns
    }

    /// The number of columns.
    pub fn arity(&self) -> usize {
        self.columns.len()
    }

    /// The column types, in order.
    pub fn column_types(&self) -> Vec<ColumnType> {
        self.columns.iter().map(|c| c.ty).collect()
    }

    /// The column names, in order.
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.columns.iter().map(|c| c.name.as_str())
    }

    pub fn qualify(&mut self, qualifier: &str) {
        for column in self.columns.iter_mut() {
            column.name = format!("{qualifier}.{}", column.name)
        }
    }

    /// The column `name` denotes, as an index.
    ///
    /// A name equals a column name exactly: there is no rule for a bare name to
    /// match a qualified one, so `DID` does not find `Emp.DID`.
    ///
    /// Errors: [`SemanticError::UnknownAttribute`] if no column has that name.
    pub fn resolve(&self, name: &str) -> Result<usize, SemanticError> {
        self.columns
            .iter()
            .position(|c| c.name == name)
            .ok_or_else(|| SemanticError::UnknownAttribute {
                name: name.to_string(),
            })
    }

    /// The schema of a two-input operator: the left columns followed by the
    /// right columns, unchanged (§1.3 of the M2 spec).
    ///
    /// Errors: [`SemanticError::DuplicateColumn`] if the two sides contribute
    /// the same name.
    pub fn join(&self, right: &Schema) -> Result<Self, SemanticError> {
        let columns = self
            .columns
            .iter()
            .chain(right.columns.iter())
            .cloned()
            .collect();
        Schema::new(columns)
    }

    /// The output schema of a projection: the named columns, in listed order.
    ///
    /// Errors: [`SemanticError::UnknownAttribute`] for a name not in `self`, or
    /// [`SemanticError::DuplicateColumn`] if a name is listed twice.
    pub fn project(&self, columns: &[String]) -> Result<Self, SemanticError> {
        let mut out = Vec::with_capacity(columns.len());
        for name in columns {
            out.push(self.columns[self.resolve(name)?].clone());
        }
        Schema::new(out)
    }

    /// Check that `other` has the same number of columns with the same types,
    /// position by position (§2.10 of the M2 spec). Names may differ.
    ///
    /// Errors: [`SemanticError::SchemaMismatch`] naming the first difference.
    pub fn same_shape(&self, other: &Schema) -> Result<(), SemanticError> {
        if self.arity() != other.arity() {
            return Err(SemanticError::SchemaMismatch {
                detail: format!("inputs have {} and {} columns", self.arity(), other.arity()),
            });
        }
        for (i, (a, b)) in self.columns.iter().zip(other.columns.iter()).enumerate() {
            if a.ty != b.ty {
                return Err(SemanticError::SchemaMismatch {
                    detail: format!(
                        "column {} is {} on the left and {} on the right",
                        i + 1,
                        a.ty,
                        b.ty
                    ),
                });
            }
        }
        Ok(())
    }

    /// The `run --json` schema spelling: `[["R.a","INT"],["S.c","VARCHAR",20]]`.
    pub fn json(&self) -> serde_json::Value {
        serde_json::Value::Array(
            self.columns
                .iter()
                .map(|c| match c.ty {
                    ColumnType::Int => serde_json::json!([c.name, "INT"]),
                    ColumnType::Varchar(n) => serde_json::json!([c.name, "VARCHAR", n]),
                })
                .collect(),
        )
    }
}

impl Serialize for Schema {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.columns.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Schema {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let columns = Vec::<Column>::deserialize(deserializer)?;
        Schema::new(columns).map_err(serde::de::Error::custom)
    }
}

impl FromStr for Schema {
    type Err = StorageError;

    /// Parse the `--schema` spelling: comma-separated `name:TYPE` pairs, where
    /// TYPE is `INT` or `VARCHAR(n)`, `1 <= n <= 255`.
    ///
    /// Errors: [`StorageError::BadSchema`] for a malformed pair, a missing or
    /// repeated name, or an unknown type.
    fn from_str(spec: &str) -> Result<Self, Self::Err> {
        let mut columns = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for part in spec.split(',') {
            let part = part.trim();
            if part.is_empty() {
                return Err(StorageError::BadSchema {
                    detail: format!("empty column in '{spec}'"),
                });
            }
            let (name, ty) = part
                .split_once(':')
                .ok_or_else(|| StorageError::BadSchema {
                    detail: format!("column '{part}' is missing ':'"),
                })?;
            let name = name.trim();
            if name.is_empty() {
                return Err(StorageError::BadSchema {
                    detail: format!("column '{part}' has no name"),
                });
            }
            if !seen.insert(name.to_string()) {
                return Err(StorageError::BadSchema {
                    detail: format!("duplicate column name '{name}'"),
                });
            }
            let ty = ty.trim().parse::<ColumnType>()?;
            columns.push(Column {
                name: name.to_string(),
                ty,
            });
        }
        if columns.is_empty() {
            return Err(StorageError::BadSchema {
                detail: "a table must have at least one column".to_string(),
            });
        }
        Schema::new(columns).map_err(|err| StorageError::BadSchema {
            detail: err.to_string(),
        })
    }
}
