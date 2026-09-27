//! The engine/interpreter: values, relations, and bottom-up evaluation of
//! a parsed [`Query`]. (Parsing of §4.1 relation definitions lives in
//! [`crate::parser`].)

use std::borrow::Cow;
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::error::Error as StdError;
use std::fmt;
use std::ops::Deref;
use std::sync::Arc;

use crate::parser::{CompareOp, Operand, Predicate, Query};

// =====================================================================
// Semantic errors
// =====================================================================

/// Why a query could not be evaluated: the errors of §6.3 that survive parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum SemanticError {
    /// Two input schemas cannot be combined: arity, column names, or
    /// qualified names that collide.
    SchemaMismatch { detail: String },
    /// A comparison or set operation crossed types, e.g. an int against a
    /// string.
    TypeError { detail: String },
    /// No column in scope is named `name`.
    UnknownAttribute { name: String },
    /// No relation is loaded under `name`.
    UnknownRelation { name: String },
    /// A relation header lists `name` twice.
    DuplicateColumn { name: String },
    /// A projection lists the same column twice.
    DuplicateProjectedAttribute { name: String },
}

impl fmt::Display for SemanticError {
    /// Renders the error as a one-line message.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SemanticError::SchemaMismatch { detail } => write!(f, "schema mismatch: {detail}"),
            SemanticError::TypeError { detail } => write!(f, "type error: {detail}"),
            SemanticError::UnknownAttribute { name } => write!(f, "unknown attribute '{name}'"),
            SemanticError::UnknownRelation { name } => write!(f, "unknown relation '{name}'"),
            SemanticError::DuplicateColumn { name } => write!(f, "duplicate column '{name}'"),
            SemanticError::DuplicateProjectedAttribute { name } => {
                write!(f, "attribute '{name}' projected more than once")
            }
        }
    }
}
impl StdError for SemanticError {}

// =====================================================================
// Values & relations
// =====================================================================

/// A column value: an integer or a string.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    /// An integer value.
    Int(i64),
    /// A string value.
    Str(String),
}

impl Value {
    /// The type name of this value, `"int"` or `"str"`, for error messages.
    fn kind(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Str(_) => "str",
        }
    }

    /// Whether `self` compares to `other` in the direction `want`.
    ///
    /// Errors: [`SemanticError::TypeError`] for an int against a string, never
    /// a silent `false` (§4.3, case #22).
    fn matches(&self, want: Ordering, other: &Value) -> Result<bool, SemanticError> {
        match (self, other) {
            (Value::Int(x), Value::Int(y)) => Ok(x.cmp(y) == want),
            (Value::Str(x), Value::Str(y)) => Ok(x.cmp(y) == want),
            (a, b) => Err(SemanticError::TypeError {
                detail: format!("cannot compare {} with {}", a.kind(), b.kind()),
            }),
        }
    }
}

impl fmt::Display for Value {
    /// Renders the value as it is written in a relation file.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(i) => write!(f, "{i}"),
            Value::Str(s) => write!(f, "{s}"),
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

/// Error from [`Relation::insert`]: the tuple does not fit this relation's
/// schema, so the insertion is refused rather than panicking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowError {
    /// The tuple has a different number of values than the relation has
    /// columns.
    Arity { expected: usize, found: usize },
    /// The `col`-th value has a different kind than the values already in that
    /// column; `expected` is the column's kind and `found` the new value's.
    Type {
        col: usize,
        name: String,
        expected: &'static str,
        found: &'static str,
    },
}

impl fmt::Display for RowError {
    /// Renders the error as a one-line message naming the column and both
    /// kinds.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RowError::Arity { expected, found } => write!(
                f,
                "tuple has {found} values but the relation has {expected} columns"
            ),
            RowError::Type {
                col,
                name,
                expected,
                found,
            } => write!(
                f,
                "column '{name}' (position {col}) already holds {expected} values; the new value is {found}"
            ),
        }
    }
}
impl StdError for RowError {}

// =====================================================================
// Schema
// =====================================================================

/// A relation's columns: the names as §4.3 says the producing operator wrote
/// them, plus the relation name to qualify them with if the schema is ever an
/// input to a `times` or `join`.
///
/// The two are separate because `project` and the set operators keep a
/// relation's columns but not its name: `project[Name](Emp)` outputs the bare
/// name `Name` while a later join must still contribute `Emp.Name`.
#[derive(Debug, Clone, PartialEq)]
pub struct Schema {
    qualifier: Option<Arc<str>>,
    names: Arc<[String]>,
}

impl Schema {
    /// A schema of the given column names, with no relation name of its own.
    pub fn new<S, I>(names: S) -> Self
    where
        S: IntoIterator<Item = I>,
        I: Into<String>,
    {
        Schema {
            qualifier: None,
            names: names.into_iter().map(Into::into).collect(),
        }
    }

    /// The column names, in order.
    pub fn names(&self) -> &[String] {
        &self.names
    }

    /// The relation name a `times` or `join` would prefix to these columns, if
    /// this schema has one.
    pub fn qualifier(&self) -> Option<&str> {
        self.qualifier.as_deref()
    }

    /// The number of columns.
    pub fn arity(&self) -> usize {
        self.names.len()
    }

