//! Join enumeration (Module 4, Section 5).
//!
//! Dynamic programming over left-deep trees, in the style of Selinger. The table
//! is built from a *join block*: a subtree of `scan`/`select`/`join`/`cross`
//! whose leaves are base scans (with their pushed filters). Every leaf is a base
//! relation, every predicate between two bases is a join predicate, and every
//! single-base predicate is a filter attached to its access path. The DP then
//! chooses a join order and algorithm for the full set, up to six relations.
//!
//! The caller only asks the table for a block it knows is well formed (no
//! residual predicate above a join); everything else is converted directly by
//! the optimizer.

use std::collections::{BTreeMap, HashMap};

use crate::exec::plan::Plan as PhysicalPlan;
use crate::optimize::cost::CostModel;
use crate::optimize::error::OptError;
use crate::optimize::selectivity::Selectivity;
use crate::sql::ast::{Condition, Operand};
use crate::sql::plan::Plan as LogicalPlan;
use crate::storage::Catalog;
use crate::storage::stats::ColumnStats;

/// A plan with its estimated cost and metadata for the DP table.
#[derive(Debug, Clone)]
pub struct PlanCost {
    /// The physical plan.
    pub plan: PhysicalPlan,
    /// Estimated page-I/O cost, reads plus writes.
    pub cost: f64,
    /// Estimated output rows.
    pub rows: u64,
    /// Estimated output pages.
    pub pages: u64,
    /// Estimated output width in bytes.
    pub width: u32,
    /// The output column names, fully qualified.
    pub cols: Vec<String>,
    /// Whether this is a single base access path.
    ///
    /// A join's formula already includes reading its outer input, so joining a
    /// base outer must not add that base's scan cost a second time. An
    /// intermediate outer is produced by its subtree and then read again by the
    /// join, so its plan cost *is* added (M4 §5.1's worked table).
    pub is_base: bool,
}

/// The DP table for join enumeration.
pub struct DpTable {
    /// Best plan per subset of base-relation keys, keyed by the sorted subset.
    entries: BTreeMap<Vec<String>, PlanCost>,
    /// Every base-relation key, in extraction order.
    base_keys: Vec<String>,
    /// Subsets the enumeration left out, for `dp-table`.
    skipped: Vec<Vec<String>>,
}

impl DpTable {
    /// Build a DP table from a join block.
    pub fn from_logical_plan(
        logical: &LogicalPlan,
        catalog: &Catalog,
        frames: usize,
    ) -> Result<Self, OptError> {
        Builder::new(frames).build(logical, catalog)
    }

    /// The best plan for the full set of relations, if the table built one.
    pub fn best_plan(&self) -> Option<&PlanCost> {
        if self.base_keys.is_empty() {
            return None;
        }
        let mut full = self.base_keys.clone();
        full.sort();
        self.entries.get(&full)
    }

    /// Every subset and its best plan, for the `dp-table` command.
    pub fn all_entries(&self) -> Vec<(Vec<String>, &PlanCost)> {
        self.entries.iter().map(|(k, v)| (k.clone(), v)).collect()
    }

    /// The best plan for a specific subset, if it was built.
    pub fn best_for_subset(&self, subset: &[String]) -> Option<&PlanCost> {
        let mut sorted = subset.to_vec();
        sorted.sort();
        self.entries.get(&sorted)
    }

    /// The subsets skipped for having no predicate when an alternative existed.
    pub fn skipped(&self) -> &[Vec<String>] {
        &self.skipped
    }
}

