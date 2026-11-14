//! `filter`: keep the tuples for which the condition holds (§2.2).
//!
//! Pipelined: it holds no frame and never touches the disk. It counts one
//! comparison per tuple examined, not per comparison operator inside a compound
//! condition (§2.2).
//!
//! The condition is compiled into **DNF (Disjunctive Normal Form)**: a list of
//! disjuncts, where each disjunct is a list of conjuncts (AND clauses).
//! Evaluation: a tuple passes if ANY disjunct is fully satisfied (all its
//! conjuncts true). This is a fast loop with short-circuiting.

use crate::exec::{Context, Counters, ExecError, Operator};
use crate::schema::{ColumnType, Schema, SemanticError};
use crate::sql::ast::{CompareOp, Condition, Operand};
use crate::value::{Row, Value};

/// `filter`: keep the tuples for which the condition holds.
pub struct Filter {
    /// The input stream.
    child: Box<dyn Operator>,
    /// The compiled condition each tuple must satisfy to pass.
    condition: FilterCondition,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl Filter {
    /// A filter of `child` by `condition`, compiled against its schema.
    ///
    /// Errors: [`ExecError::UnknownColumn`] or [`ExecError::Type`] for a
    /// condition the input cannot satisfy.
    pub fn new(child: Box<dyn Operator>, condition: &Condition) -> Result<Self, ExecError> {
        let condition =
            FilterCondition::compile(condition, child.schema()).map_err(|e| ExecError::Plan {
                detail: e.to_string(),
            })?;
        Ok(Filter {
            child,
            condition,
            counters: Counters::default(),
        })
    }
}

impl Operator for Filter {
    fn schema(&self) -> &Schema {
        self.child.schema()
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.child.open(ctx)
    }

    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        while let Some(row) = self.child.next(ctx)? {
            self.counters.comparisons += 1;
            if self.condition.eval(&row)? {
                self.counters.tuples_out += 1;
                return Ok(Some(row));
            }
        }
        Ok(None)
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

impl std::fmt::Display for Filter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("filter")
    }
}

/// One side of a compiled comparison: a column by index, or a constant.
#[derive(Debug, Clone)]
enum ValueOrCol {
    /// A constant value.
    Value(Value),
    /// A column of the filtered tuple, by index.
    Col(usize),
}

impl ValueOrCol {
    /// Resolve one operand against `schema`.
    ///
    /// Errors: [`SemanticError::UnknownAttribute`] if the operand names a
    /// column the schema does not have.
    fn new(operand: &Operand, schema: &Schema) -> Result<Self, SemanticError> {
        match operand {
            Operand::Literal(value) => Ok(ValueOrCol::Value(value.clone())),
            Operand::Column(col) => {
                let full_name = match &col.qualifier {
                    Some(q) => format!("{}.{}", q, col.name),
                    None => col.name.clone(),
                };
                let index = schema.resolve(&full_name)?;
                Ok(ValueOrCol::Col(index))
            }
            Operand::Aggregate(_) => Err(SemanticError::TypeError {
                detail: "aggregates not supported in filter conditions".to_string(),
            }),
        }
    }

    /// The comparison kind of this operand.
    fn as_type(&self, schema: &Schema) -> ColumnType {
        match self {
            ValueOrCol::Value(Value::Int(_)) => ColumnType::Int,
            ValueOrCol::Value(Value::Varchar(s)) => ColumnType::Varchar(s.len() as u8),
            ValueOrCol::Col(index) => schema.columns()[*index].ty,
        }
    }

    /// The operand's value for `row`.
    fn as_value<'a>(&'a self, row: &'a [Value]) -> &'a Value {
        match self {
            ValueOrCol::Col(i) => &row[*i],
            ValueOrCol::Value(v) => v,
        }
    }
}

/// A single comparison conjunct in a filter condition.
#[derive(Debug, Clone)]
struct Conjunct {
    left: ValueOrCol,
    right: ValueOrCol,
    op: CompareOp,
}

impl Conjunct {
    /// Compile a single comparison into a conjunct.
    ///
    /// Errors: [`SemanticError::TypeError`] if the operands are of different
    /// kinds — an `INT` against a `VARCHAR` is an error, equality included
    /// (§1.3).
    fn new(
        left: &Operand,
        right: &Operand,
        op: CompareOp,
        schema: &Schema,
    ) -> Result<Self, SemanticError> {
        let left = ValueOrCol::new(left, schema)?;
        let right = ValueOrCol::new(right, schema)?;
        let left_type = left.as_type(schema);
        let right_type = right.as_type(schema);
        if !left_type.type_compatible(right_type) {
            return Err(SemanticError::TypeError {
                detail: format!("cannot compare {left_type} with {right_type}",),
            });
        }
        Ok(Conjunct { left, right, op })
    }

