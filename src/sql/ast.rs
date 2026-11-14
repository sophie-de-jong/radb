//! The SQL abstract syntax tree.
//!
//! One [`Statement`] per semicolon-separated statement: a `CREATE TABLE`, an
//! `INSERT`, or a [`Query`]. The tree is deliberately shallow and close to the
//! grammar of §2. Nodes that need a position carry it through [`Located`], so
//! the AST structs themselves have no position fields.
//!
//! The parser does no name resolution: a [`ColumnRef`] is still the name exactly
//! as it was written. The compiler resolves names and builds the logical plan in
//! the same pass, so the tree carries no semantic annotations.

use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::fmt;
use std::ops::{Deref, DerefMut};

use super::tokenizer::Position;
use crate::Value;
use crate::exec::Direction;
use crate::exec::plan::Aggregate;
use crate::schema::ColumnType;

/// An AST node together with the position it started at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located<T> {
    /// The node.
    node: T,
    /// Where it started.
    at: Position,
}

impl<T> Located<T> {
    /// Wrap `node` at `at`.
    pub fn new(node: T, at: Position) -> Self {
        Located { node, at }
    }

    pub fn at(&self) -> Position {
        self.at
    }

    pub fn into_inner(self) -> T {
        self.node
    }
}

impl<T> Deref for Located<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.node
    }
}

impl<T> DerefMut for Located<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.node
    }
}

impl<T> AsRef<T> for Located<T> {
    fn as_ref(&self) -> &T {
        &self.node
    }
}

impl<T> AsMut<T> for Located<T> {
    fn as_mut(&mut self) -> &mut T {
        &mut self.node
    }
}

impl<T: fmt::Display> fmt::Display for Located<T> {
    /// A located node prints as the node alone; the position is for errors.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.node.fmt(f)
    }
}

/// One statement of a SQL file or shell command.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `CREATE TABLE name (coldef, ...)`.
    Create(Located<CreateTable>),
    /// `INSERT INTO name VALUES (...)`.
    Insert(Located<Insert>),
    /// A query, including set operations.
    Query(Query),
}

/// `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub struct CreateTable {
    /// The table name.
    pub name: Located<String>,
    /// The columns, in order. Never empty (the grammar requires one).
    pub columns: Vec<ColumnDef>,
}

/// One `coldef` of a `CREATE TABLE`.
#[derive(Debug, Clone, PartialEq)]
pub struct ColumnDef {
    /// The column name.
    pub name: Located<String>,
    /// The column type.
    pub ty: ColumnType,
}

/// `INSERT INTO`.
#[derive(Debug, Clone, PartialEq)]
pub struct Insert {
    /// The target table.
    pub table: Located<String>,
    /// The row's literals, in order.
    pub values: Vec<Located<Value>>,
}

// =====================================================================
// Queries
// =====================================================================

/// A query: one or more select blocks combined by set operators, left to
/// right. `QUERY ::= select_block { setop select_block }`.
#[derive(Debug, Clone, PartialEq)]
pub struct Query {
    /// The first block.
    pub first: SelectBlock,
    /// Each following set operator and its right-hand block, in order.
    pub rest: Vec<(SetOp, SelectBlock)>,
}

/// A set operator between two blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SetOp {
    /// `UNION`.
    Union,
    /// `UNION ALL`.
    UnionAll,
    /// `INTERSECT`.
    Intersect,
    /// `EXCEPT`.
    Except,
}

/// One operand of a set operator: a `SELECT` with its clauses, or a
/// parenthesised query.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum SelectBlock {
    /// A `SELECT ... FROM ...` block.
    Select(Located<SelectStmt>),
    /// A parenthesised query used as a block.
    Sub(Box<Query>),
}

/// A `SELECT` block, with every clause of §2 in order.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectStmt {
    /// Whether `DISTINCT` was written.
    pub distinct: bool,
    /// The select list: `*`, or the items.
    pub items: SelectItems,
    /// The `FROM` list, in order.
    pub from: Vec<FromItem>,
    /// `WHERE`, if written.
    pub where_: Option<Condition>,
    /// `GROUP BY`, in order (empty if absent).
    pub group_by: Vec<Located<ColumnRef>>,
    /// `HAVING`, if written.
    pub having: Option<Condition>,
    /// `ORDER BY`, in order (empty if absent).
    pub order_by: Vec<OrderItem>,
    /// `LIMIT`, if written.
    pub limit: Option<u64>,
}

/// The select list: `*`, or a non-empty list of items.
#[derive(Debug, Clone, PartialEq)]
pub enum SelectItems {
    /// `SELECT *`.
    Star,
    /// `SELECT item, item, ...`.
    List(Vec<SelectItem>),
}

/// One select item: a column or an aggregate, with an optional alias.
#[derive(Debug, Clone, PartialEq)]
pub struct SelectItem {
    /// The column or aggregate.
    pub expr: Expr,
    /// `AS alias`, if written.
    pub alias: Option<Located<String>>,
}

