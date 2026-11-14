//! The streaming SQL compiler: parse, check, and build a logical plan.
//!
//! [`Compiler`] is the public streaming facade. The private [`Planner`] owns
//! name resolution, type and aggregate checking, and plan construction. The AST
//! stays purely syntactic: every resolved name and type lives in the planner's
//! local state and in the plan it emits, never in the parsed tree.
//!
//! The canonical bottom-to-top order of §5 is followed exactly: `FROM`, `WHERE`,
//! `GROUP BY`, `HAVING`, the projection (with `DISTINCT`), `ORDER BY`, `LIMIT`,
//! then set operators between whole blocks.

use crate::exec::plan::Aggregate;
use crate::schema::{Column, ColumnType};
use crate::sql::ast::{
    AggregateArgument, AggregateExpr, ColumnRef, Condition, CreateTable as AstCreateTable, Expr,
    FromItem, Insert as AstInsert, Located, Operand, Query, SelectBlock, SelectItem, SelectItems,
    SelectStmt, SetOp, Statement as AstStatement, TableRef,
};
use crate::sql::error::SqlError;
use crate::sql::parser::Parser;
use crate::sql::plan::Plan;
use crate::sql::resolve::{Scope, requalify};
use crate::sql::tokenizer::Position;
use crate::storage::Catalog;
use crate::{Schema, Value};

/// One checked statement.
#[derive(Debug, Clone, PartialEq)]
pub enum Statement {
    /// `CREATE TABLE`.
    Create { name: String, schema: Schema },
    /// `INSERT`.
    Insert {
        table_name: String,
        values: Vec<Value>,
    },
    /// `SELECT`, lowered to a logical plan.
    Select(Plan),
}

/// A streaming parser, checker, and plan builder.
///
/// The catalog is supplied to each compile call, so the caller can execute a
/// statement before compiling the next one.
pub struct Compiler<'a> {
    parser: Parser<'a>,
}

impl<'a> Compiler<'a> {
    /// Start compiling `input`.
    ///
    /// Errors: the first lexical or syntax [`SqlError`].
    pub fn new(input: &'a str) -> Result<Self, SqlError> {
        Ok(Compiler {
            parser: Parser::new(input)?,
        })
    }

    /// Parse, check, and convert the next statement, or return `None` at end of
    /// input.
    pub fn compile(&mut self, catalog: &Catalog) -> Result<Option<Statement>, SqlError> {
        let Some(statement) = self.parser.next_statement()? else {
            return Ok(None);
        };
        match statement {
            AstStatement::Create(mut create) => Ok(Some(self.build_create(&mut create, catalog)?)),
            AstStatement::Insert(mut insert) => Ok(Some(self.build_insert(&mut insert, catalog)?)),
            AstStatement::Query(query) => Ok(Some(self.build_plan(catalog, &query)?)),
        }
    }

    /// Parse, check, and convert every remaining statement.
    pub fn compile_all(&mut self, catalog: &Catalog) -> Result<Vec<Statement>, SqlError> {
        let mut statements = Vec::new();
        while let Some(statement) = self.compile(catalog)? {
            statements.push(statement);
        }
        Ok(statements)
    }

    /// Compile the next statement, which must be a query.
    pub fn compile_select(&mut self, catalog: &Catalog) -> Result<Plan, SqlError> {
        match self.compile(catalog)? {
            Some(Statement::Select(plan)) => Ok(plan),
            Some(_) | None => Err(SqlError::syntax(
                "expected a SELECT statement",
                self.parser.at(),
            )),
        }
    }

    /// Compile the next statement, which must be a `CREATE TABLE`.
    pub fn compile_create(&mut self, catalog: &Catalog) -> Result<(String, Schema), SqlError> {
        let at = self.parser.at();
        match self.compile(catalog)? {
            Some(Statement::Create { name, schema }) => Ok((name, schema)),
            Some(_) | None => Err(SqlError::syntax("expected a CREATE TABLE statement", at)),
        }
    }

