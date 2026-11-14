//! Every way the execution engine can fail.
//!
//! Module 2 is graded on its errors (§4 of the module specification): a plan
//! that does not conform must be rejected with the problem, and a plan naming
//! an unknown table or column must be rejected at open, before any work
//! happens. Each failure mode therefore gets its own variant and the `Display`
//! text names the table or column.
//!
//! Nothing here prints; the command-line tool is what turns an error into
//! output.

use std::error::Error as StdError;
use std::fmt;
use std::io;

use crate::SqlError;
use crate::schema::SemanticError;
use crate::storage::StorageError;

/// A failure in the execution engine.
#[derive(Debug)]
pub enum ExecError {
    /// The plan JSON does not conform to §2.
    Plan {
        /// What is wrong.
        detail: String,
    },

    /// A plan referred to a table the catalog does not hold.
    UnknownTable {
        /// The table name.
        name: String,
    },

    /// A plan or condition referred to a column the schema does not hold.
    UnknownColumn {
        /// The column name.
        name: String,
    },

    /// A schema would hold two columns with the same qualified name (§1.3).
    DuplicateColumn {
        /// The repeated name.
        name: String,
    },

    /// A type rule was broken: an int against a varchar (§1.3), an aggregate
    /// over the wrong type, or a join key that is not an equality.
    Type {
        /// What is wrong.
        detail: String,
    },

    /// Two inputs of an operator do not have the same shape (set operators,
    /// §2.10).
    Arity {
        /// What is wrong.
        detail: String,
    },

    /// An operator would have held more frames than the budget allows (§1.2).
    Budget {
        /// What is wrong.
        detail: String,
    },

    /// The storage engine refused an operation.
    Storage(StorageError),

    /// The operating system refused a file operation.
    Io(io::Error),
}

impl fmt::Display for ExecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExecError::Plan { detail } => write!(f, "invalid plan: {detail}"),
            ExecError::UnknownTable { name } => write!(f, "unknown table '{name}'"),
            ExecError::UnknownColumn { name } => write!(f, "unknown column '{name}'"),
            ExecError::DuplicateColumn { name } => {
                write!(f, "schema holds two columns named '{name}'")
            }
            ExecError::Type { detail } => write!(f, "type error: {detail}"),
            ExecError::Arity { detail } => write!(f, "schema mismatch: {detail}"),
            ExecError::Budget { detail } => write!(f, "memory budget: {detail}"),
            ExecError::Storage(err) => write!(f, "{err}"),
            ExecError::Io(err) => write!(f, "io error: {err}"),
        }
    }
}

impl StdError for ExecError {}

impl From<StorageError> for ExecError {
    fn from(err: StorageError) -> Self {
        ExecError::Storage(err)
    }
}

impl From<io::Error> for ExecError {
    fn from(err: io::Error) -> Self {
        ExecError::Io(err)
    }
}

impl From<SemanticError> for ExecError {
    /// A schema-level error from resolving a column, joining two schemas, or
    /// evaluating a condition. Each variant keeps its own message: an unknown
    /// column stays an `UnknownColumn`, not a vague type error.
    fn from(err: SemanticError) -> Self {
        match err {
            SemanticError::TypeError { detail } => ExecError::Type { detail },
            SemanticError::UnknownAttribute { name } => ExecError::UnknownColumn { name },
            SemanticError::DuplicateColumn { name } => ExecError::DuplicateColumn { name },
            SemanticError::SchemaMismatch { detail } => ExecError::Arity { detail },
        }
    }
}

impl From<SqlError> for ExecError {
    /// A condition string that does not parse. The plan cannot say where, so it
    /// is reported as a type error carrying the parser's message.
    fn from(err: SqlError) -> Self {
        ExecError::Type {
            detail: format!("condition does not parse: {err}"),
        }
    }
}
