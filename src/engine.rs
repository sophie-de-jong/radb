//! The engine/interpreter: values, relations, and bottom-up evaluation of
//! a parsed [`Query`]. (Parsing of §4.1 relation definitions lives in
//! [`crate::parser`].)

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

#[derive(Debug, Clone, PartialEq)]
pub enum SemanticError {
    SchemaMismatch { detail: String },
    TypeError { detail: String },
    UnknownAttribute { name: String },
    UnknownRelation { name: String },
    AmbiguousAttribute { name: String },
    DuplicateProjectedAttribute { name: String },
}

impl fmt::Display for SemanticError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SemanticError::SchemaMismatch { detail } => write!(f, "schema mismatch: {detail}"),
            SemanticError::TypeError { detail } => write!(f, "type error: {detail}"),
            SemanticError::UnknownAttribute { name } => write!(f, "unknown attribute '{name}'"),
            SemanticError::UnknownRelation { name } => write!(f, "unknown relation '{name}'"),
            SemanticError::AmbiguousAttribute { name } => {
                write!(f, "ambiguous attribute '{name}'")
            }
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

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Value {
    Int(i64),
    Str(String),
}

impl Value {
    fn kind(&self) -> &'static str {
        match self {
            Value::Int(_) => "int",
            Value::Str(_) => "str",
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Int(i) => write!(f, "{i}"),
            Value::Str(s) => write!(f, "{s}"),
        }
    }
}

/// An immutable row of typed values, shared cheaply behind an `Arc`.
///
/// `Row` is the public handle for a [`Relation`]'s tuples: each value is a
/// [`Value`] in schema order. Cloning a `Row` is an `Arc` bump — O(1) —
/// and `Row` compares and hashes by value, so it can be used directly as a
/// set element or lookup key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Row(Arc<[Value]>);

impl Deref for Row {
    type Target = [Value];

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl AsRef<[Value]> for Row {
    fn as_ref(&self) -> &[Value] {
        &self.0
    }
}

impl From<Vec<Value>> for Row {
    fn from(values: Vec<Value>) -> Self {
        Row(Arc::from(values))
    }
}

impl<const N: usize> From<[Value; N]> for Row {
    fn from(values: [Value; N]) -> Self {
        Row(Arc::from(values))
    }
}

impl From<&[Value]> for Row {
    fn from(values: &[Value]) -> Self {
        Row(values.iter().cloned().collect())
    }
}

impl FromIterator<Value> for Row {
    fn from_iter<T: IntoIterator<Item = Value>>(iter: T) -> Self {
        Row(Arc::from(iter.into_iter().collect::<Vec<_>>()))
    }
}

/// Error from [`Relation::add_row`]: the tuple does not fit this relation's
/// schema, so the insertion is refused rather than panicking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RowError {
    /// The tuple has a different number of values than the relation has
    /// columns.
    Arity { expected: usize, found: usize },
    /// The `col`-th value has a different kind than the values already in
    /// that column. `expected` is the column's kind ("int" or "str") and
    /// `found` the new value's kind.
    Type {
        col: usize,
        name: String,
        expected: &'static str,
        found: &'static str,
    },
}

impl fmt::Display for RowError {
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

#[derive(Debug, Clone, PartialEq)]
pub struct Relation {
    schema: Arc<[String]>,
    rows: HashSet<Row>,
}

impl Relation {
    /// A new, empty relation with the given column names.
    pub fn new<S, I>(schema: S) -> Self
    where
        S: IntoIterator<Item = I>,
        I: Into<String>,
    {
        Relation {
            schema: schema.into_iter().map(Into::into).collect(),
            rows: HashSet::new(),
        }
    }

    /// Returns the column names of this relation.
    pub fn schema(&self) -> &[String] {
        self.schema.as_ref()
    }

    /// Returns the number of columns in this relation.
    pub fn arity(&self) -> usize {
        self.schema.len()
    }

    /// Returns the number of rows in this relation.
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Returns `true` if this relation contains no rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Is `row` a tuple of this relation? O(1) hash lookup, no arity or
    /// type checking: a row of the wrong shape is simply not present.
    pub fn contains(&self, row: impl Into<Row>) -> bool {
        let row = row.into();
        self.rows.contains(&row)
    }