    /// Compile the next statement, which must be an `INSERT`.
    pub fn compile_insert(&mut self, catalog: &Catalog) -> Result<(String, Vec<Value>), SqlError> {
        let at = self.parser.at();
        match self.compile(catalog)? {
            Some(Statement::Insert { table_name, values }) => Ok((table_name, values)),
            Some(_) | None => Err(SqlError::syntax("expected an INSERT statement", at)),
        }
    }

    /// Check `CREATE TABLE` and build its schema.
    fn build_create(
        &mut self,
        create: &mut Located<AstCreateTable>,
        catalog: &Catalog,
    ) -> Result<Statement, SqlError> {
        if catalog.get(&create.name).is_some() {
            return Err(SqlError::schema(
                format!("table '{}' already exists", create.name.as_ref()),
                create.at(),
            ));
        }
        let at = create.at();
        let name = create.name.as_ref().clone();
        let schema = Schema::new(
            create
                .columns
                .iter()
                .map(|column| Column {
                    name: column.name.as_ref().clone(),
                    ty: column.ty,
                })
                .collect(),
        )
        .map_err(|e| SqlError::schema(e.to_string(), at))?;
        Ok(Statement::Create { name, schema })
    }

    /// Check `INSERT` and collect its row.
    fn build_insert(
        &mut self,
        insert: &mut Located<AstInsert>,
        catalog: &Catalog,
    ) -> Result<Statement, SqlError> {
        let meta = catalog.get(&insert.table).ok_or_else(|| {
            SqlError::name(
                format!("unknown table '{}'", insert.table.as_ref()),
                insert.table.at(),
            )
        })?;
        if insert.values.len() != meta.schema.arity() {
            return Err(SqlError::schema(
                format!(
                    "table '{}' has {} column(s), but {} value(s) were given",
                    insert.table.as_ref(),
                    meta.schema.arity(),
                    insert.values.len()
                ),
                insert.at(),
            ));
        }
        for (literal, column) in insert.values.iter().zip(meta.schema.columns()) {
            let matches = matches!(
                (literal.as_ref(), column.ty),
                (Value::Int(..), ColumnType::Int) | (Value::Varchar(..), ColumnType::Varchar(_))
            );
            if !matches {
                return Err(SqlError::type_(
                    format!(
                        "column '{}' is {}, but a {} value was given",
                        column.name,
                        column.ty,
                        literal.kind()
                    ),
                    literal.at(),
                ));
            }
        }
        Ok(Statement::Insert {
            table_name: insert.table.as_ref().clone(),
            values: insert
                .values
                .iter()
                .map(|value| value.as_ref().clone())
                .collect(),
        })
    }

    /// Lower a checked query to a logical plan.
    fn build_plan(&mut self, catalog: &Catalog, query: &Query) -> Result<Statement, SqlError> {
        let planned = Planner { catalog }.query(query)?;
        Ok(Statement::Select(planned.plan))
    }
}

// =====================================================================
// Planning
// =====================================================================

/// A plan together with the schema it produces.
///
/// The output schema is what a `FROM` subquery contributes to the outer scope;
/// the logical plan itself carries no schema.
struct Planned {
    /// The logical plan tree.
    plan: Plan,
    /// The output columns, in plan order.
    output: Vec<Column>,
}

/// The result of planning one `SELECT` block before `ORDER BY` and `LIMIT`.
struct SelectPlan {
    /// The plan built so far.
    plan: Plan,
    /// Its output columns.
    output: Vec<Column>,
    /// The `project` operator's `(input, output)` columns.
    columns: Vec<(String, String)>,
}

/// The private planner: a borrowed catalog and the checking rules of §3.
struct Planner<'a> {
    catalog: &'a Catalog,
}

