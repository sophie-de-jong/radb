//! The optimizer: rewrite rules, join enumeration, and physical conversion.
//!
//! An [`Optimizer`] is created for one logical plan. It owns the rule list, the
//! memory budget, the pass limit, the plan, and the DP tables for the plan's
//! join blocks. Rules mutate the plan in place; [`Optimizer::finish`] converts
//! the result to physical form.

use std::collections::HashMap;

use crate::ColumnType;
use crate::exec::Direction;
use crate::exec::plan::{Aggregate, Mode, Plan as PhysicalPlan, SortKey};
use crate::schema::Column;
use crate::sql::ast::Condition;
use crate::sql::plan::Plan as LogicalPlan;
use crate::storage::Catalog;
use crate::storage::error::StorageError;

use super::dp::DpTable;
use super::error::OptError;
use super::rules::{RewriteRule, collapse_selects, default_rules};

/// The default number of rewrite passes.
pub const DEFAULT_PASSES: usize = 10;

/// An optimizer for one logical plan.
pub struct Optimizer<'a> {
    catalog: &'a Catalog,
    rules: Vec<Box<dyn RewriteRule>>,
    frames: usize,
    passes: usize,
    plan: LogicalPlan,
    dp_tables: Option<Vec<DpTable>>,
    cleanup: bool,
}

impl<'a> Optimizer<'a> {
    /// The standard optimizer for `plan` and a `frames`-frame budget.
    pub fn new(plan: LogicalPlan, catalog: &'a Catalog, frames: usize) -> Self {
        Self::with_rules(plan, catalog, frames, default_rules())
    }

    /// An optimizer with an explicit rule list and pass limit.
    pub fn with_rules(
        plan: LogicalPlan,
        catalog: &'a Catalog,
        frames: usize,
        rules: Vec<Box<dyn RewriteRule>>,
    ) -> Self {
        Optimizer {
            catalog,
            rules,
            frames: frames.max(1),
            passes: DEFAULT_PASSES,
            plan,
            dp_tables: None,
            cleanup: true,
        }
    }

    /// The rules, in application order.
    pub fn rules(&self) -> &[Box<dyn RewriteRule>] {
        &self.rules
    }

    /// The memory budget used for costing and join enumeration.
    pub fn frames(&self) -> usize {
        self.frames
    }

    /// The maximum number of rewrite passes.
    pub fn passes(&self) -> usize {
        self.passes
    }

    /// Change the maximum number of rewrite passes.
    pub fn set_passes(&mut self, passes: usize) -> &mut Self {
        self.passes = passes;
        self
    }

    /// Enable or disable rule 7's final adjacent-select collapse.
    pub fn set_cleanup(&mut self, cleanup: bool) -> &mut Self {
        self.cleanup = cleanup;
        self
    }

    /// The current logical plan.
    pub fn plan(&self) -> &LogicalPlan {
        &self.plan
    }

    /// Convert the plan without rewrites, join reordering, or algorithm
    /// choice: joins stay in the order written and use a block nested loop.
    ///
    /// This is the "no optimizer" baseline the M4 report compares against.
    pub fn finish_unoptimized(&self) -> Result<PhysicalPlan, OptError> {
        Ok(force_nested_loop(self.convert_simple(&self.plan)?))
    }

    /// Apply every rule once, in order. Returns whether any rule changed the
    /// plan.
    pub fn rewrite_once(&mut self) -> Result<bool, OptError> {
        let mut changed = false;
        for rule in &self.rules {
            changed |= rule.rewrite(&mut self.plan, self.catalog)?;
        }
        Ok(changed)
    }

    /// Apply the rules until they stop changing the plan, up to the configured
    /// pass limit.
    pub fn optimize(&mut self) -> Result<(), OptError> {
        for _ in 0..self.passes {
            if !self.rewrite_once()? {
                break;
            }
        }
        // Rule 7's cleanup runs once at the fixed point: collapsing inside the
        // loop would undo rule 1 on every pass.
        if self.cleanup {
            while collapse_selects(&mut self.plan) {}
        }
        self.dp_tables = None;
        Ok(())
    }

    /// The DP tables for the plan's join blocks, in AST order, built lazily.
    pub fn dp_tables(&mut self) -> Result<&[DpTable], OptError> {
        self.ensure_dp_tables()?;
        Ok(self.dp_tables.as_deref().unwrap_or(&[]))
    }