    /// Iterate over the column names, in order.
    pub fn iter(&self) -> std::slice::Iter<'_, String> {
        self.names.iter()
    }

    /// This schema with a different relation name, or with none.
    pub fn set_qualifier(&mut self, name: Option<&str>) {
        self.qualifier = name.map(Arc::from)
    }

    /// This schema's relation name with different column names.
    fn with_names(&self, names: Arc<[String]>) -> Self {
        Schema {
            qualifier: self.qualifier.clone(),
            names,
        }
    }

    /// §4.2 `rename`: the same columns qualified by `new_name`, which also
    /// becomes this schema's qualifier.
    ///
    /// Fails if that would give two columns the same name, e.g. renaming a
    /// schema holding both `Emp.DID` and `Dept.DID` to one name.
    fn rename(&self, new_name: &str) -> Result<Self, SemanticError> {
        let schema = Schema {
            qualifier: Some(Arc::from(new_name)),
            names: self
                .names
                .iter()
                .map(|header| {
                    let bare = header.rsplit_once('.').map(|(_, b)| b).unwrap_or(header);
                    format!("{new_name}.{bare}")
                })
                .collect(),
        };
        schema.check_no_qualified_duplicates()?;
        Ok(schema)
    }

    /// The column `name` denotes here, as an index.
    ///
    /// A name must equal a header exactly: there is no rule for a bare name to
    /// match a qualified header, so `DID` does not find `Emp.DID` and `ID` does
    /// not find `DID`. That is what makes a self join expressible, `Emp.EID`
    /// and `E2.EID` are two distinct names, so a condition can say which it
    /// means.
    ///
    /// Errors: [`SemanticError::UnknownAttribute`] if no column has that name.
    pub fn resolve(&self, name: &str) -> Result<usize, SemanticError> {
        self.names
            .iter()
            .position(|header| header == name)
            .ok_or_else(|| SemanticError::UnknownAttribute {
                name: name.to_string(),
            })
    }

    /// §4.3: the output schema of a `times` or `join` on inputs with these two
    /// schemas — both sets of attributes, each qualified by its own relation
    /// name.
    ///
    /// Errors: [`SemanticError::SchemaMismatch`] if the two inputs contribute
    /// the same qualified name.
    fn joined(left: &Schema, right: &Schema) -> Result<Self, SemanticError> {
        let q_left = left
            .iter()
            .map(|header| qualify_name(header, left.qualifier()));
        let q_right = right
            .iter()
            .map(|header| qualify_name(header, right.qualifier()));
        let schema = Schema::new(q_left.chain(q_right));
        match (left.qualifier(), right.qualifier()) {
            (Some(a), Some(b)) if a != b => Ok(schema),
            _ => {
                schema.check_no_qualified_duplicates()?;
                Ok(schema)
            }
        }
    }

    /// Fails if two columns share a name, which would make the schema
    /// impossible to address (§4.3).
    ///
    /// Errors: [`SemanticError::DuplicateColumn`] naming the repeated column.
    fn check_duplicates(&self) -> Result<(), SemanticError> {
        let mut seen = HashSet::new();
        for name in self.names.iter() {
            if !seen.insert(name) {
                return Err(SemanticError::DuplicateColumn { name: name.clone() });
            }
        }
        Ok(())
    }

    /// [`Schema::check_duplicates`], reported the way §4.3 phrases a collision
    /// between two inputs or a rename's two sources.
    fn check_no_qualified_duplicates(&self) -> Result<(), SemanticError> {
        self.check_duplicates().map_err(|error| match error {
            SemanticError::DuplicateColumn { name } => SemanticError::SchemaMismatch {
                detail: format!("duplicate qualified column '{name}'"),
            },
            other => other,
        })
    }
}

/// Compare a schema against a literal list of names, which is how a caller
/// states an expected output schema: `assert_eq!(rel.schema(), ["a", "b"])`.
/// The qualifier is not part of that claim, so it is not compared.
impl<const N: usize> PartialEq<[&str; N]> for &Schema {
    /// Whether these are the column names `other`, in order.
    fn eq(&self, other: &[&str; N]) -> bool {
        self.names() == other
    }
}

impl fmt::Display for Schema {
    /// Renders the column names, comma-separated.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.names.join(", "))
    }
}

/// A relation: a [`Schema`] and a set of [`Row`]s.
#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    schema: Schema,
    rows: HashSet<Row>,
}

impl Relation {
    /// A new, empty relation with the given column names.
    ///
    /// Errors: [`SemanticError::DuplicateColumn`] if a name is repeated, which
    /// would leave that name denoting more than one column.
    pub fn new<S, I>(schema: S) -> Result<Self, SemanticError>
    where
        S: IntoIterator<Item = I>,
        I: Into<String>,
    {
        let schema = Schema::new(schema);
        schema.check_duplicates()?;
        Ok(Relation {
            schema,
            rows: HashSet::new(),
        })
    }

    /// This relation's columns: their names, and the relation name to qualify
    /// them with if it is ever an input to a `times` or `join`.
    pub fn schema(&self) -> &Schema {
        &self.schema
    }

    /// The number of columns.
    pub fn arity(&self) -> usize {
        self.schema.arity()
    }

    /// The number of tuples.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether this relation contains no tuples.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Whether `row` is a tuple of this relation.
    pub fn contains(&self, row: impl Into<Row>) -> bool {
        let row = row.into();
        self.rows.contains(&row)
    }