    /// Evaluate the conjunct against a tuple.
    fn eval(&self, row: &[Value]) -> Result<bool, SemanticError> {
        match self.op {
            CompareOp::Eq => Ok(self.left.as_value(row) == self.right.as_value(row)),
            CompareOp::Ne => Ok(self.left.as_value(row) != self.right.as_value(row)),
            CompareOp::Lt => Ok(self.left.as_value(row) < self.right.as_value(row)),
            CompareOp::Le => Ok(self.left.as_value(row) <= self.right.as_value(row)),
            CompareOp::Gt => Ok(self.left.as_value(row) > self.right.as_value(row)),
            CompareOp::Ge => Ok(self.left.as_value(row) >= self.right.as_value(row)),
        }
    }
}

/// A disjunct in DNF: a conjunction of conjuncts (AND clause).
/// All conjuncts must be true for this disjunct to be true.
#[derive(Debug, Clone)]
struct Disjunct {
    conjuncts: Vec<Conjunct>,
}

impl Disjunct {
    /// Evaluate the disjunct against a tuple.
    /// Returns true if all conjuncts are true.
    fn eval(&self, row: &[Value]) -> Result<bool, SemanticError> {
        for conjunct in &self.conjuncts {
            if !conjunct.eval(row)? {
                return Ok(false);
            }
        }
        Ok(true)
    }
}

/// A compiled `select` condition in DNF (Disjunctive Normal Form):
/// a list of disjuncts (OR clauses), where each disjunct is a list of
/// conjuncts (AND clause). A tuple passes if ANY disjunct is fully satisfied.
///
/// This is the public condition interface: [`FilterCondition::compile`] a
/// typed [`Condition`] against a schema, then
/// [`FilterCondition::eval`] it against tuples.
#[derive(Debug, Clone)]
pub struct FilterCondition {
    disjuncts: Vec<Disjunct>,
}

impl FilterCondition {
    /// Compile `condition` against the schema of the tuples it will filter.
    ///
    /// The condition is converted to **DNF (Disjunctive Normal Form)** in a single
    /// recursive pass: a list of disjuncts (OR clauses), each a list of conjuncts
    /// (AND clauses). A tuple passes if ANY disjunct is fully satisfied.
    ///
    /// Errors: [`SemanticError::UnknownAttribute`] for an operand naming a
    /// column the schema does not have. [`SemanticError::TypeError`] for
    /// unsupported constructs (aggregates).
    pub fn compile(condition: &Condition, schema: &Schema) -> Result<Self, SemanticError> {
        let mut disjuncts = Vec::new();
        Self::compile_dnf(condition, schema, false, &mut disjuncts)?;
        if disjuncts.is_empty() {
            return Err(SemanticError::TypeError {
                detail: "filter condition cannot be empty".to_string(),
            });
        }
        Ok(FilterCondition { disjuncts })
    }

    /// Single-pass recursive DNF compilation with accumulator to minimize allocations.
    ///
    /// Builds disjuncts directly into `output` vector.
    /// - `negated`: whether we're under a NOT (for De Morgan's laws)
    fn compile_dnf(
        condition: &Condition,
        schema: &Schema,
        negated: bool,
        output: &mut Vec<Disjunct>,
    ) -> Result<(), SemanticError> {
        match condition {
            // Base case: comparison literal
            Condition::Compare(compare) => {
                let op = if negated {
                    compare.op.negate()
                } else {
                    compare.op
                };
                let conjunct = Conjunct::new(&compare.left, &compare.right, op, schema)?;
                output.push(Disjunct {
                    conjuncts: vec![conjunct],
                });
                Ok(())
            }
            // AND:
            // - If not negated: Cartesian product of disjuncts (distribute AND over OR)
            // - If negated: De Morgan's law: NOT (A AND B) = NOT A OR NOT B
            Condition::And(a, b) => {
                if negated {
                    // NOT (A AND B) = NOT A OR NOT B
                    // Apply negation to both sides and union
                    Self::compile_dnf(a, schema, true, output)?;
                    Self::compile_dnf(b, schema, true, output)
                } else {
                    // A AND B: Cartesian product of disjuncts
                    // Collect left disjuncts into a temp vec, then combine with right
                    let mut left_disjuncts = Vec::new();
                    Self::compile_dnf(a, schema, false, &mut left_disjuncts)?;
                    Self::combine_and_disjuncts(&mut left_disjuncts, b, schema, false)?;
                    // Move combined disjuncts to output
                    output.append(&mut left_disjuncts);
                    Ok(())
                }
            }
            // OR:
            // - If not negated: Union of disjuncts (just append to output)
            // - If negated: De Morgan's law: NOT (A OR B) = NOT A AND NOT B
            Condition::Or(a, b) => {
                if negated {
                    // NOT (A OR B) = NOT A AND NOT B
                    // Cartesian product of negated disjuncts
                    let mut left_disjuncts = Vec::new();
                    Self::compile_dnf(a, schema, true, &mut left_disjuncts)?;
                    Self::combine_and_disjuncts(&mut left_disjuncts, b, schema, true)?;
                    // Move combined disjuncts to output
                    output.append(&mut left_disjuncts);
                    Ok(())
                } else {
                    // A OR B: Union of disjuncts (append to output)
                    Self::compile_dnf(a, schema, false, output)?;
                    Self::compile_dnf(b, schema, false, output)
                }
            }
            // NOT: Flip the negation flag and recurse
            Condition::Not(inner) => Self::compile_dnf(inner, schema, !negated, output),
        }
    }