/// One base relation: a scan with the filters pushed onto it.
struct BaseRel {
    /// The key used in predicates (the table name, after alias resolution).
    key: String,
    /// The output columns, fully qualified.
    cols: Vec<String>,
    /// The predicates pushed onto this scan.
    filters: Vec<Condition>,
    /// A projection over the filtered scan, if projection pushdown made one.
    projection: Option<Vec<String>>,
    /// Column statistics, keyed by fully qualified name.
    col_stats: HashMap<String, ColumnStats>,
    /// Column widths, keyed by fully qualified name.
    col_widths: HashMap<String, u32>,
    /// The physical access path (scan with its filters and projection).
    access: PhysicalPlan,
    /// Estimated rows after the filters.
    rows: u64,
    /// Estimated output pages after the filters.
    pages: u64,
    /// The table's measured data pages, which is what its scan costs.
    raw_pages: u64,
    /// Estimated width.
    width: u32,
    /// The raw tuple count, before filters.
    raw_tuples: u64,
}

/// One join predicate between two base relations.
struct JoinPred {
    /// One side's key.
    a: String,
    /// The other side's key.
    b: String,
    /// The fully qualified column on `a`'s side.
    a_col: String,
    /// The fully qualified column on `b`'s side.
    b_col: String,
    /// The predicate.
    condition: Condition,
    /// Whether the predicate is an equality (the only kind hash/sort-merge can
    /// serve, §2.8/§2.9).
    equality: bool,
    /// Its selectivity.
    selectivity: f64,
}

struct Builder {
    cost: CostModel,
    bases: Vec<BaseRel>,
    index: HashMap<String, usize>,
    preds: Vec<JoinPred>,
    skipped: Vec<Vec<String>>,
}

impl Builder {
    fn new(frames: usize) -> Self {
        Builder {
            cost: CostModel::new(frames),
            bases: Vec::new(),
            index: HashMap::new(),
            preds: Vec::new(),
            skipped: Vec::new(),
        }
    }

    fn build(mut self, logical: &LogicalPlan, catalog: &Catalog) -> Result<DpTable, OptError> {
        self.extract(logical, catalog)?;
        if self.bases.is_empty() {
            return Ok(DpTable {
                entries: BTreeMap::new(),
                base_keys: Vec::new(),
                skipped: Vec::new(),
            });
        }
        self.finish_bases();
        self.finish_preds();
        let base_keys: Vec<String> = self.bases.iter().map(|b| b.key.clone()).collect();
        let mut full = base_keys.clone();
        full.sort();
        // Enumerate on predicate-bearing joins first; only if that leaves the
        // full set unbuildable (a pure cross-product query) allow the
        // predicate-free pairings back in (§5: "unless no alternative exists").
        let mut entries = self.enumerate(false);
        if base_keys.len() > 1 && !entries.contains_key(&full) {
            self.skipped.clear();
            entries = self.enumerate(true);
        }
        Ok(DpTable {
            entries,
            base_keys,
            skipped: self.skipped,
        })
    }

    /// Extract the base relations and predicates of a join block.
    fn extract(&mut self, plan: &LogicalPlan, catalog: &Catalog) -> Result<Vec<String>, OptError> {
        match plan {
            LogicalPlan::Scan { table } => {
                if !self.index.contains_key(table) {
                    self.add_base(table, None, catalog)?;
                }
                Ok(vec![table.clone()])
            }
            LogicalPlan::Rename { to, child } => match child.as_ref() {
                LogicalPlan::Scan { table } => {
                    if !self.index.contains_key(to) {
                        self.add_base(table, Some(to), catalog)?;
                    }
                    Ok(vec![to.clone()])
                }
                _ => self.extract(child, catalog),
            },
            LogicalPlan::Select { condition, child } => {
                let keys = self.extract(child, catalog)?;
                // A select may sit above a projection or an intermediate join
                // after predicate pushdown. Classify each conjunct by the base
                // relations it references, so a single-side predicate still
                // reaches its scan and a two-side equality becomes a join
                // predicate.
                self.classify(condition, &keys);
                Ok(keys)
            }
            LogicalPlan::Join {
                condition,
                left,
                right,
            } => {
                let mut keys = self.extract(left, catalog)?;
                keys.extend(self.extract(right, catalog)?);
                self.classify(condition, &keys);
                Ok(keys)
            }
            LogicalPlan::Cross { left, right } => {
                let mut keys = self.extract(left, catalog)?;
                keys.extend(self.extract(right, catalog)?);
                Ok(keys)
            }
            // Projection pushdown inserts projections inside a join block.
            // A one-relation projection is folded into that base's access
            // path; a projection above an intermediate join is cosmetic for
            // execution, and the DP chooses the joins underneath it.
            LogicalPlan::Project { columns, child } => {
                let keys = self.extract(child, catalog)?;
                if keys.len() == 1
                    && let Some(&index) = self.index.get(&keys[0])
                {
                    self.bases[index].projection =
                        Some(columns.iter().map(|(input, _)| input.clone()).collect());
                }
                Ok(keys)
            }
            // A plan handed straight to the DP table (the `dp-table` command)
            // may have operators above the join block; walk through them.
            LogicalPlan::GroupBy { child, .. }
            | LogicalPlan::Distinct { child }
            | LogicalPlan::Sort { child, .. }
            | LogicalPlan::Limit { child, .. } => self.extract(child, catalog),
            _ => Err(OptError::Plan {
                detail: "not a join block".to_string(),
            }),
        }
    }