    /// Convert the current logical plan to a physical plan.
    pub fn finish(&mut self) -> Result<PhysicalPlan, OptError> {
        self.ensure_dp_tables()?;
        let mut cursor = 0;
        self.convert(&self.plan, &mut cursor)
    }

    /// Build the DP tables for every join block, in AST order.
    fn ensure_dp_tables(&mut self) -> Result<(), OptError> {
        if self.dp_tables.is_some() {
            return Ok(());
        }
        let mut tables = Vec::new();
        self.collect_dp_tables(&self.plan, &mut tables)?;
        self.dp_tables = Some(tables);
        Ok(())
    }

    /// Collect the DP table for each join block the conversion will ask for.
    fn collect_dp_tables(
        &self,
        plan: &LogicalPlan,
        out: &mut Vec<DpTable>,
    ) -> Result<(), OptError> {
        match plan {
            LogicalPlan::Scan { .. } | LogicalPlan::Join { .. } | LogicalPlan::Cross { .. } => {
                if self.is_join_block(plan) && !self.has_residual_select(plan) {
                    out.push(DpTable::from_logical_plan(plan, self.catalog, self.frames)?);
                }
                Ok(())
            }
            LogicalPlan::Select { child, .. } => {
                if self.has_join(child) {
                    self.collect_dp_tables(child, out)
                } else if self.is_join_block(plan) && !self.has_residual_select(plan) {
                    out.push(DpTable::from_logical_plan(plan, self.catalog, self.frames)?);
                    Ok(())
                } else {
                    Ok(())
                }
            }
            LogicalPlan::Rename { child, .. } => match child.as_ref() {
                LogicalPlan::Scan { .. } => Ok(()),
                _ => self.collect_dp_tables(child, out),
            },
            LogicalPlan::Project { child, .. }
            | LogicalPlan::GroupBy { child, .. }
            | LogicalPlan::Distinct { child }
            | LogicalPlan::Sort { child, .. }
            | LogicalPlan::Limit { child, .. } => self.collect_dp_tables(child, out),
            LogicalPlan::Union { left, right }
            | LogicalPlan::UnionAll { left, right }
            | LogicalPlan::Intersect { left, right }
            | LogicalPlan::Except { left, right } => {
                self.collect_dp_tables(left, out)?;
                self.collect_dp_tables(right, out)
            }
        }
    }

    /// Convert one logical operator, consuming the DP tables in AST order.
    fn convert(&self, plan: &LogicalPlan, cursor: &mut usize) -> Result<PhysicalPlan, OptError> {
        match plan {
            LogicalPlan::Scan { .. } | LogicalPlan::Join { .. } | LogicalPlan::Cross { .. } => {
                self.convert_block(plan, cursor)
            }
            LogicalPlan::Select { condition, child } => {
                if self.has_join(child) {
                    // A predicate above a join is residual: keep it above the
                    // join the block produces, translated from the logical
                    // child's output names to the physical ones.
                    let condition = condition.rename_columns(&self.name_map(child)?);
                    Ok(PhysicalPlan::Filter {
                        condition,
                        child: Box::new(self.convert(child, cursor)?),
                    })
                } else {
                    self.convert_block(plan, cursor)
                }
            }
            LogicalPlan::Rename { to, child } => match child.as_ref() {
                LogicalPlan::Scan { table } => Ok(PhysicalPlan::SeqScan {
                    table: table.clone(),
                    alias: Some(to.clone()),
                }),
                _ => self.convert(child, cursor),
            },
            LogicalPlan::Project { columns, child } => {
                let map = self.name_map(child)?;
                Ok(PhysicalPlan::Project {
                    columns: columns
                        .iter()
                        .map(|(input, _)| map_name(&map, input))
                        .collect(),
                    child: Box::new(self.convert(child, cursor)?),
                })
            }
            LogicalPlan::GroupBy {
                keys,
                aggregates,
                child,
            } => {
                let map = self.name_map(child)?;
                Ok(PhysicalPlan::GroupBy {
                    mode: Mode::Hash,
                    keys: keys.iter().map(|key| map_name(&map, key)).collect(),
                    aggregates: aggregates
                        .iter()
                        .map(|(aggregate, column, _)| (*aggregate, map_name(&map, column)))
                        .collect(),
                    child: Box::new(self.convert(child, cursor)?),
                })
            }
            LogicalPlan::Distinct { child } => Ok(PhysicalPlan::Distinct {
                mode: Mode::Hash,
                child: Box::new(self.convert(child, cursor)?),
            }),
            LogicalPlan::Sort { keys, child } => self.convert_sort(keys, child, cursor),
            LogicalPlan::Limit { n, child } => Ok(PhysicalPlan::Limit {
                n: *n,
                child: Box::new(self.convert(child, cursor)?),
            }),
            LogicalPlan::UnionAll { left, right } => Ok(PhysicalPlan::UnionAll {
                left: Box::new(self.convert(left, cursor)?),
                right: Box::new(self.convert(right, cursor)?),
            }),
            LogicalPlan::Union { left, right } => Ok(PhysicalPlan::Union {
                left: Box::new(self.convert(left, cursor)?),
                right: Box::new(self.convert(right, cursor)?),
            }),
            LogicalPlan::Intersect { left, right } => Ok(PhysicalPlan::Intersect {
                left: Box::new(self.convert(left, cursor)?),
                right: Box::new(self.convert(right, cursor)?),
            }),
            LogicalPlan::Except { left, right } => Ok(PhysicalPlan::Except {
                left: Box::new(self.convert(left, cursor)?),
                right: Box::new(self.convert(right, cursor)?),
            }),
        }
    }