    /// Helper: combine left_disjuncts with a new condition using AND (cartesian product).
    /// Modifies left_disjuncts in-place to avoid extra allocation.
    fn combine_and_disjuncts(
        left_disjuncts: &mut Vec<Disjunct>,
        right_cond: &Condition,
        schema: &Schema,
        negated: bool,
    ) -> Result<(), SemanticError> {
        // First compile the right side into its own disjuncts
        let mut right_disjuncts = Vec::new();
        Self::compile_dnf(right_cond, schema, negated, &mut right_disjuncts)?;

        if left_disjuncts.is_empty() {
            // No left disjuncts - just use right disjuncts
            *left_disjuncts = right_disjuncts;
            return Ok(());
        }

        if right_disjuncts.is_empty() {
            // No right disjuncts - left stays as is
            return Ok(());
        }

        // Both sides have disjuncts - need Cartesian product
        // Optimize: if one side has only 1 disjunct, we can avoid some allocation
        if left_disjuncts.len() == 1 && right_disjuncts.len() == 1 {
            // Both have 1: just extend the single left with right's conjuncts
            let left = &mut left_disjuncts[0];
            left.conjuncts.append(&mut right_disjuncts[0].conjuncts);
        } else if left_disjuncts.len() == 1 {
            // Only 1 left disjunct: extend it with all right conjuncts
            let left = &mut left_disjuncts[0];
            for right in right_disjuncts.drain(..) {
                left.conjuncts.extend(right.conjuncts.clone());
            }
        } else if right_disjuncts.len() == 1 {
            // Multiple left, 1 right: extend each left with right's conjuncts
            let right = right_disjuncts.pop().unwrap();
            for left in left_disjuncts.iter_mut() {
                left.conjuncts.extend(right.conjuncts.clone());
            }
        } else {
            // Both have multiple disjuncts - full Cartesian product
            let mut new_disjuncts =
                Vec::with_capacity(left_disjuncts.len() * right_disjuncts.len());
            for left in left_disjuncts.iter() {
                for right in &right_disjuncts {
                    let mut conjuncts = left.conjuncts.clone();
                    conjuncts.extend(right.conjuncts.clone());
                    new_disjuncts.push(Disjunct { conjuncts });
                }
            }
            *left_disjuncts = new_disjuncts;
        }
        Ok(())
    }