/// An expression that may appear in a select list: a column or an aggregate.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    /// A column reference, qualified or not.
    Column(Located<ColumnRef>),
    /// An aggregate function call.
    Aggregate(Located<AggregateExpr>),
}

/// An aggregate call: a function and its argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AggregateExpr {
    /// The function.
    pub func: Aggregate,
    /// The argument: `*`, or a column.
    pub arg: AggregateArgument,
}

/// The argument of an aggregate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AggregateArgument {
    /// `*`, legal only for `COUNT`.
    Star,
    /// A column reference.
    Column(Located<ColumnRef>),
}

impl fmt::Display for AggregateArgument {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AggregateArgument::Star => write!(f, "*"),
            AggregateArgument::Column(col) => write!(f, "{}", col.as_ref()),
        }
    }
}

/// One `FROM` entry: a table reference, possibly with `JOIN ... ON` clauses
/// chained onto it. A comma in the `FROM` list is a separate [`FromItem`].
#[derive(Debug, Clone, PartialEq)]
pub enum FromItem {
    /// A bare table reference.
    Table(TableRef),
    /// `left JOIN right ON condition`.
    ///
    /// The condition is boxed so a `FromItem` stays small: a [`Condition`] is
    /// by far the largest thing a `FROM` entry carries.
    Join {
        /// The already-parsed left side.
        left: Box<FromItem>,
        /// The right table reference.
        right: TableRef,
        /// The join condition.
        on: Box<Condition>,
    },
}

/// A table reference: a base table or a parenthesised subquery, each with an
/// optional alias.
#[derive(Debug, Clone, PartialEq)]
pub enum TableRef {
    /// A base table, by name. `alias` is `AS name` if written.
    Base {
        /// The table name.
        name: Located<String>,
        /// The alias, if written.
        alias: Option<Located<String>>,
    },
    /// A parenthesised subquery. The grammar requires an alias.
    Subquery {
        /// The subquery.
        query: Box<Query>,
        /// The alias (required).
        alias: Located<String>,
    },
}

/// One `ORDER BY` item.
#[derive(Debug, Clone, PartialEq)]
pub struct OrderItem {
    /// The sort key, as written.
    pub key: Located<ColumnRef>,
    /// The direction (`ASC` is the default).
    pub direction: Direction,
}

// =====================================================================
// Conditions and names
// =====================================================================

/// A condition: one comparison, combined with `AND`, `OR` and `NOT`.
///
/// An operand may be an aggregate, because `HAVING` may compare one. `WHERE` is
/// where the parser and the resolver reject an aggregate; the tree can represent
/// it so the error can name it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Condition {
    /// `left op right`.
    Compare(Located<Compare>),
    /// Both sides must hold.
    And(Box<Condition>, Box<Condition>),
    /// Either side must hold.
    Or(Box<Condition>, Box<Condition>),
    /// Negation.
    Not(Box<Condition>),
}

/// One comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Compare {
    /// The left operand.
    pub left: Operand,
    /// The comparison operator.
    pub op: CompareOp,
    /// The right operand.
    pub right: Operand,
}

impl Condition {
    /// A tautology, for a join that has no predicate.
    pub fn always_true() -> Self {
        Condition::Compare(Located::new(
            Compare {
                left: Operand::Literal(Value::Int(1)),
                op: CompareOp::Eq,
                right: Operand::Literal(Value::Int(1)),
            },
            Position::default(),
        ))
    }

    /// Whether this is an equality between two columns, or a conjunction of
    /// such equalities — the shape a hash or sort-merge join can serve.
    pub fn is_equality(&self) -> bool {
        match self {
            Condition::And(a, b) => a.is_equality() && b.is_equality(),
            Condition::Compare(compare) => {
                matches!(
                    (&compare.left, compare.op, &compare.right),
                    (Operand::Column(_), CompareOp::Eq, Operand::Column(_))
                )
            }
            _ => false,
        }
    }

    /// The fully qualified column names the condition references.
    pub fn columns(&self) -> HashSet<String> {
        let mut out = HashSet::new();
        self.collect_columns(&mut out);
        out
    }

    fn collect_columns(&self, out: &mut HashSet<String>) {
        match self {
            Condition::Compare(compare) => {
                for operand in [&compare.left, &compare.right] {
                    if let Operand::Column(col) = operand {
                        out.insert(col.to_string());
                    }
                }
            }
            Condition::And(a, b) | Condition::Or(a, b) => {
                a.collect_columns(out);
                b.collect_columns(out);
            }
            Condition::Not(inner) => inner.collect_columns(out),
        }
    }

