//! `project`: keep the listed columns, in order (§2.3).
//!
//! Fully pipelined: it holds at most one frame and copies the selected columns
//! of each tuple as it passes. Duplicates in the projection are kept.

use crate::schema::Schema;
use crate::value::{Row, Value};

use crate::exec::{Context, Counters, ExecError, Operator};

/// `project`: keep the listed columns, in order, duplicates and all.
pub struct Project {
    /// The input stream.
    child: Box<dyn Operator>,
    /// Child column indices to keep, in output order (duplicates kept).
    indices: Vec<usize>,
    /// The output shape.
    schema: Schema,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl Project {
    /// A projection of `child` onto the named `columns`, in order; duplicates
    /// are kept. The output shape takes the child's column names.
    ///
    /// Errors: [`ExecError::UnknownColumn`] for a name the child has no column
    /// for.
    pub fn new(child: Box<dyn Operator>, columns: &[String]) -> Result<Self, ExecError> {
        let schema = child.schema().project(columns)?;
        let indices = columns
            .iter()
            .map(|column| child.schema().resolve(column))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Project {
            child,
            indices,
            schema,
            counters: Counters::default(),
        })
    }
}

impl Operator for Project {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.child.open(ctx)
    }

    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        match self.child.next(ctx)? {
            Some(row) => {
                let values: Vec<Value> = self.indices.iter().map(|&i| row[i].clone()).collect();
                self.counters.tuples_out += 1;
                Ok(Some(Row::from(values)))
            }
            None => Ok(None),
        }
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.child.close(ctx)
    }

    fn counters(&self) -> Counters {
        self.counters
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![&*self.child]
    }
}

impl std::fmt::Display for Project {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("project")
    }
}