    /// Iterate over the tuples, in arbitrary set order.
    pub fn iter(&self) -> impl Iterator<Item = Row> + '_ {
        self.rows.iter().cloned()
    }

    /// Add one tuple to this relation.
    ///
    /// Returns `Ok(true)` if the tuple was newly inserted and `Ok(false)` if it
    /// was already present (§1.2: a relation is a set).
    ///
    /// Errors: [`RowError::Arity`] for the wrong number of values, or
    /// [`RowError::Type`] for a value whose kind differs from the column's.
    pub fn insert(&mut self, row: impl Into<Row>) -> Result<bool, RowError> {
        let row = row.into();
        let arity = self.arity();
        if row.len() != arity {
            return Err(RowError::Arity {
                expected: arity,
                found: row.len(),
            });
        }
        if let Some(prev) = self.rows.iter().next() {
            for (i, value) in row.iter().enumerate() {
                if value.kind() != prev[i].kind() {
                    return Err(RowError::Type {
                        col: i,
                        name: self.schema.names[i].clone(),
                        expected: prev[i].kind(),
                        found: value.kind(),
                    });
                }
            }
        }
        Ok(self.rows.insert(row))
    }

    /// §4.2 `select`: keep the tuples for which `cond` returns `Ok(true)`, with
    /// the schema unchanged.
    ///
    /// Errors: the first [`SemanticError`] from `cond`, e.g. an int compared
    /// with a string, which aborts the selection.
    pub fn select<F>(&self, mut cond: F) -> Result<Self, SemanticError>
    where
        F: FnMut(&Row) -> Result<bool, SemanticError>,
    {
        let mut rows = HashSet::new();
        for row in &self.rows {
            if cond(row)? {
                rows.insert(row.clone());
            }
        }
        Ok(Relation {
            schema: self.schema.clone(),
            rows
        })
    }

    /// §4.2 `project`: keep only the named columns, in the order listed, and
    /// remove duplicate tuples afterwards (row #23). The output column names
    /// are the names as written, qualified or not.
    ///
    /// Errors: [`SemanticError::UnknownAttribute`] for a name this relation
    /// does not have, or [`SemanticError::DuplicateProjectedAttribute`] if a
    /// column is listed twice (row #24).
    pub fn project<S, I>(&self, cols: S) -> Result<Self, SemanticError>
    where
        S: IntoIterator<Item = I>,
        I: Into<String>,
    {
        let mut schema = Vec::new();
        let mut seen_idx = HashSet::new();
        let mut indices = Vec::new();
        for name in cols {
            let name = name.into();
            let idx = self.schema.resolve(&name)?;
            if !seen_idx.insert(idx) {
                return Err(SemanticError::DuplicateProjectedAttribute { name });
            }
            schema.push(name);
            indices.push(idx);
        }
        Ok(Relation {
            schema: self.schema.with_names(Arc::from(schema)),
            rows: self
                .rows
                .iter()
                .map(|row| Row::from(indices.iter().map(|&i| row[i].clone()).collect::<Vec<_>>()))
                .collect(),
        })
    }

    /// §4.2 `rename`: the same attributes under a new relation name. Every
    /// column becomes `<new_name>.<bare name>` and the schema's relation name
    /// becomes `new_name`, so a later `times` or `join` uses it too. Tuples are
    /// unchanged.
    ///
    /// Errors: [`SemanticError::SchemaMismatch`] if that would give two columns
    /// the same name, e.g. when the input holds both `Emp.EID` and `Dept.EID`.
    pub fn rename(self, new_name: &str) -> Result<Self, SemanticError> {
        Ok(Relation {
            schema: self.schema.rename(new_name)?,
            rows: self.rows,
        })
    }

    /// §4.3 `times`: the cartesian product with `other`, over the concatenation
    /// of both schemas.
    ///
    /// Errors: [`SemanticError::SchemaMismatch`] if the two inputs contribute
    /// the same qualified column name.
    pub fn times(&self, other: &Relation) -> Result<Self, SemanticError> {
        let schema = Schema::joined(&self.schema, &other.schema)?;
        let mut rows = HashSet::new();
        for lr in &self.rows {
            for rr in &other.rows {
                let row = lr.iter().chain(rr.iter()).cloned().collect();
                rows.insert(row);
            }
        }
        Ok(Relation { schema, rows })
    }

    /// §4.3 `join[c]`: a theta join — every pair of tuples for which `cond`
    /// returns `Ok(true)`, left tuple first. The output schema is the
    /// concatenation of both schemas.
    ///
    /// The output schema is built here rather than taken as an argument,
    /// because the caller has no other use for it: `cond` is compiled from the
    /// two inputs, so a caller that had to pass a schema in would have had to
    /// build the same one twice.
    ///
    /// Errors: [`SemanticError::SchemaMismatch`] for colliding qualified names,
    /// or the first [`SemanticError`] from `cond`.
    pub fn join<F>(&self, other: &Relation, mut cond: F) -> Result<Self, SemanticError>
    where
        F: FnMut(&Row, &Row) -> Result<bool, SemanticError>,
    {
        let schema = Schema::joined(&self.schema, &other.schema)?;
        let mut rows = HashSet::new();
        for lr in &self.rows {
            for rr in &other.rows {
                if cond(lr, rr)? {
                    let row = lr.iter().chain(rr.iter()).cloned().collect();
                    rows.insert(row);
                }
            }
        }
        Ok(Relation { schema, rows })
    }

    /// §4.3 `union`: the tuples of either relation, keeping this relation's
    /// schema.
    ///
    /// Errors: if `other` is not union compatible — same arity, same column
    /// names, compatible types position by position.
    pub fn union(&self, other: &Relation) -> Result<Self, SemanticError> {
        self.check_compatible(other)?;
        let rows = self.rows.union(&other.rows).cloned().collect();
        Ok(Relation {
            schema: self.schema.clone(),
            rows
        })
    }

    /// §4.3 `minus`: the tuples of `self` not present in `other`, keeping this
    /// relation's schema.
    ///
    /// Errors: if `other` is not union compatible.
    pub fn minus(&self, other: &Relation) -> Result<Self, SemanticError> {
        self.check_compatible(other)?;
        let rows = self.rows.difference(&other.rows).cloned().collect();
        Ok(Relation {
            schema: self.schema.clone(),
            rows
        })
    }

    /// §4.3 `intersect`: the tuples in both relations, keeping this relation's
    /// schema.
    ///
    /// Errors: if `other` is not union compatible.
    pub fn intersect(&self, other: &Relation) -> Result<Self, SemanticError> {
        self.check_compatible(other)?;
        let rows = self.rows.intersection(&other.rows).cloned().collect();
        Ok(Relation {
            schema: self.schema.clone(),
            rows
        })
    }

    /// Union compatibility (§4.3): the same arity, the same column names in
    /// the same order, and compatible types position by position. An operand
    /// with no tuples carries no type information and stays compatible with
    /// anything.
    ///
    /// Errors: [`SemanticError::SchemaMismatch`] for arity or name differences,
    /// [`SemanticError::TypeError`] for a type difference in some column.
    fn check_compatible(&self, other: &Relation) -> Result<(), SemanticError> {
        if self.schema.arity() != other.schema.arity() {
            return Err(SemanticError::SchemaMismatch {
                detail: format!(
                    "{} attributes on left, {} on right",
                    self.schema.arity(),
                    other.schema.arity()
                ),
            });
        }
        for (i, (a, b)) in self.schema.iter().zip(other.schema.iter()).enumerate() {
            if a != b {
                return Err(SemanticError::SchemaMismatch {
                    detail: format!("column {i} differs: '{a}' vs '{b}'"),
                });
            }
        }
        if let (Some(lrow), Some(rrow)) = (self.rows.iter().next(), other.rows.iter().next()) {
            for (i, (a, b)) in lrow.iter().zip(rrow.iter()).enumerate() {
                if a.kind() != b.kind() {
                    return Err(SemanticError::TypeError {
                        detail: format!(
                            "column '{}' has type {} on left but {} on right",
                            self.schema.names[i],
                            a.kind(),
                            b.kind(),
                        ),
                    });
                }
            }
        }
        Ok(())
    }
}