    /// Iterate over the tuples of this relation, in arbitrary set order,
    /// yielding each one as a [`Row`] that shares this relation's storage.
    pub fn iter(&self) -> impl Iterator<Item = Row> + '_ {
        self.rows.iter().cloned()
    }

    /// Add one tuple to this relation.
    ///
    /// Returns `Ok(true)` if the tuple was newly inserted, `Ok(false)` if it
    /// was already present, and `Err` if the tuple would break the schema: wrong
    /// arity, or a value whose type differs from the column's existing
    /// values.
    pub fn push(&mut self, row: impl Into<Row>) -> Result<bool, RowError> {
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
                        name: self.schema[i].clone(),
                        expected: prev[i].kind(),
                        found: value.kind(),
                    });
                }
            }
        }
        Ok(self.rows.insert(row))
    }

    /// Keep the tuples for which `cond` returns `Ok(true)`.
    ///
    /// `cond` receives a whole tuple and may return an error — for example
    /// an int/string comparison (§4.3, case #22) — which aborts the
    /// selection. The schema is unchanged.
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
            rows,
        })
    }

    /// Keep only the columns listed in `cols`, in the listed order. The
    /// output column names are the listed names, exactly as written: a
    /// qualified name keeps its qualifier, an unqualified name stays bare.
    ///
    /// Each listed name is resolved against this relation's schema — a
    /// qualified name must match exactly, an unqualified name may match the
    /// bare attribute — and listing the same column twice is an error
    /// (case #24). Duplicate tuples are removed: projection has set
    /// semantics (case #23).
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
            let idx = resolve_attr(&name, &self.schema)?;
            if !seen_idx.insert(idx) {
                return Err(SemanticError::DuplicateProjectedAttribute { name });
            }
            schema.push(name);
            indices.push(idx);
        }
        Ok(Relation {
            schema: Arc::from(schema),
            rows: self
                .rows
                .iter()
                .map(|row| Row::from(indices.iter().map(|&i| row[i].clone()).collect::<Vec<_>>()))
                .collect(),
        })
    }

    /// Rename every column to `<new_name>.<bare name>`, stripping any
    /// existing qualifier first. Tuples are unchanged.
    pub fn rename(self, new_name: &str) -> Self {
        let schema: Arc<[String]> = self
            .schema
            .iter()
            .map(|c| {
                let bare = c.rsplit_once('.').map(|(_, b)| b).unwrap_or(c.as_str());
                format!("{new_name}.{bare}")
            })
            .collect();
        Relation {
            schema,
            rows: self.rows,
        }
    }

    /// The cartesian product with `other` (spec §4.3: times). The output
    /// schema is the concatenation of both schemas; a colliding qualified
    /// column name is an error.
    pub fn times(&self, other: &Relation) -> Result<Self, SemanticError> {
        let schema: Arc<[String]> = self.schema.iter().chain(other.schema.iter()).cloned().collect();
        check_col_duplicates(&schema)?;
        let mut rows = HashSet::new();
        for lr in &self.rows {
            for rr in &other.rows {
                let mut row: Vec<Value> = lr.iter().cloned().collect();
                row.extend(rr.iter().cloned());
                rows.insert(Row::from(row));
            }
        }
        Ok(Relation { schema, rows })
    }

    /// The join with `other` (spec §4.3: join is times followed by
    /// selection): every pair of tuples for which `cond` returns `Ok(true)`.
    ///
    /// `cond` receives one tuple from each side, in that order, and may
    /// error (e.g. int/string comparison), which aborts the join. The output
    /// schema is the concatenation of both schemas.
    pub fn join<F>(&self, other: &Relation, mut cond: F) -> Result<Self, SemanticError>
    where
        F: FnMut(&Row, &Row) -> Result<bool, SemanticError>,
    {
        let schema: Arc<[String]> = self.schema.iter().chain(other.schema.iter()).cloned().collect();
        check_col_duplicates(&schema)?;
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

    /// Set union with `other`: keeps the left schema.
    pub fn union(&self, other: &Relation) -> Result<Self, SemanticError> {
        self.check_compatible(other)?;
        Ok(Relation {
            schema: self.schema.clone(),
            rows: self.rows.union(&other.rows).cloned().collect(),
        })
    }

    /// Set difference: the tuples of `self` not present in `other`.
    pub fn minus(&self, other: &Relation) -> Result<Self, SemanticError> {
        self.check_compatible(other)?;
        Ok(Relation {
            schema: self.schema.clone(),
            rows: self.rows.difference(&other.rows).cloned().collect(),
        })
    }

    /// Set intersection with `other`.
    pub fn intersect(&self, other: &Relation) -> Result<Self, SemanticError> {
        self.check_compatible(other)?;
        Ok(Relation {
            schema: self.schema.clone(),
            rows: self.rows.intersection(&other.rows).cloned().collect(),
        })
    }

    /// Set-op compatibility: same arity, same schema names, and compatible
    /// types position by position. Both operands are homogeneous per column
    /// (`push` keeps that invariant), so one representative tuple from each
    /// side is enough to compare every column's type — O(arity), not
    /// O(rows). An operand with no tuples carries no type information and
    /// stays compatible with anything.
    fn check_compatible(&self, other: &Relation) -> Result<(), SemanticError> {
        if self.schema.len() != other.schema.len() {
            return Err(SemanticError::SchemaMismatch {
                detail: format!(
                    "{} attributes on left, {} on right",
                    self.schema.len(),
                    other.schema.len()
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
                            self.schema[i],
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
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let names: Vec<&str> = self.schema.iter().map(|s| s.as_str()).collect();
        writeln!(f, "{}", names.join(", "))?;
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
// Resolved conditions (private)
// =====================================================================

#[derive(Debug, Clone)]
enum RExpr {
    Col(usize),
    Int(i64),
    Str(String),
}

impl RExpr {
    fn resolve(operand: &Operand, schema: &[String]) -> Result<Self, SemanticError> {
        match operand {
            Operand::Num(i) => Ok(RExpr::Int(*i)),
            Operand::Str(s) => Ok(RExpr::Str(s.clone())),
            Operand::Attr(name) => {
                let idx = resolve_attr(name, schema)?;
                Ok(RExpr::Col(idx))
            }
        }
    }

    /// The value this expression denotes at evaluation time: a column read
    /// from `left` or `right` (an index below `base` reads `left`, anything
    /// else reads `right`), or the constant itself. The condition tree owns
    /// the string constants, so nothing is copied or allocated while a
    /// condition is evaluated.
    fn value<'a>(&'a self, left: &'a [Value], right: &'a [Value], base: usize) -> Scalar<'a> {
        match self {
            RExpr::Col(i) => {
                let v = if *i < base { &left[*i] } else { &right[*i - base] };
                match v {
                    Value::Int(x) => Scalar::Int(*x),
                    Value::Str(s) => Scalar::Str(s.as_str()),
                }
            }
            RExpr::Int(i) => Scalar::Int(*i),
            RExpr::Str(s) => Scalar::Str(s.as_str()),
        }
    }
}

/// The one thing condition evaluation needs from either side of a
/// comparison: an int or a borrowed string. Column values are borrowed
/// from the tuple under examination; string constants are borrowed from
/// the condition tree. Building or comparing one of these never clones
/// a [`Value`] or allocates.
#[derive(Debug, Clone, Copy)]
enum Scalar<'a> {
    Int(i64),
    Str(&'a str),
}

impl Scalar<'_> {
    fn kind(&self) -> &'static str {
        match self {
            Scalar::Int(_) => "int",
            Scalar::Str(_) => "str",
        }
    }

    /// Three-way compare with `other` (`a.cmp(b)`) and test whether the
    /// result is exactly `want`. An int against a string is a
    /// [`SemanticError::TypeError`], never a silent `false` (spec §4.3,
    /// case #22).
    fn matches(&self, want: Ordering, other: Scalar<'_>) -> Result<bool, SemanticError> {
        match (*self, other) {
            (Scalar::Int(x), Scalar::Int(y)) => Ok(x.cmp(&y) == want),
            (Scalar::Str(x), Scalar::Str(y)) => Ok(x.cmp(y) == want),
            (a, b) => Err(SemanticError::TypeError {
                detail: format!("cannot compare {} with {}", a.kind(), b.kind()),
            }),
        }
    }
}

#[derive(Debug, Clone)]
enum RCond {
    /// `a op b` compiled to a three-way match: `a.cmp(b) == want`
    /// (grammar ops `<`, `=`, `>`).
    Cmp(RExpr, RExpr, Ordering),
    /// Compiled to the complement: `a.cmp(b) != want`
    /// (grammar ops `>=`, `<=`, `!=`).
    InvertCmp(RExpr, RExpr, Ordering),
    And(Box<RCond>, Box<RCond>),
    Or(Box<RCond>, Box<RCond>),
    Not(Box<RCond>),
}

impl RCond {
    fn resolve(predicate: &Predicate, schema: &[String]) -> Result<Self, SemanticError> {
        match predicate {
            Predicate::Compare { left, op, right } => {
                let l = RExpr::resolve(left, schema)?;
                let r = RExpr::resolve(right, schema)?;
                // The six grammar operators compile to a three-way `Ordering`
                // "want" plus an optional not-flag. `<`, `=`, `>` match an
                // ordering directly; the other three match its complement:
                //   a >= b ⇔ a.cmp(b) != Less,  a <= b ⇔ a.cmp(b) != Greater,
                //   a != b ⇔ a.cmp(b) != Equal.
                let (want, negate) = match op {
                    CompareOp::Lt => (Ordering::Less, false),
                    CompareOp::Eq => (Ordering::Equal, false),
                    CompareOp::Gt => (Ordering::Greater, false),
                    CompareOp::Ge => (Ordering::Less, true),
                    CompareOp::Le => (Ordering::Greater, true),
                    CompareOp::Ne => (Ordering::Equal, true),
                };
                Ok(if negate {
                    RCond::InvertCmp(l, r, want)
                } else {
                    RCond::Cmp(l, r, want)
                })
            }
            Predicate::And(a, b) => Ok(RCond::And(
                Box::new(RCond::resolve(a, schema)?),
                Box::new(RCond::resolve(b, schema)?),
            )),
            Predicate::Or(a, b) => Ok(RCond::Or(
                Box::new(RCond::resolve(a, schema)?),
                Box::new(RCond::resolve(b, schema)?),
            )),
            Predicate::Not(a) => Ok(RCond::Not(Box::new(RCond::resolve(a, schema)?))),
        }
    }

    fn eval_single(&self, row: &Row) -> Result<bool, SemanticError> {
        match self {
            RCond::Cmp(l, r, want) => {
                l.value(row, row, row.len()).matches(*want, r.value(row, row, row.len()))
            }
            RCond::InvertCmp(l, r, want) => {
                Ok(!l.value(row, row, row.len()).matches(*want, r.value(row, row, row.len()))?)
            }
            RCond::And(a, b) => Ok(a.eval_single(row)? && b.eval_single(row)?),
            RCond::Or(a, b) => Ok(a.eval_single(row)? || b.eval_single(row)?),
            RCond::Not(a) => Ok(!a.eval_single(row)?),
        }
    }

    fn eval_pair(
        &self,
        left: &Row,
        right: &Row,
        base: usize,
    ) -> Result<bool, SemanticError> {
        match self {
            RCond::Cmp(l, r, want) => {
                l.value(left, right, base).matches(*want, r.value(left, right, base))
            }
            RCond::InvertCmp(l, r, want) => Ok(!l
                .value(left, right, base)
                .matches(*want, r.value(left, right, base))?),
            RCond::And(a, b) => {
                Ok(a.eval_pair(left, right, base)? && b.eval_pair(left, right, base)?)
            }
            RCond::Or(a, b) => {
                Ok(a.eval_pair(left, right, base)? || b.eval_pair(left, right, base)?)
            }
            RCond::Not(a) => Ok(!a.eval_pair(left, right, base)?),
        }
    }
}

// =====================================================================
// Engine — schema helpers (private)
// =====================================================================

/// Resolve an attribute name against a schema, allowing qualified names
/// (`Emp.DID` exact match) and unqualified names (exact match or suffix
/// match after a dot).
fn resolve_attr(name: &str, schema: &[String]) -> Result<usize, SemanticError> {
    let matches: Vec<usize> = if name.contains('.') {
        // Qualified: exact match only.
        schema
            .iter()
            .enumerate()
            .filter(|(_, c)| c.as_str() == name)
            .map(|(i, _)| i)
            .collect()
    } else {
        // Unqualified: exact match OR suffix after dot matches.
        let suffix = format!(".{name}");
        schema
            .iter()
            .enumerate()
            .filter(|(_, c)| c.as_str() == name || c.ends_with(&suffix))
            .map(|(i, _)| i)
            .collect()
    };
    match matches.len() {
        1 => Ok(matches[0]),
        0 => Err(SemanticError::UnknownAttribute {
            name: name.to_string(),
        }),
        _ => Err(SemanticError::AmbiguousAttribute {
            name: name.to_string(),
        }),
    }
}

/// The relation name that qualifies the columns of this expression's result,
/// if any. `None` means the result's columns are already fully qualified
/// (join/times outputs), so no further qualification is needed.
fn qualifier_of(expr: &Query) -> Option<&str> {
    match expr {
        Query::Variable(name) => Some(name),
        Query::Rename { new_name, .. } => Some(new_name),
        Query::Select { input, .. } | Query::Project { input, .. } => qualifier_of(input),
        Query::Union { left, .. } | Query::Intersect { left, .. } | Query::Minus { left, .. } => {
            qualifier_of(left)
        }
        Query::Join { .. } | Query::Times { .. } => None,
    }
}

/// Column names of a relation qualified with its relation name (spec §4.3:
/// times/join qualify ALL attributes by relation name). Names already
/// qualified with `name` are left untouched; `None` leaves every name as-is.
fn qualified_schema(name: Option<&str>, schema: &[String]) -> Vec<String> {
    let Some(name) = name else {
        return schema.to_vec();
    };
    let prefix = format!("{name}.");
    schema
        .iter()
        .map(|c| {
            if c.starts_with(&prefix) || c.as_str() == name {
                c.clone()
            } else {
                format!("{name}.{c}")
            }
        })
        .collect()
}

/// Rebuild a relation with every column qualified with `name` (spec §4.3:
/// the inputs of times/join are fully qualified before the operation runs).
/// Rows are untouched.
fn qualify(rel: Relation, name: Option<&str>) -> Relation {
    Relation {
        schema: Arc::from(qualified_schema(name, &rel.schema)),
        rows: rel.rows,
    }
}

fn check_col_duplicates(cols: &[String]) -> Result<(), SemanticError> {
    let mut seen = HashSet::new();
    for c in cols {
        if !seen.insert(c.clone()) {
            return Err(SemanticError::SchemaMismatch {
                detail: format!("duplicate qualified column '{c}'"),
            });
        }
    }
    Ok(())
}

// =====================================================================
// Engine
// =====================================================================

/// Counters for section 8.2.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    pub join_comparisons: u64,
    pub select_comparisons: u64,
}

pub struct Engine {
    relations: HashMap<String, Relation>,
    pub stats: Stats,
}

impl Default for Engine {
    fn default() -> Self {
        Self::new()
    }
}

impl Engine {
    pub fn new() -> Self {
        Engine {
            relations: HashMap::new(),
            stats: Stats::default(),
        }
    }

    pub fn load(&mut self, name: &str, relation: Relation) {
        self.relations.insert(name.to_string(), relation);
    }

    pub fn get(&self, name: &str) -> Option<&Relation> {
        self.relations.get(name)
    }

    pub fn reset_stats(&mut self) {
        self.stats = Stats::default();
    }

    pub fn execute(&mut self, expr: &Query) -> Result<Relation, SemanticError> {
        match expr {
            Query::Variable(name) => self
                .relations
                .get(name)
                .cloned()
                .ok_or_else(|| SemanticError::UnknownRelation { name: name.clone() }),

            Query::Select { predicate, input } => {
                let relation = self.execute(input)?;
                let rc = RCond::resolve(predicate, &relation.schema)?;
                relation.select(move |row| {
                    self.stats.select_comparisons += 1;
                    rc.eval_single(row)
                })
            }

            Query::Project { attrs, input } => {
                let relation = self.execute(input)?;
                relation.project(attrs)
            }

            Query::Rename { new_name, input } => {
                let relation = self.execute(input)?;
                Ok(relation.rename(new_name))
            }

            Query::Join {
                condition,
                left,
                right,
            } => {
                let left = qualify(self.execute(left)?, qualifier_of(left));
                let right = qualify(self.execute(right)?, qualifier_of(right));
                // The condition names resolve against the combined schema. The
                // duplicate-column check runs here, before resolution, so a
                // colliding qualified name fails as a schema error rather
                // than as an ambiguous attribute.
                let combined: Arc<[String]> = left.schema.iter().chain(right.schema.iter()).cloned().collect();
                check_col_duplicates(&combined)?;
                let rc = RCond::resolve(condition, &combined)?;
                let base = left.schema.len();
                left.join(&right, move |lr, rr| {
                    self.stats.join_comparisons += 1;
                    rc.eval_pair(lr, rr, base)
                })
            }

            Query::Times { left, right } => {
                let left = qualify(self.execute(left)?, qualifier_of(left));
                let right = qualify(self.execute(right)?, qualifier_of(right));
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
        let mut rel = Relation::new(["a", "b"]);
        for i in 0..n {
            rel.push([Value::Int(i), Value::Int(i)])
                .expect("static test data is well-formed");
        }
        rel
    }

    // ── §4.1 load-time row checks (spec §1.2: a column's type is the type
    //    of its values, and a relation is a set) ─────────────────────────

    #[test]
    fn add_row_returns_inserted_status_and_schema_errors() {
        let mut rel = Relation::new(["a", "b"]);

        // First row: no type checking needed, and it is newly inserted.
        assert_eq!(rel.push([Value::Int(1), Value::Int(2)]), Ok(true));

        // The same tuple again: already present, so not inserted (set
        // semantics — §1.2 duplicates collapse).
        assert_eq!(rel.push([Value::Int(1), Value::Int(2)]), Ok(false));

        // Wrong arity is refused, not panicked.
        assert_eq!(
            rel.push([Value::Int(1)]),
            Err(RowError::Arity {
                expected: 2,
                found: 1
            })
        );

        // A value whose kind differs from the kinds already in its column is
        // refused; the error names the column and both kinds.
        match rel.push([Value::Str("x".into()), Value::Int(3)]) {
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

    #[test]
    fn union_of_same_name_columns_with_different_types_is_type_error() {
        // §4.3: comparison types must be compatible position by position. Both
        // sides are homogeneous per column, so a single representative tuple
        // from each side triggers the error — no row content beyond types is
        // involved.
        let mut eng = Engine::new();
        let mut r = Relation::new(["X"]);
        r.push([Value::Int(1)])
            .expect("static test data is well-formed");
        eng.load("R", r);
        let mut s = Relation::new(["X"]);
        s.push([Value::Str("a".into())])
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

    #[test]
    fn union_with_an_empty_operand_does_not_fail_the_type_check() {
        // An operand with no tuples carries no type information, so it stays
        // compatible with a typed operand — the representative-row check has
        // nothing to compare, exactly like the old sampling check.
        let mut eng = Engine::new();
        eng.load("E", Relation::new(["X"]));
        let mut i = Relation::new(["X"]);
        i.push([Value::Int(1)])
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

    #[test]
    fn join_counts_pairs_even_when_nothing_matches() {
        let mut eng = Engine::new();
        // Relation is a set: tuples must be distinct to keep 10 rows per side.
        let mut r = Relation::new(["a", "b"]);
        for i in 0..10 {
            r.push([Value::Int(i), Value::Int(i)])
                .expect("static test data is well-formed");
        }
        eng.load("R", r);
        let mut s = Relation::new(["b", "c"]);
        for i in 0..10 {
            s.push([Value::Int(10 + i), Value::Int(10 + i)])
                .expect("static test data is well-formed");
        }
        eng.load("S", s);
        let expr = parse_query("R join[R.b=S.b] S").unwrap();
        let result = eng.execute(&expr).expect("join should run");
        assert_eq!(result.len(), 0);
        assert_eq!(eng.stats.join_comparisons, 100, "100 = 10 × 10 pairs");
    }

    // ── condition evaluation: int/string type errors ────────────────────
    // The borrow-based condition evaluator must keep the spec rule: an
    // int-versus-string comparison is a TypeError, never a silent `false`
    // (spec §4.3, case #22).

    #[test]
    fn join_comparing_int_column_to_str_column_is_type_error() {
        let mut r = Relation::new(["a", "b"]);
        r.push([Value::Int(1), Value::Int(10)])
            .expect("static test data is well-formed");
        let mut s = Relation::new(["b", "c"]);
        s.push([Value::Str("x".into()), Value::Str("y".into())])
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

    #[test]
    fn all_six_compare_operators_select_the_right_rows() {
        // Pins the operator → `Ordering` mapping compiled by `RCond::resolve`
        // (`<` `=` `>` → `Cmp`, `>=` `<=` `!=` → `InvertCmp`). Any wrong
        // mapping shows up as a wrong row set here.
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
        let mut s = Relation::new(["c"]);
        for c in ["p", "q", "r"] {
            s.push([Value::Str(c.into())])
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
}