    /// Convert a `sort`. If it sits above a `project` that dropped one of its
    /// keys, the key is projected in below the sort and projected away again
    /// above it, so `ORDER BY` on a column outside the select list still runs
    /// (M3 §7 case 9).
    fn convert_sort(
        &self,
        keys: &[(String, Direction)],
        child: &LogicalPlan,
        cursor: &mut usize,
    ) -> Result<PhysicalPlan, OptError> {
        let map = self.name_map(child)?;
        let physical_keys: Vec<SortKey> = keys
            .iter()
            .map(|(column, direction)| SortKey::new(map_name(&map, column), *direction))
            .collect();

        if let LogicalPlan::Project {
            child: grandchild, ..
        } = child
        {
            // A key outside the select list is not produced by the project.
            // Put it into a project below the sort, sort, then project it away
            // again (M3 §7 case 9).
            let projected: Vec<String> = self
                .physical_columns(child)?
                .into_iter()
                .map(|column| column.name)
                .collect();
            let missing: Vec<String> = physical_keys
                .iter()
                .map(|key| key.column().to_string())
                .filter(|key| !projected.contains(key))
                .collect();
            if !missing.is_empty() {
                let mut inner = projected.clone();
                inner.extend(missing);
                let inner_project = PhysicalPlan::Project {
                    columns: inner,
                    child: Box::new(self.convert(grandchild, cursor)?),
                };
                let sort = PhysicalPlan::Sort {
                    keys: physical_keys,
                    child: Box::new(inner_project),
                };
                return Ok(PhysicalPlan::Project {
                    columns: projected,
                    child: Box::new(sort),
                });
            }
        }

        Ok(PhysicalPlan::Sort {
            keys: physical_keys,
            child: Box::new(self.convert(child, cursor)?),
        })
    }

    /// Convert a join block: use the next collected DP table if it is
    /// available, otherwise convert the block directly.
    fn convert_block(
        &self,
        plan: &LogicalPlan,
        cursor: &mut usize,
    ) -> Result<PhysicalPlan, OptError> {
        if self.is_join_block(plan) && !self.has_residual_select(plan) {
            if let Some(table) = self
                .dp_tables
                .as_ref()
                .and_then(|tables| tables.get(*cursor))
            {
                *cursor += 1;
                if let Some(best) = table.best_plan() {
                    return Ok(best.plan.clone());
                }
            } else {
                // The collection and conversion should stay in step; if they
                // do not, fall back to a local table.
                let table = DpTable::from_logical_plan(plan, self.catalog, self.frames)?;
                if let Some(best) = table.best_plan() {
                    return Ok(best.plan.clone());
                }
            }
        }
        self.convert_simple(plan)
    }