impl fmt::Display for Relation {
    /// Renders the schema, then one tuple per line, or `(0 tuples)` when the
    /// relation is empty (row #25).
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "{}", self.schema)?;
        if self.rows.is_empty() {
            write!(f, "(0 tuples)")
        } else {
            let body: Vec<String> = self
                .rows
                .iter()
                .map(|row| {
                    row.iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(", ")
                })
                .collect();
            write!(f, "{}", body.join("\n"))
        }
    }
}

// =====================================================================
// Resolved conditions
// =====================================================================

/// A `select` operand (§4.2) resolved against the schema being filtered: a
/// column of the tuple under test, or a constant. There is only one tuple, so
/// an operand has no side to record.
#[derive(Debug, Clone)]
enum SelectExpr {
    /// The column at this index.
    Col(usize),
    /// A constant value.
    Value(Value)
}

impl SelectExpr {
    /// Resolve one operand against `schema`.
    ///
    /// Errors: [`SemanticError::UnknownAttribute`] if the operand names a
    /// column the schema does not have.
    fn resolve(operand: &Operand, schema: &Schema) -> Result<Self, SemanticError> {
        match operand {
            Operand::Num(i) => Ok(SelectExpr::Value(Value::Int(*i))),
            Operand::Str(s) => Ok(SelectExpr::Value(Value::Str(s.clone()))),
            Operand::Attr(name) => schema.resolve(name).map(SelectExpr::Col),
        }
    }

    /// The operand's value for `row`.
    fn value<'a>(&'a self, row: &'a Row) -> &'a Value {
        match self {
            SelectExpr::Col(i) => &row[*i],
            SelectExpr::Value(v) => v,
        }
    }
}

/// A compiled `select` condition: every column it mentions belongs to the
/// single schema it was compiled against.
#[derive(Debug, Clone)]
enum SelectCond {
    /// `left` in direction `Ordering` against `right`.
    Cmp(SelectExpr, SelectExpr, Ordering),
    /// As [`SelectCond::Cmp`], with the result negated.
    ICmp(SelectExpr, SelectExpr, Ordering),
    /// Both sides must hold.
    And(Box<SelectCond>, Box<SelectCond>),
    /// Either side must hold.
    Or(Box<SelectCond>, Box<SelectCond>),
    /// The operand must not hold.
    Not(Box<SelectCond>),
}

impl SelectCond {
    /// Compile `predicate` against the schema of the relation it will filter.
    ///
    /// Errors: [`SemanticError::UnknownAttribute`] for an operand naming a
    /// column the schema does not have.
    pub fn compile(predicate: &Predicate, schema: &Schema) -> Result<Self, SemanticError> {
        match predicate {
            Predicate::Compare { left, op, right } => {
                let l = SelectExpr::resolve(left, schema)?;
                let r = SelectExpr::resolve(right, schema)?;
                let (want, negate) = match op {
                    CompareOp::Lt => (Ordering::Less, false),
                    CompareOp::Eq => (Ordering::Equal, false),
                    CompareOp::Gt => (Ordering::Greater, false),
                    CompareOp::Ge => (Ordering::Less, true),
                    CompareOp::Le => (Ordering::Greater, true),
                    CompareOp::Ne => (Ordering::Equal, true),
                };
                Ok(if negate {
                    SelectCond::ICmp(l, r, want)
                } else {
                    SelectCond::Cmp(l, r, want)
                })
            }
            Predicate::And(a, b) => Ok(SelectCond::And(
                Box::new(Self::compile(a, schema)?),
                Box::new(Self::compile(b, schema)?),
            )),
            Predicate::Or(a, b) => Ok(SelectCond::Or(
                Box::new(Self::compile(a, schema)?),
                Box::new(Self::compile(b, schema)?),
            )),
            Predicate::Not(a) => Ok(SelectCond::Not(
                Box::new(Self::compile(a, schema,)?)
            )),
        }
    }

    /// Test one tuple.
    ///
    /// Errors: [`SemanticError::TypeError`] if the condition compares an int
    /// with a string (§4.3, case #22).
    pub fn eval(&self, row: &Row) -> Result<bool, SemanticError> {
        match self {
            SelectCond::Cmp(l, r, want) => Ok(l.value(row).matches(*want, r.value(row))?),
            SelectCond::ICmp(l, r, want) => Ok(!l.value(row).matches(*want, r.value(row))?),
            SelectCond::And(a, b) => Ok(a.eval(row)? && b.eval(row)?),
            SelectCond::Or(a, b) => Ok(a.eval(row)? || b.eval(row)?),
            SelectCond::Not(a) => Ok(!a.eval(row)?),
        }
    }
}