    fn add_base(
        &mut self,
        table: &str,
        alias: Option<&str>,
        catalog: &Catalog,
    ) -> Result<(), OptError> {
        let meta = catalog.require(table)?;
        let key = alias.unwrap_or(table).to_string();
        let (col_stats, raw_tuples, pages, width) = match &meta.stats {
            Some(stats) => {
                let mut col_stats = HashMap::new();
                for (i, col) in meta.schema.columns().iter().enumerate() {
                    if let Some(cs) = stats.columns.get(i) {
                        col_stats.insert(format!("{key}.{}", col.name), cs.clone());
                    }
                }
                let t = &stats.table;
                (col_stats, t.n_tuples, t.n_pages, t.width)
            }
            None => (HashMap::new(), 10000, 100, 100),
        };
        let cols: Vec<String> = meta
            .schema
            .columns()
            .iter()
            .map(|c| format!("{key}.{}", c.name))
            .collect();
        let mut col_widths = HashMap::new();
        for column in meta.schema.columns() {
            let qualified = format!("{key}.{}", column.name);
            let width = match column.ty {
                crate::ColumnType::Int => 4,
                crate::ColumnType::Varchar(n) => u32::from(n) + 2,
            };
            col_widths.insert(qualified, width);
        }
        let idx = self.bases.len();
        self.bases.push(BaseRel {
            key: key.clone(),
            cols,
            filters: Vec::new(),
            projection: None,
            col_stats,
            col_widths,
            access: PhysicalPlan::SeqScan {
                table: table.to_string(),
                alias: alias.map(str::to_string),
            },
            rows: raw_tuples,
            pages,
            raw_pages: pages,
            width,
            raw_tuples,
        });
        self.index.insert(key, idx);
        Ok(())
    }

    /// Build each base's access path and post-filter estimate.
    fn finish_bases(&mut self) {
        for base in &mut self.bases {
            let raw = base.raw_tuples;
            let mut plan = base.access.clone();
            let mut rows = raw;
            for filter in &base.filters {
                let sel = Selectivity::new(&base.col_stats, raw).estimate(filter);
                rows = ((rows as f64) * sel).round() as u64;
                plan = PhysicalPlan::Filter {
                    condition: filter.clone(),
                    child: Box::new(plan),
                };
            }
            base.rows = rows.max(1);
            if let Some(projection) = &base.projection {
                base.width = projection
                    .iter()
                    .map(|name| base.col_widths.get(name).copied().unwrap_or(4))
                    .sum();
                base.cols = projection.clone();
                plan = PhysicalPlan::Project {
                    columns: projection.clone(),
                    child: Box::new(plan),
                };
            }
            // A scan always reads the table's measured pages; only the output
            // of a filtered access path is narrowed by its selectivity.
            base.pages = if base.filters.is_empty() && base.projection.is_none() {
                base.raw_pages
            } else {
                self.cost.pages(base.rows, base.width)
            };
            base.access = plan;
        }
    }