    /// Test one tuple. Returns true if ANY disjunct is fully satisfied.
    pub fn eval(&self, row: &[Value]) -> Result<bool, SemanticError> {
        for disjunct in &self.disjuncts {
            if disjunct.eval(row)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Column;

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

    /// A one-input condition resolves its columns and evaluates each tuple.
    #[test]
    fn condition_selects_the_right_rows() {
        let schema = schema(&[("R.a", ColumnType::Int)]);
        let cond = FilterCondition::compile(&condition("R.a > 1"), &schema).unwrap();
        assert!(!cond.eval(&[Value::Int(0)]).unwrap());
        assert!(!cond.eval(&[Value::Int(1)]).unwrap());
        assert!(cond.eval(&[Value::Int(2)]).unwrap());
    }

    /// A name that is not in the schema is reported as an unknown attribute.
    #[test]
    fn condition_rejects_unknown_columns() {
        let schema = schema(&[("R.a", ColumnType::Int)]);
        match FilterCondition::compile(&condition("R.zz = 1"), &schema) {
            Err(SemanticError::UnknownAttribute { name }) => assert_eq!(name, "R.zz"),
            other => panic!("expected UnknownAttribute, got {other:?}"),
        }
    }

    /// Multiple AND conjuncts are flattened and all must pass.
    #[test]
    fn multiple_and_conjuncts() {
        let schema = schema(&[("R.a", ColumnType::Int), ("R.b", ColumnType::Int)]);
        let cond = FilterCondition::compile(&condition("R.a > 1 and R.b < 5"), &schema).unwrap();
        assert!(!cond.eval(&[Value::Int(0), Value::Int(10)]).unwrap());
        assert!(!cond.eval(&[Value::Int(2), Value::Int(10)]).unwrap());
        assert!(cond.eval(&[Value::Int(2), Value::Int(3)]).unwrap());
    }

    /// OR is compiled to DNF and evaluates correctly.
    #[test]
    fn or_condition() {
        let schema = schema(&[("R.a", ColumnType::Int)]);
        let cond = FilterCondition::compile(&condition("R.a = 1 or R.a = 2"), &schema).unwrap();
        assert!(cond.eval(&[Value::Int(1)]).unwrap());
        assert!(cond.eval(&[Value::Int(2)]).unwrap());
        assert!(!cond.eval(&[Value::Int(3)]).unwrap());
    }

    /// NOT is pushed down and evaluates correctly.
    #[test]
    fn not_condition() {
        let schema = schema(&[("R.a", ColumnType::Int)]);
        let cond = FilterCondition::compile(&condition("not R.a = 1"), &schema).unwrap();
        assert!(!cond.eval(&[Value::Int(1)]).unwrap());
        assert!(cond.eval(&[Value::Int(2)]).unwrap());
    }

    /// Complex condition with AND, OR, NOT.
    #[test]
    fn complex_condition() {
        let schema = schema(&[("R.a", ColumnType::Int), ("R.b", ColumnType::Int)]);
        // (R.a = 1 AND R.b > 5) OR (R.a = 2 AND R.b < 3)
        let cond = FilterCondition::compile(
            &condition("(R.a = 1 and R.b > 5) or (R.a = 2 and R.b < 3)"),
            &schema,
        )
        .unwrap();
        assert!(cond.eval(&[Value::Int(1), Value::Int(6)]).unwrap());
        assert!(cond.eval(&[Value::Int(2), Value::Int(2)]).unwrap());
        assert!(!cond.eval(&[Value::Int(1), Value::Int(3)]).unwrap());
        assert!(!cond.eval(&[Value::Int(2), Value::Int(5)]).unwrap());
        assert!(!cond.eval(&[Value::Int(3), Value::Int(1)]).unwrap());
    }

    /// NOT of AND becomes OR of NOTs (De Morgan).
    #[test]
    fn not_and_becomes_or() {
        let schema = schema(&[("R.a", ColumnType::Int)]);
        // NOT (R.a = 1 AND R.a = 2) => NOT R.a = 1 OR NOT R.a = 2 => always true since a can't be both
        let cond =
            FilterCondition::compile(&condition("not (R.a = 1 and R.a = 2)"), &schema).unwrap();
        assert!(cond.eval(&[Value::Int(1)]).unwrap());
        assert!(cond.eval(&[Value::Int(2)]).unwrap());
        assert!(cond.eval(&[Value::Int(3)]).unwrap());
    }

    /// NOT of OR becomes AND (De Morgan).
    #[test]
    fn not_or_becomes_and() {
        let schema = schema(&[("R.a", ColumnType::Int)]);
        // NOT (R.a = 1 OR R.a = 2) => NOT R.a = 1 AND NOT R.a = 2 => R.a != 1 AND R.a != 2
        let cond =
            FilterCondition::compile(&condition("not (R.a = 1 or R.a = 2)"), &schema).unwrap();
        assert!(!cond.eval(&[Value::Int(1)]).unwrap());
        assert!(!cond.eval(&[Value::Int(2)]).unwrap());
        assert!(cond.eval(&[Value::Int(3)]).unwrap());
    }

    /// Comparing across kinds is a type error, equality included (§1.3).
    #[test]
    fn comparison_across_kinds_is_rejected() {
        let schema = schema(&[("R.a", ColumnType::Int)]);
        for source in ["R.a = 'x'", "R.a > 'x'", "'x' = R.a"] {
            match FilterCondition::compile(&condition(source), &schema) {
                Err(SemanticError::TypeError { .. }) => {}
                other => panic!("expected TypeError for {source:?}, got {other:?}"),
            }
        }
    }

    /// Two VARCHARs of different declared lengths compare as strings.
    #[test]
    fn varchar_lengths_compare() {
        let schema = schema(&[("R.b", ColumnType::Varchar(5))]);
        let cond = FilterCondition::compile(&condition("R.b = 'hi'"), &schema).unwrap();
        assert!(cond.eval(&[Value::Varchar("hi".into())]).unwrap());
    }
}