/// A `join` operand (§4.2) resolved against the join's output schema: a column
/// of one named side, or a constant. The side is part of the operand, so
/// evaluation never has to work out which tuple a column belongs to.
#[derive(Debug, Clone)]
enum JoinExpr {
    /// A column of the left input, by index.
    LeftCol(usize),
    /// A column of the right input, by index.
    RightCol(usize),
    /// A constant value.
    Value(Value)
}

impl JoinExpr {
    /// Resolve one operand of a join condition to a column of one of the two
    /// inputs.
    ///
    /// A join condition is written against the join's own output, where every
    /// column is qualified by relation name — so `R join[R.b=S.b] S` says `R.b`
    /// even though `R`'s own header is the bare `b`. Rather than build that
    /// output schema to resolve against, each input is searched under the names
    /// it would have there, which is what [`qualify_name`] decides. So a bare
    /// name matches nothing, and left is tried first, as the output orders its
    /// columns.
    ///
    /// Errors: [`SemanticError::UnknownAttribute`] if the operand names a
    /// column of neither input.
    fn resolve(
        operand: &Operand,
        left_schema: &Schema,
        right_schema: &Schema,
    ) -> Result<Self, SemanticError> {
        let name = match operand {
            Operand::Num(i) => return Ok(JoinExpr::Value(Value::Int(*i))),
            Operand::Str(s) => return Ok(JoinExpr::Value(Value::Str(s.clone()))),
            Operand::Attr(name) => name.as_str(),
        };
        let col = left_schema.iter().position(|h| qualify_name(h, left_schema.qualifier()) == name);
        match col {
            Some(i) => Ok(JoinExpr::LeftCol(i)),
            None => right_schema
                .iter()
                .position(|h| qualify_name(h, right_schema.qualifier()) == name)
                .map(JoinExpr::RightCol)
                .ok_or_else(|| SemanticError::UnknownAttribute {
                    name: name.to_string(),
                }),
        }
    }

    /// The operand's value for the left and right tuples.
    fn value<'a>(&'a self, left: &'a Row, right: &'a Row) -> &'a Value {
        match self {
            JoinExpr::LeftCol(i) => &left[*i],
            JoinExpr::RightCol(i) => &right[*i],
            JoinExpr::Value(v) => v,
        }
    }
}

/// A compiled `join` condition: every column it mentions carries the side it
/// reads from, because a join has two tuples in play and neither can be
/// assumed.
#[derive(Debug, Clone)]
enum JoinCond {
    /// `left` in direction `Ordering` against `right`.
    Cmp(JoinExpr, JoinExpr, Ordering),
    /// As [`JoinCond::Cmp`], with the result negated.
    ICmp(JoinExpr, JoinExpr, Ordering),
    /// Both sides must hold.
    And(Box<JoinCond>, Box<JoinCond>),
    /// Either side must hold.
    Or(Box<JoinCond>, Box<JoinCond>),
    /// The operand must not hold.
    Not(Box<JoinCond>),
}

impl JoinCond {
    /// Compile `predicate` against the join's two input schemas, which is all a
    /// condition needs: each leaf records which side its column is on, so the
    /// pair loop never has to work that out.
    ///
    /// Errors: [`SemanticError::UnknownAttribute`] for an operand naming a
    /// column of neither input.
    pub fn compile(
        predicate: &Predicate,
        left_schema: &Schema,
        right_schema: &Schema,
    ) -> Result<Self, SemanticError> {
        match predicate {
            Predicate::Compare { left, op, right } => {
                let l = JoinExpr::resolve(left, left_schema, right_schema)?;
                let r = JoinExpr::resolve(right, left_schema, right_schema)?;
                let (want, negate) = match op {
                    CompareOp::Lt => (Ordering::Less, false),
                    CompareOp::Eq => (Ordering::Equal, false),
                    CompareOp::Gt => (Ordering::Greater, false),
                    CompareOp::Ge => (Ordering::Less, true),
                    CompareOp::Le => (Ordering::Greater, true),
                    CompareOp::Ne => (Ordering::Equal, true),
                };
                Ok(if negate {
                    JoinCond::ICmp(l, r, want)
                } else {
                    JoinCond::Cmp(l, r, want)
                })
            }
            Predicate::And(a, b) => Ok(JoinCond::And(
                Box::new(Self::compile(a, left_schema, right_schema)?),
                Box::new(Self::compile(b, left_schema, right_schema)?),
            )),
            Predicate::Or(a, b) => Ok(JoinCond::Or(
                Box::new(Self::compile(a, left_schema, right_schema)?),
                Box::new(Self::compile(b, left_schema, right_schema)?),
            )),
            Predicate::Not(a) => Ok(JoinCond::Not(Box::new(Self::compile(
                a,
                left_schema,
                right_schema,
            )?))),
        }
    }

    /// Test one pair of tuples, left first.
    ///
    /// Errors: [`SemanticError::TypeError`] if the condition compares an int
    /// with a string (§4.3, case #22).
    pub fn eval(&self, left: &Row, right: &Row) -> Result<bool, SemanticError> {
        match self {
            JoinCond::Cmp(l, r, want) => Ok(l.value(left, right).matches(*want, r.value(left, right))?),
            JoinCond::ICmp(l, r, want) => Ok(!l.value(left, right).matches(*want, r.value(left, right))?),
            JoinCond::And(a, b) => Ok(a.eval(left, right)? && b.eval(left, right)?),
            JoinCond::Or(a, b) => Ok(a.eval(left, right)? || b.eval(left, right)?),
            JoinCond::Not(a) => Ok(!a.eval(left, right)?),
        }
    }
}