    /// A direct conversion, used for a join block the DP table will not order
    /// (a subquery leaf, a residual predicate, more than six relations) and as
    /// the fallback for a block with no usable predicate.
    fn convert_simple(&self, logical: &LogicalPlan) -> Result<PhysicalPlan, OptError> {
        match logical {
            LogicalPlan::Scan { table } => {
                self.catalog.require(table).map_err(|e| match e {
                    StorageError::UnknownTable { name } => OptError::UnknownTable { name },
                    other => OptError::Plan {
                        detail: other.to_string(),
                    },
                })?;
                Ok(PhysicalPlan::SeqScan {
                    table: table.clone(),
                    alias: None,
                })
            }
            LogicalPlan::Select { condition, child } => {
                let condition = condition.rename_columns(&self.name_map(child)?);
                Ok(PhysicalPlan::Filter {
                    condition,
                    child: Box::new(self.convert_simple(child)?),
                })
            }
            LogicalPlan::Project { columns, child } => {
                let map = self.name_map(child)?;
                Ok(PhysicalPlan::Project {
                    columns: columns
                        .iter()
                        .map(|(input, _)| map_name(&map, input))
                        .collect(),
                    child: Box::new(self.convert_simple(child)?),
                })
            }
            LogicalPlan::Rename { to, child } => match child.as_ref() {
                LogicalPlan::Scan { table } => Ok(PhysicalPlan::SeqScan {
                    table: table.clone(),
                    alias: Some(to.clone()),
                }),
                _ => self.convert_simple(child),
            },
            LogicalPlan::GroupBy {
                keys,
                aggregates,
                child,
            } => {
                let map = self.name_map(child)?;
                Ok(PhysicalPlan::GroupBy {
                    mode: Mode::Hash,
                    keys: keys.iter().map(|key| map_name(&map, key)).collect(),
                    aggregates: aggregates
                        .iter()
                        .map(|(aggregate, column, _)| (*aggregate, map_name(&map, column)))
                        .collect(),
                    child: Box::new(self.convert_simple(child)?),
                })
            }
            LogicalPlan::Distinct { child } => Ok(PhysicalPlan::Distinct {
                mode: Mode::Hash,
                child: Box::new(self.convert_simple(child)?),
            }),
            LogicalPlan::Sort { keys, child } => {
                let map = self.name_map(child)?;
                let physical_keys: Vec<SortKey> = keys
                    .iter()
                    .map(|(column, direction)| SortKey::new(map_name(&map, column), *direction))
                    .collect();
                if let LogicalPlan::Project {
                    child: grandchild, ..
                } = child.as_ref()
                {
                    let projected: Vec<String> = self
                        .physical_columns(child)?
                        .into_iter()
                        .map(|column| column.name)
                        .collect();
                    let missing: Vec<String> = physical_keys
                        .iter()
                        .map(|key| key.column().to_string())
                        .filter(|key| !projected.contains(key))
                        .collect();
                    if !missing.is_empty() {
                        let mut inner = projected.clone();
                        inner.extend(missing);
                        let inner_project = PhysicalPlan::Project {
                            columns: inner,
                            child: Box::new(self.convert_simple(grandchild)?),
                        };
                        let sort = PhysicalPlan::Sort {
                            keys: physical_keys,
                            child: Box::new(inner_project),
                        };
                        return Ok(PhysicalPlan::Project {
                            columns: projected,
                            child: Box::new(sort),
                        });
                    }
                }
                Ok(PhysicalPlan::Sort {
                    keys: physical_keys,
                    child: Box::new(self.convert_simple(child)?),
                })
            }
            LogicalPlan::Limit { n, child } => Ok(PhysicalPlan::Limit {
                n: *n,
                child: Box::new(self.convert_simple(child)?),
            }),
            LogicalPlan::UnionAll { left, right } => Ok(PhysicalPlan::UnionAll {
                left: Box::new(self.convert_simple(left)?),
                right: Box::new(self.convert_simple(right)?),
            }),
            LogicalPlan::Union { left, right } => Ok(PhysicalPlan::Union {
                left: Box::new(self.convert_simple(left)?),
                right: Box::new(self.convert_simple(right)?),
            }),
            LogicalPlan::Intersect { left, right } => Ok(PhysicalPlan::Intersect {
                left: Box::new(self.convert_simple(left)?),
                right: Box::new(self.convert_simple(right)?),
            }),
            LogicalPlan::Except { left, right } => Ok(PhysicalPlan::Except {
                left: Box::new(self.convert_simple(left)?),
                right: Box::new(self.convert_simple(right)?),
            }),
            LogicalPlan::Join {
                condition,
                left,
                right,
            } => {
                let mut map = self.name_map(left)?;
                map.extend(self.name_map(right)?);
                let condition = condition.rename_columns(&map);
                let left = Box::new(self.convert_simple(left)?);
                let right = Box::new(self.convert_simple(right)?);
                if condition.is_equality() {
                    Ok(PhysicalPlan::HashJoin {
                        condition,
                        left,
                        right,
                    })
                } else {
                    Ok(PhysicalPlan::NestedLoopJoin {
                        condition,
                        left,
                        right,
                    })
                }
            }
            LogicalPlan::Cross { left, right } => {
                let left = Box::new(self.convert_simple(left)?);
                let right = Box::new(self.convert_simple(right)?);
                Ok(PhysicalPlan::NestedLoopJoin {
                    condition: Condition::always_true(),
                    left,
                    right,
                })
            }
        }
    }