    /// A copy of this condition with every column operand renamed by `mapping`.
    ///
    /// The optimizer uses this when a logical output alias (`t.n`, `COUNT(*) AS
    /// n`) must become the name the physical operator actually produces.
    pub fn rename_columns(&self, mapping: &HashMap<String, String>) -> Condition {
        match self {
            Condition::Compare(compare) => {
                let mut inner = compare.as_ref().clone();
                inner.left = rename_operand(&inner.left, mapping);
                inner.right = rename_operand(&inner.right, mapping);
                Condition::Compare(Located::new(inner, compare.at()))
            }
            Condition::And(left, right) => Condition::And(
                Box::new(left.rename_columns(mapping)),
                Box::new(right.rename_columns(mapping)),
            ),
            Condition::Or(left, right) => Condition::Or(
                Box::new(left.rename_columns(mapping)),
                Box::new(right.rename_columns(mapping)),
            ),
            Condition::Not(inner) => Condition::Not(Box::new(inner.rename_columns(mapping))),
        }
    }
}

/// Rename one operand's column through `mapping`.
fn rename_operand(operand: &Operand, mapping: &HashMap<String, String>) -> Operand {
    match operand {
        Operand::Column(column) => {
            let name = mapping
                .get(&column.to_string())
                .cloned()
                .unwrap_or_else(|| column.to_string());
            let (qualifier, name) = match name.rsplit_once('.') {
                Some((qualifier, name)) => (Some(qualifier.to_string()), name.to_string()),
                None => (None, name),
            };
            Operand::Column(Located::new(ColumnRef { qualifier, name }, column.at()))
        }
        Operand::Literal(value) => Operand::Literal(value.clone()),
        Operand::Aggregate(aggregate) => Operand::Aggregate(aggregate.clone()),
    }
}

impl fmt::Display for Condition {
    /// Render the JSON wire form: `left op right`, lower-case `and`/`or`/`not`,
    /// with parentheses only where the tree would otherwise change shape.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Condition::Compare(compare) => {
                write!(f, "{} {} {}", compare.left, compare.op, compare.right)
            }
            Condition::Not(inner) => {
                f.write_str("not ")?;
                match inner.as_ref() {
                    Condition::And(_, _) | Condition::Or(_, _) => write!(f, "({inner})"),
                    _ => write!(f, "{inner}"),
                }
            }
            Condition::And(left, right) => {
                write_and_side(f, left)?;
                f.write_str(" and ")?;
                write_and_side(f, right)
            }
            Condition::Or(left, right) => {
                write!(f, "{left} or ")?;
                match right.as_ref() {
                    Condition::Or(_, _) => write!(f, "({right})"),
                    _ => write!(f, "{right}"),
                }
            }
        }
    }
}

/// One side of an `AND`: an `OR` child needs parentheses.
fn write_and_side(f: &mut fmt::Formatter<'_>, condition: &Condition) -> fmt::Result {
    match condition {
        Condition::Or(_, _) => write!(f, "({condition})"),
        _ => write!(f, "{condition}"),
    }
}

/// The six comparison operators.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompareOp {
    /// `=`.
    Eq,
    /// `!=`.
    Ne,
    /// `<`.
    Lt,
    /// `<=`.
    Le,
    /// `>`.
    Gt,
    /// `>=`.
    Ge,
}

impl CompareOp {
    pub fn negate(self) -> CompareOp {
        match self {
            CompareOp::Eq => CompareOp::Ne,
            CompareOp::Ne => CompareOp::Eq,
            CompareOp::Lt => CompareOp::Ge,
            CompareOp::Le => CompareOp::Gt,
            CompareOp::Gt => CompareOp::Le,
            CompareOp::Ge => CompareOp::Lt,
        }
    }
}

impl fmt::Display for CompareOp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CompareOp::Eq => write!(f, "="),
            CompareOp::Ne => write!(f, "<>"),
            CompareOp::Lt => write!(f, "<"),
            CompareOp::Le => write!(f, "<="),
            CompareOp::Gt => write!(f, ">"),
            CompareOp::Ge => write!(f, ">="),
        }
    }
}

/// One side of a comparison.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    /// A column reference.
    Column(Located<ColumnRef>),
    /// An aggregate call.
    Aggregate(Located<AggregateExpr>),
    /// A literal.
    Literal(Value),
}

impl fmt::Display for Operand {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Operand::Column(col) => write!(f, "{}", col.as_ref()),
            Operand::Literal(Value::Int(n)) => write!(f, "{n}"),
            Operand::Literal(Value::Varchar(s)) => write!(f, "'{}'", s.replace('\'', "''")),
            Operand::Aggregate(agg) => write!(f, "{}({})", agg.func, agg.arg),
        }
    }
}

/// A column reference, qualified or not.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ColumnRef {
    /// The qualifier (`e` in `e.name`), if written.
    pub qualifier: Option<String>,
    /// The column name.
    pub name: String,
}

impl fmt::Display for ColumnRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.qualifier {
            Some(qualifier) => write!(f, "{qualifier}.{}", self.name),
            None => write!(f, "{}", self.name),
        }
    }
}