/// `header` as a `times` or `join` output names it: with the relation's own
/// name in front, per §4.3 ("all attributes of both inputs, qualified by
/// relation name"). A header that already carries the qualifier is left alone,
/// and a schema with no qualifier keeps its headers as they are.
///
/// This is the one place that rule is written down, so a join condition can
/// resolve an operand against an input schema and reach exactly the names a
/// `times` or `join` output would have.
fn qualify_name<'a>(header: &'a str, qualifier: Option<&str>) -> Cow<'a, str> {
    match qualifier {
        // No relation name to apply: the headers already stand on their own.
        None => Cow::Borrowed(header),
        // Already carrying this very name, so leave it be.
        Some(q) if header.strip_prefix(q).is_some_and(|rest| rest.starts_with('.')) => {
            Cow::Borrowed(header)
        }
        Some(q) => Cow::Owned(format!("{q}.{header}")),
    }
}

// =====================================================================
// Engine
// =====================================================================

/// The §8.2 instrumentation counters.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Pairs of tuples a join condition was evaluated on, matching or not.
    pub join_comparisons: u64,
    /// Tuples a select condition was evaluated on.
    pub select_comparisons: u64,
}

/// The relations in scope for a query, plus the §8.2 counters.
pub struct Engine {
    relations: HashMap<String, Relation>,
    /// The counters [`Engine::execute`] maintains.
    pub stats: Stats,
}

impl Default for Engine {
    /// An engine with no relations loaded.
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    /// An engine with no relations loaded.
    pub fn new() -> Self {
        Engine {
            relations: HashMap::new(),
            stats: Stats::default(),
        }
    }

    /// Put `relation` in scope under `name`. This is where a relation learns
    /// its own name: until it is loaded, §4.3's "qualified by relation name"
    /// has nothing to qualify its columns with.
    pub fn load(&mut self, name: &str, mut relation: Relation) {
        relation.schema.set_qualifier(Some(name));
        self.relations.insert(name.to_string(), relation);
    }

