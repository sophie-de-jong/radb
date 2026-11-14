//! Optimizer errors.

use std::error::Error as StdError;
use std::fmt;
use std::io;

use crate::SqlError;
use crate::schema::SemanticError;
use crate::storage::StorageError;

#[derive(Debug)]
pub enum OptError {
    Plan { detail: String },
    UnknownTable { name: String },
    UnknownColumn { name: String },
    DuplicateColumn { name: String },
    Type { detail: String },
    Arity { detail: String },
    Budget { detail: String },
    Storage(StorageError),
    Io(io::Error),
}

impl fmt::Display for OptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            OptError::Plan { detail } => write!(f, "invalid plan: {detail}"),
            OptError::UnknownTable { name } => write!(f, "unknown table '{name}'"),
            OptError::UnknownColumn { name } => write!(f, "unknown column '{name}'"),
            OptError::DuplicateColumn { name } => {
                write!(f, "schema holds two columns named '{name}'")
            }
            OptError::Type { detail } => write!(f, "type error: {detail}"),
            OptError::Arity { detail } => write!(f, "schema mismatch: {detail}"),
            OptError::Budget { detail } => write!(f, "memory budget: {detail}"),
            OptError::Storage(err) => write!(f, "{err}"),
            OptError::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

impl StdError for OptError {}

impl From<StorageError> for OptError {
    fn from(err: StorageError) -> Self {
        OptError::Storage(err)
    }
}

impl From<io::Error> for OptError {
    fn from(err: io::Error) -> Self {
        OptError::Io(err)
    }
}

impl From<SemanticError> for OptError {
    fn from(err: SemanticError) -> Self {
        match err {
            SemanticError::TypeError { detail } => OptError::Type { detail },
            SemanticError::UnknownAttribute { name } => OptError::UnknownColumn { name },
            SemanticError::DuplicateColumn { name } => OptError::DuplicateColumn { name },
            SemanticError::SchemaMismatch { detail } => OptError::Arity { detail },
        }
    }
}

impl From<SqlError> for OptError {
    fn from(err: SqlError) -> Self {
        OptError::Type {
            detail: format!("condition does not parse: {err}"),
        }
    }
}