    /// Map every logical output name of `plan` to the name the physical plan
    /// that [`Optimizer::convert`] builds for it actually produces.
    ///
    /// M2's physical `project` and `group_by` have no output-alias field, so an
    /// SQL alias (`SELECT name AS n`, `COUNT(*) AS n`) exists only in the
    /// logical plan. This map is how a predicate, grouping key, aggregate
    /// argument or sort key written against a logical name is translated to the
    /// physical column the executor will see.
    fn name_map(&self, plan: &LogicalPlan) -> Result<HashMap<String, String>, OptError> {
        let logical = self.logical_columns(plan)?;
        let physical = self.physical_columns(plan)?;
        if logical.len() != physical.len() {
            return Err(OptError::Plan {
                detail: format!(
                    "logical plan produces {} columns but physical plan produces {}",
                    logical.len(),
                    physical.len()
                ),
            });
        }
        Ok(logical
            .into_iter()
            .zip(physical)
            .map(|(logical, physical)| (logical.name, physical.name))
            .collect())
    }

    /// The output columns of a logical plan, before physical conversion.
    fn logical_columns(&self, plan: &LogicalPlan) -> Result<Vec<Column>, OptError> {
        match plan {
            LogicalPlan::Scan { table } => {
                let mut schema = self.require(table)?.schema.clone();
                schema.qualify(table);
                Ok(schema.columns().to_vec())
            }
            LogicalPlan::Rename { to, child } => Ok(requalify(to, &self.logical_columns(child)?)),
            LogicalPlan::Select { child, .. }
            | LogicalPlan::Distinct { child }
            | LogicalPlan::Sort { child, .. }
            | LogicalPlan::Limit { child, .. } => self.logical_columns(child),
            LogicalPlan::Project { columns, child } => {
                let input = self.logical_columns(child)?;
                columns
                    .iter()
                    .map(|(source, output)| {
                        let column = input
                            .iter()
                            .find(|column| &column.name == source)
                            .ok_or_else(|| OptError::Plan {
                                detail: format!("project input '{source}' is not in the child"),
                            })?;
                        Ok(Column {
                            name: output.clone(),
                            ty: column.ty,
                        })
                    })
                    .collect()
            }
            LogicalPlan::GroupBy {
                keys,
                aggregates,
                child,
            } => {
                let input = self.logical_columns(child)?;
                let mut columns = Vec::with_capacity(keys.len() + aggregates.len());
                for key in keys {
                    columns.push(
                        input
                            .iter()
                            .find(|column| &column.name == key)
                            .cloned()
                            .ok_or_else(|| OptError::Plan {
                                detail: format!("grouping key '{key}' is not in the child"),
                            })?,
                    );
                }
                for (aggregate, column, output) in aggregates {
                    let ty = match aggregate {
                        Aggregate::Count | Aggregate::Sum | Aggregate::Avg => ColumnType::Int,
                        Aggregate::Min | Aggregate::Max => input
                            .iter()
                            .find(|input| &input.name == column)
                            .map_or(ColumnType::Int, |input| input.ty),
                    };
                    columns.push(Column {
                        name: output.clone(),
                        ty,
                    });
                }
                Ok(columns)
            }
            LogicalPlan::Join { left, right, .. } | LogicalPlan::Cross { left, right } => {
                let mut columns = self.logical_columns(left)?;
                columns.extend(self.logical_columns(right)?);
                Ok(columns)
            }
            LogicalPlan::UnionAll { left, .. }
            | LogicalPlan::Union { left, .. }
            | LogicalPlan::Intersect { left, .. }
            | LogicalPlan::Except { left, .. } => self.logical_columns(left),
        }
    }