    /// Classify a join condition's conjuncts into join predicates and filters.
    fn classify(&mut self, condition: &Condition, keys: &[String]) {
        for conjunct in conjuncts(condition) {
            let cols: Vec<String> = columns_of(&conjunct);
            let mut referenced: Vec<String> = cols
                .iter()
                .filter_map(|c| c.split_once('.').map(|(t, _)| t.to_string()))
                .filter(|t| keys.iter().any(|k| k == t))
                .collect();
            referenced.sort();
            referenced.dedup();
            let equality = conjunct.is_equality();
            match referenced.len() {
                1 => {
                    if let Some(&idx) = self.index.get(&referenced[0]) {
                        self.bases[idx].filters.push(conjunct);
                    }
                }
                2 => {
                    let a_col = cols
                        .iter()
                        .find(|c| c.starts_with(&format!("{}.", referenced[0])))
                        .cloned()
                        .unwrap_or_default();
                    let b_col = cols
                        .iter()
                        .find(|c| c.starts_with(&format!("{}.", referenced[1])))
                        .cloned()
                        .unwrap_or_default();
                    self.preds.push(JoinPred {
                        a: referenced[0].clone(),
                        b: referenced[1].clone(),
                        a_col,
                        b_col,
                        condition: conjunct,
                        equality,
                        selectivity: 0.0,
                    });
                }
                _ => {}
            }
        }
    }

    /// Compute predicate selectivities now that every base is known.
    fn finish_preds(&mut self) {
        let ndv = |bases: &[BaseRel], index: &HashMap<String, usize>, key: &str, col: &str| {
            index
                .get(key)
                .and_then(|&i| bases[i].col_stats.get(col))
                .map(|s| s.ndv)
                .unwrap_or(1)
                .max(1)
        };
        for pred in &mut self.preds {
            pred.selectivity = if pred.equality {
                let left = ndv(&self.bases, &self.index, &pred.a, &pred.a_col);
                let right = ndv(&self.bases, &self.index, &pred.b, &pred.b_col);
                1.0 / left.max(right) as f64
            } else {
                Selectivity::DEFAULT
            };
        }
    }

    /// The dynamic program over subsets, up to six relations.
    ///
    /// A pairing with no join predicate is a cross product; it is left out while
    /// `allow_cross` is false so the table matches §5's "no predicate, skipped".
    fn enumerate(&mut self, allow_cross: bool) -> BTreeMap<Vec<String>, PlanCost> {
        let mut entries: BTreeMap<Vec<String>, PlanCost> = BTreeMap::new();

        for base in &self.bases {
            entries.insert(
                vec![base.key.clone()],
                PlanCost {
                    plan: base.access.clone(),
                    cost: self.cost.seqscan(base.raw_pages) as f64,
                    rows: base.rows,
                    pages: base.pages,
                    width: base.width,
                    cols: base.cols.clone(),
                    is_base: true,
                },
            );
        }

        let keys: Vec<String> = self.bases.iter().map(|b| b.key.clone()).collect();
        let n = keys.len().min(6);
        for size in 2..=n {
            for subset in combinations(&keys, size) {
                let mut key = subset.clone();
                key.sort();
                let mut best: Option<PlanCost> = None;
                let mut skipped_here = false;
                for (position, right_key) in subset.iter().enumerate() {
                    let mut rest = subset.clone();
                    rest.remove(position);
                    rest.sort();
                    let Some(left) = entries.get(&rest) else {
                        continue;
                    };
                    let right = &self.bases[self.index[right_key]];
                    let preds: Vec<&JoinPred> = self
                        .preds
                        .iter()
                        .filter(|p| {
                            (p.a == right.key && rest.contains(&p.b))
                                || (p.b == right.key && rest.contains(&p.a))
                        })
                        .collect();
                    if preds.is_empty() && !allow_cross {
                        skipped_here = true;
                        continue;
                    }
                    let joined = self.join_cost(left, right, &preds);
                    if best.as_ref().is_none_or(|b| joined.cost < b.cost) {
                        best = Some(joined);
                    }
                }
                match best {
                    Some(best) => {
                        entries.insert(key, best);
                    }
                    None => {
                        if skipped_here {
                            let mut sorted = subset.clone();
                            sorted.sort();
                            self.skipped.push(sorted);
                        }
                    }
                }
            }
        }
        entries
    }