impl Planner<'_> {
    /// Plan a whole query, folding set operators left to right (§2, §4).
    fn query(&self, query: &Query) -> Result<Planned, SqlError> {
        let mut planned = self.block(&query.first)?;
        for (op, block) in &query.rest {
            let right = self.block(block)?;
            check_shape(&planned.output, &right.output, block.at())?;
            planned.plan = match op {
                SetOp::UnionAll => Plan::UnionAll {
                    left: Box::new(planned.plan),
                    right: Box::new(right.plan),
                },
                SetOp::Union => Plan::Union {
                    left: Box::new(planned.plan),
                    right: Box::new(right.plan),
                },
                SetOp::Intersect => Plan::Intersect {
                    left: Box::new(planned.plan),
                    right: Box::new(right.plan),
                },
                SetOp::Except => Plan::Except {
                    left: Box::new(planned.plan),
                    right: Box::new(right.plan),
                },
            };
        }
        Ok(planned)
    }

    /// Plan one operand of a set operator.
    fn block(&self, block: &SelectBlock) -> Result<Planned, SqlError> {
        match block {
            SelectBlock::Select(select) => self.select(select),
            SelectBlock::Sub(query) => self.query(query),
        }
    }

    /// Plan a single `SELECT` block in the canonical order of §5.
    fn select(&self, select: &Located<SelectStmt>) -> Result<Planned, SqlError> {
        let mut scope = Scope::new();
        let mut plan = self.from(&select.from, &mut scope)?;

        // 2. WHERE sits directly above the FROM tree, before grouping.
        if let Some(where_) = &select.where_ {
            if let Some(aggregate) = first_aggregate(where_) {
                return Err(SqlError::aggregate(
                    "an aggregate may not appear in WHERE, which runs before grouping",
                    aggregate.at(),
                ));
            }
            let condition = self.condition(where_, &scope, "WHERE", None, None)?;
            plan = Plan::Select {
                condition,
                child: Box::new(plan),
            };
        }

        let aggregating = !select.group_by.is_empty()
            || select.having.is_some()
            || items_have_aggregate(&select.items);

        // 3-5. GROUP BY, HAVING and the projection, with DISTINCT only when it
        // is not already redundant over a grouping.
        let SelectPlan {
            mut plan,
            output,
            columns: project_columns,
        } = if aggregating {
            self.aggregating_select(select, &scope, plan)?
        } else {
            self.plain_select(select, &scope, plan)?
        };

        plan = Plan::Project {
            columns: project_columns,
            child: Box::new(plan),
        };
        if select.distinct && !aggregating {
            plan = Plan::Distinct {
                child: Box::new(plan),
            };
        }

        // 6. ORDER BY, resolved against output aliases first, then the FROM
        // scope; a column need not appear in the select list.
        if !select.order_by.is_empty() {
            let names: Vec<String> = output.iter().map(|column| column.name.clone()).collect();
            let keys = self.order_keys(select, &scope, &names, aggregating)?;
            plan = Plan::Sort {
                keys,
                child: Box::new(plan),
            };
        }

        // 7. LIMIT.
        if let Some(n) = select.limit {
            plan = Plan::Limit {
                n,
                child: Box::new(plan),
            };
        }

        Ok(Planned { plan, output })
    }

    /// Plan a non-aggregating select list and resolve its `ORDER BY` keys.
    fn plain_select(
        &self,
        select: &Located<SelectStmt>,
        scope: &Scope,
        plan: Plan,
    ) -> Result<SelectPlan, SqlError> {
        let items = expand_star(&select.items, scope, select.at());
        let mut output = Vec::with_capacity(items.len());
        let mut columns = Vec::with_capacity(items.len());

        for item in &items {
            let Expr::Column(reference) = &item.expr else {
                // `aggregating` includes any select-list aggregate, so this is
                // unreachable for a well-formed tree.
                unreachable!("a non-aggregating select list has no aggregate");
            };
            let column = scope.resolve(reference)?;
            let name = output_name(&column, item.alias.as_ref());
            columns.push((column.name.clone(), name.clone()));
            output.push(Column {
                name,
                ty: column.ty,
            });
        }

        Ok(SelectPlan {
            plan,
            output,
            columns,
        })
    }

    /// Plan an aggregating select list, adding the `group_by` and `HAVING`
    /// nodes and checking the grouping rules of §3.1.
    fn aggregating_select(
        &self,
        select: &Located<SelectStmt>,
        scope: &Scope,
        mut plan: Plan,
    ) -> Result<SelectPlan, SqlError> {
        let mut group_keys = Vec::with_capacity(select.group_by.len());
        for key in &select.group_by {
            group_keys.push(scope.resolve(key)?.name);
        }

        let items = expand_star(&select.items, scope, select.at());
        let mut aggregates = Aggregates::default();
        let mut output = Vec::with_capacity(items.len());
        let mut columns = Vec::with_capacity(items.len());

        for item in &items {
            match &item.expr {
                Expr::Column(reference) => {
                    let column = scope.resolve(reference)?;
                    if !group_keys.contains(&column.name) {
                        return Err(SqlError::aggregate(
                            format!(
                                "column '{}' must appear in GROUP BY or inside an aggregate",
                                reference.as_ref()
                            ),
                            reference.at(),
                        ));
                    }
                    let name = output_name(&column, item.alias.as_ref());
                    columns.push((column.name.clone(), name.clone()));
                    output.push(Column {
                        name,
                        ty: column.ty,
                    });
                }
                Expr::Aggregate(aggregate) => {
                    let (name, ty) = aggregates.add(aggregate, item.alias.as_ref(), scope)?;
                    columns.push((name.clone(), name.clone()));
                    output.push(Column { name, ty });
                }
            }
        }

        // 4. HAVING sits above the grouping; its aggregates may be ones the
        // select list did not mention, so they are added to the same list
        // before the `group_by` is built.
        let having = match &select.having {
            Some(having) => Some(self.condition(
                having,
                scope,
                "HAVING",
                Some(&group_keys),
                Some(&mut aggregates),
            )?),
            None => None,
        };

        plan = Plan::GroupBy {
            keys: group_keys,
            aggregates: aggregates.specs,
            child: Box::new(plan),
        };
        if let Some(condition) = having {
            plan = Plan::Select {
                condition,
                child: Box::new(plan),
            };
        }

        Ok(SelectPlan {
            plan,
            output,
            columns,
        })
    }

    /// Plan the `FROM` clause: comma entries become left-to-right crosses.
    fn from(&self, items: &[FromItem], scope: &mut Scope) -> Result<Plan, SqlError> {
        let mut iter = items.iter();
        let mut plan = self.plan_from_item(
            iter.next()
                .expect("the grammar requires at least one FROM item"),
            scope,
        )?;
        for item in iter {
            plan = Plan::Cross {
                left: Box::new(plan),
                right: Box::new(self.plan_from_item(item, scope)?),
            };
        }
        Ok(plan)
    }

    /// Plan one `FROM` entry and add its columns to the scope.
    fn plan_from_item(&self, item: &FromItem, scope: &mut Scope) -> Result<Plan, SqlError> {
        match item {
            FromItem::Table(table) => self.table_ref(table, scope),
            FromItem::Join { left, right, on } => {
                let left_plan = self.plan_from_item(left, scope)?;
                let right_plan = self.table_ref(right, scope)?;
                let condition = self.condition(on, scope, "JOIN ... ON", None, None)?;
                Ok(Plan::Join {
                    condition,
                    left: Box::new(left_plan),
                    right: Box::new(right_plan),
                })
            }
        }
    }

    /// Plan one table reference, adding its entry to the scope.
    fn table_ref(&self, table: &TableRef, scope: &mut Scope) -> Result<Plan, SqlError> {
        match table {
            TableRef::Base { name, alias } => {
                let meta = self.catalog.get(name.as_ref()).ok_or_else(|| {
                    SqlError::name(format!("unknown table '{}'", name.as_ref()), name.at())
                })?;
                let key = alias
                    .as_ref()
                    .map_or_else(|| name.as_ref().clone(), |alias| alias.as_ref().clone());
                let columns = requalify(&key, meta.schema.columns().iter().cloned());
                scope.push(key.clone(), columns, name.at())?;
                let scan = Plan::Scan {
                    table: name.as_ref().clone(),
                };
                Ok(match alias {
                    Some(_) => Plan::Rename {
                        to: key,
                        child: Box::new(scan),
                    },
                    None => scan,
                })
            }
            TableRef::Subquery { query, alias } => {
                let planned = self.query(query)?;
                let columns = requalify(alias.as_ref(), planned.output);
                scope.push(alias.as_ref().clone(), columns, alias.at())?;
                Ok(Plan::Rename {
                    to: alias.as_ref().clone(),
                    child: Box::new(planned.plan),
                })
            }
        }
    }

    /// Resolve the `ORDER BY` items to fully qualified or output-alias names.
    fn order_keys(
        &self,
        select: &Located<SelectStmt>,
        scope: &Scope,
        output_names: &[String],
        aggregating: bool,
    ) -> Result<Vec<(String, crate::exec::Direction)>, SqlError> {
        let mut keys = Vec::with_capacity(select.order_by.len());
        for item in &select.order_by {
            let written = item.key.to_string();
            let name = if output_names.iter().any(|name| name == &written) {
                written
            } else {
                let column = scope.resolve(&item.key)?;
                if aggregating {
                    let grouping = select
                        .group_by
                        .iter()
                        .any(|key| scope.resolve(key).is_ok_and(|key| key.name == column.name));
                    if !grouping {
                        return Err(SqlError::aggregate(
                            format!(
                                "ORDER BY '{}' must name an output column or a grouping column",
                                written
                            ),
                            item.key.at(),
                        ));
                    }
                }
                column.name
            };
            keys.push((name, item.direction));
        }
        Ok(keys)
    }

    /// Resolve and type check one condition, rewriting aggregate operands to the
    /// output columns the `group_by` produces.
    fn condition(
        &self,
        condition: &Condition,
        scope: &Scope,
        context: &str,
        group_keys: Option<&[String]>,
        mut aggregates: Option<&mut Aggregates>,
    ) -> Result<Condition, SqlError> {
        match condition {
            Condition::Compare(compare) => {
                let (left, left_kind) = self.operand(
                    &compare.left,
                    scope,
                    context,
                    group_keys,
                    aggregates.as_deref_mut(),
                )?;
                let (right, right_kind) = self.operand(
                    &compare.right,
                    scope,
                    context,
                    group_keys,
                    aggregates.as_deref_mut(),
                )?;
                if left_kind != right_kind {
                    return Err(SqlError::type_(
                        format!(
                            "comparison between {} and {}",
                            left_kind.name(),
                            right_kind.name()
                        ),
                        compare.at(),
                    ));
                }
                Ok(Condition::Compare(Located::new(
                    super::ast::Compare {
                        left,
                        op: compare.op,
                        right,
                    },
                    compare.at(),
                )))
            }
            Condition::And(left, right) => Ok(Condition::And(
                Box::new(self.condition(
                    left,
                    scope,
                    context,
                    group_keys,
                    aggregates.as_deref_mut(),
                )?),
                Box::new(self.condition(right, scope, context, group_keys, aggregates)?),
            )),
            Condition::Or(left, right) => Ok(Condition::Or(
                Box::new(self.condition(
                    left,
                    scope,
                    context,
                    group_keys,
                    aggregates.as_deref_mut(),
                )?),
                Box::new(self.condition(right, scope, context, group_keys, aggregates)?),
            )),
            Condition::Not(inner) => Ok(Condition::Not(Box::new(
                self.condition(inner, scope, context, group_keys, aggregates)?,
            ))),
        }
    }

    /// Resolve one operand and return its comparison kind.
    fn operand(
        &self,
        operand: &Operand,
        scope: &Scope,
        context: &str,
        group_keys: Option<&[String]>,
        aggregates: Option<&mut Aggregates>,
    ) -> Result<(Operand, Kind), SqlError> {
        match operand {
            Operand::Literal(value) => {
                let kind = match value {
                    Value::Int(_) => Kind::Int,
                    Value::Varchar(_) => Kind::Str,
                };
                Ok((Operand::Literal(value.clone()), kind))
            }
            Operand::Column(reference) => {
                let column = scope.resolve(reference)?;
                if let Some(keys) = group_keys
                    && !keys.contains(&column.name)
                {
                    return Err(SqlError::aggregate(
                        format!(
                            "column '{}' must appear in GROUP BY or inside an aggregate",
                            reference.as_ref()
                        ),
                        reference.at(),
                    ));
                }
                let resolved = column_ref(&column.name, reference.at());
                Ok((Operand::Column(resolved), Kind::of(column.ty)))
            }
            Operand::Aggregate(aggregate) => {
                let Some(aggregates) = aggregates else {
                    return Err(SqlError::aggregate(
                        format!("an aggregate may not appear in {context}"),
                        aggregate.at(),
                    ));
                };
                let (name, ty) = aggregates.add(aggregate, None, scope)?;
                let resolved = column_ref(&name, aggregate.at());
                Ok((Operand::Column(resolved), Kind::of(ty)))
            }
        }
    }
}