    /// The output columns the physical plan for `plan` actually produces.
    fn physical_columns(&self, plan: &LogicalPlan) -> Result<Vec<Column>, OptError> {
        match plan {
            LogicalPlan::Scan { table } => {
                let mut schema = self.require(table)?.schema.clone();
                schema.qualify(table);
                Ok(schema.columns().to_vec())
            }
            LogicalPlan::Rename { to, child } => match child.as_ref() {
                LogicalPlan::Scan { table } => {
                    let mut schema = self.require(table)?.schema.clone();
                    schema.qualify(to);
                    Ok(schema.columns().to_vec())
                }
                // A non-scan rename has no physical operator; the mapping
                // handles its logical names.
                _ => self.physical_columns(child),
            },
            LogicalPlan::Select { child, .. }
            | LogicalPlan::Distinct { child }
            | LogicalPlan::Sort { child, .. }
            | LogicalPlan::Limit { child, .. } => self.physical_columns(child),
            LogicalPlan::Project { columns, child } => {
                let map = self.name_map(child)?;
                let input = self.logical_columns(child)?;
                columns
                    .iter()
                    .map(|(source, _)| {
                        let column = input
                            .iter()
                            .find(|column| &column.name == source)
                            .ok_or_else(|| OptError::Plan {
                                detail: format!("project input '{source}' is not in the child"),
                            })?;
                        Ok(Column {
                            name: map_name(&map, source),
                            ty: column.ty,
                        })
                    })
                    .collect()
            }
            LogicalPlan::GroupBy {
                keys,
                aggregates,
                child,
            } => {
                let map = self.name_map(child)?;
                let input = self.logical_columns(child)?;
                let mut columns = Vec::with_capacity(keys.len() + aggregates.len());
                for key in keys {
                    columns.push(Column {
                        name: map_name(&map, key),
                        ty: input
                            .iter()
                            .find(|column| &column.name == key)
                            .map_or(ColumnType::Int, |column| column.ty),
                    });
                }
                for (aggregate, column, _) in aggregates {
                    let source = map_name(&map, column);
                    let ty = match aggregate {
                        Aggregate::Count | Aggregate::Sum | Aggregate::Avg => ColumnType::Int,
                        Aggregate::Min | Aggregate::Max => input
                            .iter()
                            .find(|input| map_name(&map, &input.name) == source)
                            .map_or(ColumnType::Int, |input| input.ty),
                    };
                    columns.push(Column {
                        name: aggregate.column_name(&source),
                        ty,
                    });
                }
                Ok(columns)
            }
            LogicalPlan::Join { left, right, .. } | LogicalPlan::Cross { left, right } => {
                let mut columns = self.physical_columns(left)?;
                columns.extend(self.physical_columns(right)?);
                Ok(columns)
            }
            LogicalPlan::UnionAll { left, .. }
            | LogicalPlan::Union { left, .. }
            | LogicalPlan::Intersect { left, .. }
            | LogicalPlan::Except { left, .. } => self.physical_columns(left),
        }
    }

    /// Look a table up, translating a storage error into an optimizer error.
    fn require(&self, table: &str) -> Result<&crate::storage::TableMeta, OptError> {
        self.catalog.require(table).map_err(|error| match error {
            StorageError::UnknownTable { name } => OptError::UnknownTable { name },
            other => OptError::Plan {
                detail: other.to_string(),
            },
        })
    }

    /// Whether `plan` is built only from the join-block operators.
    fn is_join_block(&self, plan: &LogicalPlan) -> bool {
        match plan {
            LogicalPlan::Scan { .. } => true,
            LogicalPlan::Select { child, .. } => self.is_join_block(child),
            LogicalPlan::Project { child, .. } => self.is_join_block(child),
            // A rename is part of a DP block only over a base scan, where the
            // physical seqscan carries the alias. Any other rename is resolved
            // by the conversion name map instead.
            LogicalPlan::Rename { child, .. } => matches!(child.as_ref(), LogicalPlan::Scan { .. }),
            LogicalPlan::Join { left, right, .. } | LogicalPlan::Cross { left, right } => {
                self.is_join_block(left) && self.is_join_block(right)
            }
            _ => false,
        }
    }

    /// Whether a `select` sits above a join and so is not part of the join
    /// block.
    fn has_residual_select(&self, plan: &LogicalPlan) -> bool {
        match plan {
            LogicalPlan::Select { child, .. } => {
                self.has_join(child) || self.has_residual_select(child)
            }
            LogicalPlan::Join { left, right, .. } | LogicalPlan::Cross { left, right } => {
                self.has_residual_select(left) || self.has_residual_select(right)
            }
            _ => false,
        }
    }