    /// The relation loaded under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Relation> {
        self.relations.get(name)
    }

    /// Zero the instrumentation counters.
    pub fn reset_stats(&mut self) {
        self.stats = Stats::default();
    }

    /// Evaluate `expr` bottom-up and return its relation, counting the tuples
    /// and tuple pairs each `select` and `join` examines.
    ///
    /// Errors: the [`SemanticError`] of the first operator that cannot be
    /// applied, e.g. [`SemanticError::UnknownRelation`] for a query naming a
    /// relation that was never loaded.
    pub fn execute(&mut self, expr: &Query) -> Result<Relation, SemanticError> {
        match expr {
            Query::Variable(name) => self
                .relations
                .get(name)
                .cloned()
                .ok_or_else(|| SemanticError::UnknownRelation { name: name.clone() }),

            Query::Select { predicate, input } => {
                let relation = self.execute(input)?;
                let cond = SelectCond::compile(predicate, &relation.schema)?;
                relation.select(move |row| {
                    self.stats.select_comparisons += 1;
                    cond.eval(row)
                })
            }

            Query::Project { attrs, input } => {
                let relation = self.execute(input)?;
                relation.project(attrs)
            }

            Query::Rename { new_name, input } => {
                let relation = self.execute(input)?;
                relation.rename(new_name)
            }

            Query::Join {
                condition,
                left,
                right,
            } => {
                let left = self.execute(left)?;
                let right = self.execute(right)?;
                let cond = JoinCond::compile(condition, &left.schema, &right.schema)?;
                left.join(&right, move |left_row, right_row| {
                    self.stats.join_comparisons += 1;
                    cond.eval(left_row, right_row)
                })
            }

            Query::Times { left, right } => {
                let left = self.execute(left)?;
                let right = self.execute(right)?;
                left.times(&right)
            }

            Query::Union { left, right } => {
                let left = self.execute(left)?;
                let right = self.execute(right)?;
                left.union(&right)
            }

            Query::Minus { left, right } => {
                let left = self.execute(left)?;
                let right = self.execute(right)?;
                left.minus(&right)
            }

            Query::Intersect { left, right } => {
                let left = self.execute(left)?;
                let right = self.execute(right)?;
                left.intersect(&right)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse_query;

    /// The column names of a relation, for schema assertions.
    fn names(rel: &Relation) -> Vec<&str> {
        rel.schema().iter().map(|s| s.as_str()).collect()
    }

    /// A small `(a, b)` relation: row i is `(i, i)`.
    fn make(n: i64) -> Relation {
        let mut rel = Relation::new(["a", "b"]).expect("test header names are distinct");
        for i in 0..n {
            rel.insert([Value::Int(i), Value::Int(i)])
                .expect("static test data is well-formed");
        }
        rel
    }

    // ── §4.1 load-time row checks (spec §1.2: a column's type is the type
    //    of its values, and a relation is a set) ─────────────────────────

    /// `insert` reports new vs. already present, and refuses a tuple of the
    /// wrong arity or a column mixing types.
    #[test]
    fn add_row_returns_inserted_status_and_schema_errors() {
        let mut rel = Relation::new(["a", "b"]).expect("test header names are distinct");

        // First row: no type checking needed, and it is newly inserted.
        assert_eq!(rel.insert([Value::Int(1), Value::Int(2)]), Ok(true));

        // The same tuple again: already present, so not inserted (set
        // semantics — §1.2 duplicates collapse).
        assert_eq!(rel.insert([Value::Int(1), Value::Int(2)]), Ok(false));

        // Wrong arity is refused, not panicked.
        assert_eq!(
            rel.insert([Value::Int(1)]),
            Err(RowError::Arity {
                expected: 2,
                found: 1
            })
        );

        // A value whose kind differs from the kinds already in its column is
        // refused; the error names the column and both kinds.
        match rel.insert([Value::Str("x".into()), Value::Int(3)]) {
            Err(RowError::Type {
                col,
                name,
                expected,
                found,
            }) => {
                assert_eq!(col, 0);
                assert_eq!(name, "a");
                assert_eq!(expected, "int");
                assert_eq!(found, "str");
            }
            other => panic!("expected ColumnType RowError, got {other:?}"),
        }

        // Refused rows never disturb the relation's contents.
        assert_eq!(rel.len(), 1);
        assert_eq!(rel.arity(), 2);
    }

    // ── §4.3 compatibility checks beyond the numbered §7 cases ──────────

    /// A set operation between columns of the same name but different types is
    /// a type error, not a crash.
    #[test]
    fn union_of_same_name_columns_with_different_types_is_type_error() {
        // §4.3: comparison types must be compatible position by position. Both
        // sides are homogeneous per column, so a single representative tuple
        // from each side triggers the error — no row content beyond types is
        // involved.
        let mut eng = Engine::new();
        let mut r = Relation::new(["X"]).expect("test header names are distinct");
        r.insert([Value::Int(1)])
            .expect("static test data is well-formed");
        eng.load("R", r);
        let mut s = Relation::new(["X"]).expect("test header names are distinct");
        s.insert([Value::Str("a".into())])
            .expect("static test data is well-formed");
        eng.load("S", s);
        let expr = parse_query("R union S").unwrap();
        let err = eng.execute(&expr).unwrap_err();
        match err {
            SemanticError::TypeError { detail } => {
                assert_eq!(detail, "column 'X' has type int on left but str on right")
            }
            other => panic!("expected TypeError, got {other:?}"),
        }
    }

    /// An operand with no tuples carries no type information, so it stays
    /// union compatible with a typed one.
    #[test]
    fn union_with_an_empty_operand_does_not_fail_the_type_check() {
        // An operand with no tuples carries no type information, so it stays
        // compatible with a typed operand — the representative-row check has
        // nothing to compare, exactly like the old sampling check.
        let mut eng = Engine::new();
        eng.load("E", Relation::new(["X"]).expect("test header names are distinct"));
        let mut i = Relation::new(["X"]).expect("test header names are distinct");
        i.insert([Value::Int(1)])
            .expect("static test data is well-formed");
        eng.load("I", i);
        let expr = parse_query("E union I").unwrap();
        let result = eng
            .execute(&expr)
            .expect("empty operand must not cause a type error");
        assert_eq!(names(&result), ["X"]);
        assert_eq!(result.len(), 1);
        assert!(result.contains([Value::Int(1)]));
    }

    /// §8.2: a join counts every pair it examines, matching or not.
    #[test]
    fn join_counts_exactly_n_times_m_pairs() {
        let (r, s) = (make(4), make(3));
        let expr = parse_query("R join[R.b=S.b] S").unwrap();
        let mut eng = Engine::new();
        eng.load("R", r);
        eng.load("S", s);
        let result = eng.execute(&expr).expect("join should run");
        assert_eq!(eng.stats.join_comparisons, 4 * 3);
        assert_eq!(
            result.len(),
            3,
            "b ∈ {{0,1,2}} pairs matched on R.b = S.b"
        );
    }

    /// A join that matches nothing still counts all n × m pairs.
    #[test]
    fn join_counts_pairs_even_when_nothing_matches() {
        let mut eng = Engine::new();
        // Relation is a set: tuples must be distinct to keep 10 rows per side.
        let mut r = Relation::new(["a", "b"]).expect("test header names are distinct");
        for i in 0..10 {
            r.insert([Value::Int(i), Value::Int(i)])
                .expect("static test data is well-formed");
        }
        eng.load("R", r);
        let mut s = Relation::new(["b", "c"]).expect("test header names are distinct");
        for i in 0..10 {
            s.insert([Value::Int(10 + i), Value::Int(10 + i)])
                .expect("static test data is well-formed");
        }
        eng.load("S", s);
        let expr = parse_query("R join[R.b=S.b] S").unwrap();
        let result = eng.execute(&expr).expect("join should run");
        assert_eq!(result.len(), 0);
        assert_eq!(eng.stats.join_comparisons, 100, "100 = 10 × 10 pairs");
    }

    // ── condition evaluation: int/string type errors ────────────────────

    /// §4.3 case #22: comparing an int column to a string column is a type
    /// error, under `not` as well.
    #[test]
    fn join_comparing_int_column_to_str_column_is_type_error() {
        let mut r = Relation::new(["a", "b"]).expect("test header names are distinct");
        r.insert([Value::Int(1), Value::Int(10)])
            .expect("static test data is well-formed");
        let mut s = Relation::new(["b", "c"]).expect("test header names are distinct");
        s.insert([Value::Str("x".into()), Value::Str("y".into())])
            .expect("static test data is well-formed");
        let run = |query: &str| {
            let mut eng = Engine::new();
            eng.load("R", r.clone());
            eng.load("S", s.clone());
            eng.execute(&parse_query(query).unwrap()).unwrap_err()
        };
        // A plain column comparison…
        match run("R join[R.b=S.b] S") {
            SemanticError::TypeError { detail } => {
                assert_eq!(detail, "cannot compare int with str")
            }
            other => panic!("expected TypeError, got {other:?}"),
        }
        // …and the same comparison under `not`, which reuses the same
        // evaluator.
        match run("R join[not(R.b=S.b)] S") {
            SemanticError::TypeError { detail } => {
                assert_eq!(detail, "cannot compare int with str")
            }
            other => panic!("expected TypeError, got {other:?}"),
        }
    }

    /// A header with a repeated column name is refused, and the error names
    /// the repeat.
    #[test]
    fn relation_new_refuses_a_repeated_column_name() {
        // Every other schema is derived from schemas that were already
        // distinct, so a repeat can only enter here. It would not be
        // cosmetic: the name would resolve to whichever copy comes first, and a
        // condition on it would silently read the wrong column.
        //
        // Each case is (header names, the repeated name), so `x, y, x` is
        // checked to report the repeat rather than the first name it meets.
        let cases: &[(&[&str], &str)] = &[
            (&["a", "a"], "a"),
            (&["a", "b", "c", "b"], "b"),
            (&["x", "y", "x"], "x"),
        ];
        for (names, repeated) in cases {
            match Relation::new(names.iter().copied()) {
                Err(SemanticError::DuplicateColumn { name }) => {
                    assert_eq!(name, *repeated, "for {names:?}")
                }
                other => panic!("expected DuplicateColumn for {names:?}, got {other:?}"),
            }
        }

        // Distinct names are accepted, whatever their shape — including one
        // equal to the relation it will be loaded as, and a keyword.
        for names in [
            &["a"][..],
            &["a", "b"][..],
            &["K", "x"][..],
            &["union", "x"][..],
        ] {
            assert!(
                Relation::new(names.iter().copied()).is_ok(),
                "{names:?} should be a schema"
            );
        }
    }

    /// Each of the six comparison operators selects the rows it should, for
    /// both int and str columns.
    #[test]
    fn all_six_compare_operators_select_the_right_rows() {
        // Any wrong operator → comparison mapping shows up here as a wrong row
        // set.
        let check_int = |op: &str, len: usize, keep: &[i64]| {
            let mut eng = Engine::new();
            eng.load("R", make(3)); // rows (0,0), (1,1), (2,2)
            let result = eng
                .execute(&parse_query(&format!("select[a{op}1](R)")).unwrap())
                .expect("select should run");
            assert_eq!(result.len(), len, "rows for a{op}1");
            for i in keep {
                assert!(
                    result.contains([Value::Int(*i), Value::Int(*i)]),
                    "missing row a={i} for a{op}1"
                );
            }
        };
        check_int("<", 1, &[0]);
        check_int("=", 1, &[1]);
        check_int(">", 1, &[2]);
        check_int(">=", 2, &[1, 2]);
        check_int("<=", 2, &[0, 1]);
        check_int("!=", 2, &[0, 2]);

        // Same mapping through the lexicographic str compare.
        let mut s = Relation::new(["c"]).expect("test header names are distinct");
        for c in ["p", "q", "r"] {
            s.insert([Value::Str(c.into())])
                .expect("static test data is well-formed");
        }
        let check_str = |op: &str, len: usize, keep: &[&str]| {
            let mut eng = Engine::new();
            eng.load("S", s.clone());
            let result = eng
                .execute(&parse_query(&format!("select[c{op}'q'](S)")).unwrap())
                .expect("select should run");
            assert_eq!(result.len(), len, "rows for c{op}'q'");
            for c in keep {
                assert!(
                    result.contains([Value::Str((*c).into())]),
                    "missing row c={c} for c{op}'q'"
                );
            }
        };
        check_str("<", 1, &["p"]);
        check_str("=", 1, &["q"]);
        check_str(">", 1, &["r"]);
        check_str(">=", 2, &["q", "r"]);
        check_str("<=", 2, &["p", "q"]);
        check_str("!=", 2, &["p", "r"]);
    }

    /// A select counts every tuple it examines, even when none match.
    #[test]
    fn select_counts_every_tuple_even_when_none_match() {
        let mut eng = Engine::new();
        eng.load("R", make(7));
        let expr = parse_query("select[a>=1000](R)").unwrap();
        let result = eng.execute(&expr).expect("select should run");
        assert_eq!(result.len(), 0);
        assert_eq!(
            eng.stats.select_comparisons, 7,
            "every tuple was examined even though none matched"
        );
    }

    /// Each operator contributes its own counter, so counts accumulate over a
    /// nested query.
    #[test]
    fn nested_operators_accumulate_counts() {
        let mut eng = Engine::new();
        eng.load("R", make(5));
        let expr = parse_query("project[b](select[a>=0](R))").unwrap();
        let result = eng.execute(&expr).expect("query should run");
        assert_eq!(result.len(), 5);
        assert_eq!(eng.stats.select_comparisons, 5);
        assert_eq!(eng.stats.join_comparisons, 0);
    }

    /// `reset_stats` zeroes the counters between timed runs.
    #[test]
    fn reset_stats_starts_from_zero() {
        let mut eng = Engine::new();
        eng.load("R", make(3));
        let expr = parse_query("select[a>=0](R)").unwrap();
        let _ = eng.execute(&expr).expect("select should run");
        assert_eq!(eng.stats.select_comparisons, 3);
        eng.reset_stats();
        assert_eq!(eng.stats.select_comparisons, 0);
    }

    /// Naming a relation that was never loaded is an `UnknownRelation`
    /// carrying the name the query wrote. The lookup is a leaf case, so it
    /// fires wherever the variable sits — at the root, on one side of a
    /// join, or buried under an operator — not only when the whole query is
    /// a single variable.
    #[test]
    fn unknown_relation_is_reported_wherever_the_variable_sits() {
        let mut eng = Engine::new();
        eng.load("R", make(3));

        for query in ["S", "select[a=0](S)", "R join[a=b] S", "R times S", "project[a](S)"] {
            match eng.execute(&parse_query(query).unwrap()) {
                Err(SemanticError::UnknownRelation { name }) => {
                    assert_eq!(name, "S", "for {query}")
                }
                other => panic!("expected UnknownRelation for {query}, got {other:?}"),
            }
        }
    }

    /// A relation that *is* loaded still reports its own errors, so the
    /// missing name is the only thing `UnknownRelation` is reserved for:
    /// the relation lookup runs first, and only a surviving operand goes on
    /// to fail attribute resolution.
    #[test]
    fn a_loaded_relation_reports_attribute_errors_instead() {
        let mut eng = Engine::new();
        eng.load("R", make(3));
        match eng.execute(&parse_query("select[zz=0](R)").unwrap()) {
            Err(SemanticError::UnknownAttribute { name }) => assert_eq!(name, "zz"),
            other => panic!("expected UnknownAttribute, got {other:?}"),
        }
    }
}