// =====================================================================
// Aggregates
// =====================================================================

/// The aggregates one query computes, in first-seen order.
///
/// An aggregate is identified by its function and its resolved argument, so
/// `HAVING COUNT(*) > 5` reuses the select list's `COUNT(*)` and its output
/// name rather than computing it twice.
#[derive(Default)]
struct Aggregates {
    /// The `[function, column, output]` triples the `group_by` will carry.
    specs: Vec<(Aggregate, String, String)>,
    /// The same aggregates with their result types.
    entries: Vec<AggregateEntry>,
}

/// One resolved aggregate.
struct AggregateEntry {
    func: Aggregate,
    arg: String,
    output: String,
    ty: ColumnType,
}

impl Aggregates {
    /// Add `aggregate` if it is new, returning its output name and result type.
    ///
    /// Errors: a `type` error for `SUM`/`AVG` over a non-`INT` column, or a
    /// missing column argument.
    fn add(
        &mut self,
        aggregate: &Located<AggregateExpr>,
        alias: Option<&Located<String>>,
        scope: &Scope,
    ) -> Result<(String, ColumnType), SqlError> {
        let (arg, arg_ty) = match &aggregate.arg {
            AggregateArgument::Star => ("*".to_string(), None),
            AggregateArgument::Column(reference) => {
                let column = scope.resolve(reference)?;
                (column.name, Some(column.ty))
            }
        };
        let ty = result_type(aggregate, aggregate.at(), arg_ty, &arg)?;

        if let Some(entry) = self
            .entries
            .iter()
            .find(|entry| entry.func == aggregate.func && entry.arg == arg)
        {
            return Ok((entry.output.clone(), entry.ty));
        }
        let output = alias
            .map(|alias| alias.as_ref().clone())
            .unwrap_or_else(|| aggregate.func.column_name(&arg));
        self.specs
            .push((aggregate.func, arg.clone(), output.clone()));
        self.entries.push(AggregateEntry {
            func: aggregate.func,
            arg,
            output: output.clone(),
            ty,
        });
        Ok((output, ty))
    }
}