    /// Whether `plan` contains a join or cross anywhere below it.
    fn has_join(&self, plan: &LogicalPlan) -> bool {
        match plan {
            LogicalPlan::Join { .. } | LogicalPlan::Cross { .. } => true,
            LogicalPlan::Select { child, .. }
            | LogicalPlan::Project { child, .. }
            | LogicalPlan::Rename { child, .. }
            | LogicalPlan::GroupBy { child, .. }
            | LogicalPlan::Distinct { child }
            | LogicalPlan::Sort { child, .. }
            | LogicalPlan::Limit { child, .. } => self.has_join(child),
            LogicalPlan::Union { left, right }
            | LogicalPlan::UnionAll { left, right }
            | LogicalPlan::Intersect { left, right }
            | LogicalPlan::Except { left, right } => self.has_join(left) || self.has_join(right),
            LogicalPlan::Scan { .. } => false,
        }
    }
}

/// The physical name of a logical column, or the name unchanged when it is
/// already physical.
fn map_name(map: &HashMap<String, String>, name: &str) -> String {
    map.get(name).cloned().unwrap_or_else(|| name.to_string())
}

/// Rewrite every join in `plan` to a block nested loop join.
fn force_nested_loop(plan: PhysicalPlan) -> PhysicalPlan {
    match plan {
        PhysicalPlan::HashJoin {
            condition,
            left,
            right,
        }
        | PhysicalPlan::SortMergeJoin {
            condition,
            left,
            right,
        } => PhysicalPlan::NestedLoopJoin {
            condition,
            left: Box::new(force_nested_loop(*left)),
            right: Box::new(force_nested_loop(*right)),
        },
        PhysicalPlan::NestedLoopJoin {
            condition,
            left,
            right,
        } => PhysicalPlan::NestedLoopJoin {
            condition,
            left: Box::new(force_nested_loop(*left)),
            right: Box::new(force_nested_loop(*right)),
        },
        PhysicalPlan::Filter { condition, child } => PhysicalPlan::Filter {
            condition,
            child: Box::new(force_nested_loop(*child)),
        },
        PhysicalPlan::Project { columns, child } => PhysicalPlan::Project {
            columns,
            child: Box::new(force_nested_loop(*child)),
        },
        PhysicalPlan::Distinct { mode, child } => PhysicalPlan::Distinct {
            mode,
            child: Box::new(force_nested_loop(*child)),
        },
        PhysicalPlan::Sort { keys, child } => PhysicalPlan::Sort {
            keys,
            child: Box::new(force_nested_loop(*child)),
        },
        PhysicalPlan::GroupBy {
            mode,
            keys,
            aggregates,
            child,
        } => PhysicalPlan::GroupBy {
            mode,
            keys,
            aggregates,
            child: Box::new(force_nested_loop(*child)),
        },
        PhysicalPlan::Limit { n, child } => PhysicalPlan::Limit {
            n,
            child: Box::new(force_nested_loop(*child)),
        },
        PhysicalPlan::UnionAll { left, right } => PhysicalPlan::UnionAll {
            left: Box::new(force_nested_loop(*left)),
            right: Box::new(force_nested_loop(*right)),
        },
        PhysicalPlan::Union { left, right } => PhysicalPlan::Union {
            left: Box::new(force_nested_loop(*left)),
            right: Box::new(force_nested_loop(*right)),
        },
        PhysicalPlan::Intersect { left, right } => PhysicalPlan::Intersect {
            left: Box::new(force_nested_loop(*left)),
            right: Box::new(force_nested_loop(*right)),
        },
        PhysicalPlan::Except { left, right } => PhysicalPlan::Except {
            left: Box::new(force_nested_loop(*left)),
            right: Box::new(force_nested_loop(*right)),
        },
        leaf @ PhysicalPlan::SeqScan { .. } => leaf,
    }
}

/// Re-qualify `columns` under `key`, keeping their bare names and types.
fn requalify(key: &str, columns: &[Column]) -> Vec<Column> {
    columns
        .iter()
        .map(|column| {
            let bare = column
                .name
                .rsplit_once('.')
                .map_or(column.name.as_str(), |(_, bare)| bare);
            Column {
                name: format!("{key}.{bare}"),
                ty: column.ty,
            }
        })
        .collect()
}