    /// The cheapest join of `left` and `right` on `preds`.
    fn join_cost(&self, left: &PlanCost, right: &BaseRel, preds: &[&JoinPred]) -> PlanCost {
        let condition = if preds.is_empty() {
            Condition::always_true()
        } else {
            preds
                .iter()
                .map(|p| p.condition.clone())
                .reduce(|a, b| Condition::And(Box::new(a), Box::new(b)))
                .expect("at least one predicate")
        };
        let selectivity: f64 = preds.iter().map(|p| p.selectivity).product();
        let factor = if preds.is_empty() { 1.0 } else { selectivity };
        let rows = (((left.rows as f64) * (right.rows as f64) * factor).round() as u64).max(1);
        let width = left.width + right.width;
        let pages = self.cost.pages(rows, width);

        let all_equality = !preds.is_empty() && preds.iter().all(|p| p.equality);
        let mut algorithms: Vec<Algo> = vec![Algo::NestedLoop];
        if all_equality {
            algorithms.push(Algo::SortMerge);
            algorithms.push(Algo::Hash);
        }

        let mut best: Option<PlanCost> = None;
        for algo in algorithms {
            let algo_cost = match algo {
                Algo::NestedLoop => self.cost.nested_loop_join(left.pages, right.pages),
                Algo::SortMerge => self.cost.sort_merge_join(left.pages, right.pages),
                Algo::Hash => self.cost.hash_join(left.pages, right.pages),
            } as f64;
            let plan = match algo {
                Algo::NestedLoop => PhysicalPlan::NestedLoopJoin {
                    condition: condition.clone(),
                    left: Box::new(left.plan.clone()),
                    right: Box::new(right.access.clone()),
                },
                Algo::SortMerge => PhysicalPlan::SortMergeJoin {
                    condition: condition.clone(),
                    left: Box::new(left.plan.clone()),
                    right: Box::new(right.access.clone()),
                },
                Algo::Hash => PhysicalPlan::HashJoin {
                    condition: condition.clone(),
                    left: Box::new(left.plan.clone()),
                    right: Box::new(right.access.clone()),
                },
            };
            // A base outer's pages are read by the algorithm's own M term; an
            // intermediate outer is produced by `left` and then re-read.
            let outer_cost = if left.is_base { 0.0 } else { left.cost };
            let cost = outer_cost + algo_cost;
            let candidate = PlanCost {
                plan,
                cost,
                rows,
                pages,
                width,
                cols: {
                    let mut cols = left.cols.clone();
                    cols.extend(right.cols.clone());
                    cols
                },
                is_base: false,
            };
            if best.as_ref().is_none_or(|b| candidate.cost < b.cost) {
                best = Some(candidate);
            }
        }
        best.expect("at least one algorithm")
    }
}

#[derive(Clone, Copy)]
enum Algo {
    NestedLoop,
    SortMerge,
    Hash,
}

/// Split a condition's top-level `AND`s.
fn conjuncts(condition: &Condition) -> Vec<Condition> {
    match condition {
        Condition::And(a, b) => {
            let mut parts = conjuncts(a);
            parts.extend(conjuncts(b));
            parts
        }
        _ => vec![condition.clone()],
    }
}

/// The fully qualified column names a condition references.
fn columns_of(condition: &Condition) -> Vec<String> {
    fn operand(operand: &Operand, out: &mut Vec<String>) {
        if let Operand::Column(col) = operand {
            out.push(match &col.qualifier {
                Some(q) => format!("{q}.{}", col.name),
                None => col.name.clone(),
            });
        }
    }
    let mut out = Vec::new();
    match condition {
        Condition::Compare(compare) => {
            operand(&compare.left, &mut out);
            operand(&compare.right, &mut out);
        }
        Condition::And(a, b) | Condition::Or(a, b) => {
            out.extend(columns_of(a));
            out.extend(columns_of(b));
        }
        Condition::Not(inner) => out.extend(columns_of(inner)),
    }
    out
}