/// The result type of an aggregate, after checking its argument (§3.2).
fn result_type(
    aggregate: &AggregateExpr,
    at: Position,
    arg_ty: Option<ColumnType>,
    arg: &str,
) -> Result<ColumnType, SqlError> {
    match aggregate.func {
        Aggregate::Count => Ok(ColumnType::Int),
        Aggregate::Sum | Aggregate::Avg => match arg_ty {
            Some(ColumnType::Int) => Ok(ColumnType::Int),
            Some(other) => Err(SqlError::type_(
                format!(
                    "{} requires an INT column, but '{}' is {}",
                    aggregate.func, arg, other
                ),
                at,
            )),
            None => Err(SqlError::type_(
                format!("{} requires a column argument", aggregate.func),
                at,
            )),
        },
        Aggregate::Min | Aggregate::Max => arg_ty.ok_or_else(|| {
            SqlError::type_(format!("{} requires a column argument", aggregate.func), at)
        }),
    }
}

// =====================================================================
// Small checks and helpers
// =====================================================================

/// Whether a select list mentions an aggregate.
fn items_have_aggregate(items: &SelectItems) -> bool {
    match items {
        SelectItems::Star => false,
        SelectItems::List(items) => items
            .iter()
            .any(|item| matches!(&item.expr, Expr::Aggregate(_))),
    }
}

/// The first aggregate anywhere in a condition, for the `WHERE` rule.
fn first_aggregate(condition: &Condition) -> Option<&Located<AggregateExpr>> {
    match condition {
        Condition::Compare(compare) => {
            for operand in [&compare.left, &compare.right] {
                if let Operand::Aggregate(aggregate) = operand {
                    return Some(aggregate);
                }
            }
            None
        }
        Condition::And(left, right) | Condition::Or(left, right) => {
            first_aggregate(left).or_else(|| first_aggregate(right))
        }
        Condition::Not(inner) => first_aggregate(inner),
    }
}

/// Expand `SELECT *` to every scope column, in FROM then schema order.
fn expand_star(items: &SelectItems, scope: &Scope, at: Position) -> Vec<SelectItem> {
    match items {
        SelectItems::List(items) => items.clone(),
        SelectItems::Star => scope
            .all_columns()
            .into_iter()
            .map(|column| SelectItem {
                expr: Expr::Column(column_ref(&column.name, at)),
                alias: None,
            })
            .collect(),
    }
}

/// The output name a selected column takes: its `AS` alias, or its own name.
fn output_name(column: &Column, alias: Option<&Located<String>>) -> String {
    alias.map_or_else(|| column.name.clone(), |alias| alias.as_ref().clone())
}