/// Generate all combinations of size `k`.
fn combinations<T: Clone + Ord>(items: &[T], k: usize) -> Vec<Vec<T>> {
    let mut result = Vec::new();
    if k == 0 {
        result.push(Vec::new());
        return result;
    }
    if k > items.len() {
        return result;
    }
    combine_rec(items, k, 0, &mut Vec::new(), &mut result);
    result
}

fn combine_rec<T: Clone + Ord>(
    items: &[T],
    k: usize,
    start: usize,
    current: &mut Vec<T>,
    result: &mut Vec<Vec<T>>,
) {
    if current.len() == k {
        result.push(current.clone());
        return;
    }
    for i in start..items.len() {
        current.push(items[i].clone());
        combine_rec(items, k, i + 1, current, result);
        current.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Schema;
    use crate::storage::stats::{ColumnStats, Stats, TableStats};
    use crate::storage::{BufferPool, Catalog, Policy, TableMeta};

    /// The §5.1 worked example reproduces: the DP table costs are 110 for
    /// `{A,B'}`, 410 for `{B',C'}` and 530 for the whole set, with 550 as the
    /// alternative order.
    #[test]
    fn worked_example_costs_reproduce() {
        let dir = std::env::temp_dir().join(format!("radb_dp_{}.db", std::process::id()));
        let _ = std::fs::remove_file(&dir);
        let mut pool = BufferPool::new(32, Policy::Lru);
        let mut catalog = Catalog::open(&mut pool, &dir).unwrap();
        for (name, rows, pages, width, ndv) in [
            ("A", 5_000u64, 100u64, 49u32, 2_500u64),
            ("B", 500, 10, 32, 500),
            ("C", 20_000, 400, 49, 5_000),
        ] {
            let schema: Schema = "x:INT,y:INT".parse().unwrap();
            let mut meta = TableMeta::new(name, schema);
            meta.num_pages = pages as u32;
            meta.num_tuples = rows;
            meta.stats = Some(Stats {
                table: TableStats {
                    n_tuples: rows,
                    n_pages: pages,
                    width,
                },
                columns: vec![
                    ColumnStats {
                        ndv,
                        ..ColumnStats::default()
                    },
                    ColumnStats {
                        ndv,
                        ..ColumnStats::default()
                    },
                ],
            });
            catalog.insert(meta).unwrap();
        }
        catalog.save(&mut pool).unwrap();
        pool.flush_all().unwrap();

        let scan = |table: &str| LogicalPlan::Scan {
            table: table.to_string(),
        };
        let ab = LogicalPlan::Join {
            condition: "A.x = B.x".parse().unwrap(),
            left: Box::new(scan("A")),
            right: Box::new(scan("B")),
        };
        let plan = LogicalPlan::Join {
            condition: "B.y = C.y".parse().unwrap(),
            left: Box::new(ab),
            right: Box::new(scan("C")),
        };
        let table = DpTable::from_logical_plan(&plan, &catalog, 32).unwrap();

        let key = |names: &[&str]| {
            let mut subset: Vec<String> = names.iter().map(|name| name.to_string()).collect();
            subset.sort();
            table.entries.get(&subset).map(|entry| entry.cost)
        };
        assert_eq!(key(&["A", "B"]), Some(110.0));
        assert_eq!(key(&["B", "C"]), Some(410.0));
        assert_eq!(key(&["A", "B", "C"]), Some(530.0));
        let best = table.best_plan().unwrap();
        assert_eq!(best.cost, 530.0);
        assert_eq!(best.rows, 4_000);

        let _ = std::fs::remove_file(&dir);
    }
}