/// A fully qualified or alias column reference from a name.
fn column_ref(name: &str, at: Position) -> Located<ColumnRef> {
    let (qualifier, name) = match name.rsplit_once('.') {
        Some((qualifier, name)) => (Some(qualifier.to_string()), name.to_string()),
        None => (None, name.to_string()),
    };
    Located::new(ColumnRef { qualifier, name }, at)
}

/// Set operators need the same number of columns with the same types (§3.2).
fn check_shape(left: &[Column], right: &[Column], at: Position) -> Result<(), SqlError> {
    if left.len() != right.len() {
        return Err(SqlError::schema(
            format!(
                "set operands have {} and {} columns",
                left.len(),
                right.len()
            ),
            at,
        ));
    }
    for (index, (left, right)) in left.iter().zip(right).enumerate() {
        if left.ty != right.ty {
            return Err(SqlError::schema(
                format!(
                    "set operand column {} is {} on the left and {} on the right",
                    index + 1,
                    left.ty,
                    right.ty
                ),
                at,
            ));
        }
    }
    Ok(())
}

/// Whether a type is an `INT` or a string, the only distinction a comparison
/// cares about (§3.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// An integer.
    Int,
    /// A string, whatever its declared length.
    Str,
}

impl Kind {
    /// The `Kind` a column type belongs to.
    fn of(ty: ColumnType) -> Kind {
        match ty {
            ColumnType::Int => Kind::Int,
            ColumnType::Varchar(_) => Kind::Str,
        }
    }

    /// The type name a type error names.
    fn name(self) -> &'static str {
        match self {
            Kind::Int => "INT",
            Kind::Str => "VARCHAR",
        }
    }
}

/// The position a block's set-operator error is reported at.
trait BlockAt {
    /// The position of the block's `SELECT` or its first block.
    fn at(&self) -> Position;
}

impl BlockAt for SelectBlock {
    fn at(&self) -> Position {
        match self {
            SelectBlock::Select(select) => select.at(),
            SelectBlock::Sub(query) => query.at(),
        }
    }
}

impl BlockAt for Query {
    fn at(&self) -> Position {
        self.first.at()
    }
}
