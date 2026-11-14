//! `group_by`: group by some columns and aggregate (§2.6).
//!
//! Blocking: it must see every input tuple before it can decide its groups, so
//! it has two implementations and the plan picks between them.
//!
//! Sort mode drains the input through [`ExternalSort`] and consumes it one key
//! group at a time, assembling each group's aggregates as it sees the key
//! change. A group's aggregates are held in a few counters, never the group's
//! tuples, so a million-row group costs the same memory as a one-row group.
//!
//! Hash mode asks the same [`ExternalSort`] for partitions of the input, spilled
//! to disk by the grouping key, then processes one at a time. A partition that
//! fits the budget is collapsed in an in-memory hash table; one that is still
//! too large is sorted (again by [`ExternalSort`]) and streamed by the sort path,
//! which is bounded for any partition size.
//!
//! The memory rule: a blocking operator is allowed the whole budget. Sort mode
//! spends it inside the sort (B frames for pass 0, B for a merge pass). Hash
//! mode holds `B − 1` frames for its partition buffers and its one live
//! in-memory partition; the overflow fallback releases them, sorts with the
//! full B, and takes them back.

use std::collections::HashMap;
use std::collections::hash_map::Entry;

use crate::value::{Row, Value};

use crate::exec::external_sort::{ExternalSort, Partition};
use crate::exec::order::SortOrder;
use crate::exec::plan::{Aggregate, Mode};
use crate::exec::{Context, Counters, ExecError, Operator};
use crate::schema::{Column, ColumnType, Schema};

/// `group_by`: group by some columns and aggregate (§2.6).
pub struct GroupBy {
    /// The input stream.
    child: Box<dyn Operator>,
    /// Whether to group by sorting or by hashing.
    mode: Mode,
    /// The output shape: the grouping columns then the aggregate columns.
    schema: Schema,
    /// The grouping columns, in output order.
    key_indices: Vec<usize>,
    /// The aggregates, with the input column each reads.
    aggregators: Vec<Aggregator>,
    /// The grouping-key sort keys, the partition runs and the sorted stream —
    /// everything both modes of the collapse share.
    sorted: ExternalSort,
    /// Hash mode: one partition that fit in memory, whose groups are ready.
    emitting: Option<std::vec::IntoIter<Row>>,
    /// Whether any group has been produced, so the empty-input single group is
    /// emitted at most once.
    emitted_any: bool,
    /// §1.4 counters for this operator.
    counters: Counters,
}

impl GroupBy {
    /// A `group_by` of `child` on the named `keys` with `aggregates`.
    ///
    /// The output is the grouping columns (in the child's names) then one
    /// column per aggregate (`count_star`, or `tag_column`).
    ///
    /// Errors: [`ExecError::UnknownColumn`] for a key or aggregate column the
    /// child has none for, [`ExecError::Type`] for `SUM`/`AVG` over a
    /// non-`INT` column, or [`ExecError::Arity`] for colliding output names.
    pub fn new(
        child: Box<dyn Operator>,
        mode: Mode,
        keys: &[String],
        aggregates: &[(Aggregate, String)],
    ) -> Result<Self, ExecError> {
        let child_schema = child.schema();
        let mut columns = Vec::with_capacity(keys.len() + aggregates.len());

        let mut key_indices = Vec::with_capacity(keys.len());
        for name in keys {
            let index = child_schema.resolve(name)?;
            key_indices.push(index);
            columns.push(child_schema.columns()[index].clone());
        }

        let mut aggregators = Vec::with_capacity(aggregates.len());
        for (func, column) in aggregates {
            let (index, ty) = aggregate_column(child_schema, column, *func)?;
            aggregators.push(Aggregator {
                func: *func,
                column: index,
            });
            columns.push(Column {
                name: func.column_name(column),
                ty,
            });
        }

        let schema = Schema::new(columns)?;
        let sorted = ExternalSort::by(child_schema, SortOrder::ascending(key_indices.clone()));
        Ok(GroupBy {
            child,
            mode,
            schema,
            key_indices,
            aggregators,
            sorted,
            emitting: None,
            emitted_any: false,
            counters: Counters::default(),
        })
    }

    /// The grouping values of `row`, in output order.
    fn key_values(&self, row: &Row) -> Result<Vec<Value>, ExecError> {
        let mut values = Vec::with_capacity(self.key_indices.len());
        for &index in &self.key_indices {
            let value = row.get(index).ok_or_else(|| ExecError::Type {
                detail: format!("tuple is missing column {}", index + 1),
            })?;
            values.push(value.clone());
        }
        Ok(values)
    }

    /// Collapse one in-memory partition into its groups.
    fn groups_in_memory(&mut self, rows: Vec<Row>) -> Result<Vec<Row>, ExecError> {
        let mut groups: HashMap<Vec<Value>, (Vec<Value>, Vec<AggregateState>)> = HashMap::new();
        for row in rows {
            let key = row.key(&self.key_indices)?;
            self.counters.comparisons += 1;
            let (_, states) = match groups.entry(key) {
                Entry::Occupied(entry) => entry.into_mut(),
                Entry::Vacant(entry) => {
                    let key_values = self.key_values(&row)?;
                    entry.insert((
                        key_values,
                        vec![AggregateState::default(); self.aggregators.len()],
                    ))
                }
            };
            for (state, aggregator) in states.iter_mut().zip(&self.aggregators) {
                let value = aggregator.column.map(|index| &row[index]);
                state.update(aggregator.func, value)?;
            }
        }
        let mut out = Vec::with_capacity(groups.len());
        for (_, (key_values, states)) in groups {
            out.push(self.build_group_row(&key_values, &states)?);
        }
        Ok(out)
    }

    /// Assemble one output tuple: the key values then the aggregate values.
    fn build_group_row(
        &self,
        key_values: &[Value],
        states: &[AggregateState],
    ) -> Result<Row, ExecError> {
        let mut values = key_values.to_vec();
        for (aggregator, state) in self.aggregators.iter().zip(states) {
            values.push(state.value(aggregator.func)?);
        }
        Ok(Row::from(values))
    }
}

impl Operator for GroupBy {
    fn schema(&self) -> &Schema {
        &self.schema
    }

    fn open(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        match self.mode {
            Mode::Sort => self.sorted.sort_child(&mut *self.child, ctx),
            Mode::Hash => self.sorted.partition_child(&mut *self.child, ctx),
        }
    }

    fn next(&mut self, ctx: &mut Context) -> Result<Option<Row>, ExecError> {
        loop {
            // Sort mode (or a hash-mode overflow): fold the next key group
            // out of the sorted stream. Running dry settles it, so the next
            // iteration moves on to whatever is left.
            let mut states = vec![AggregateState::default(); self.aggregators.len()];
            let first = self.sorted.next_group(|row| {
                for (state, aggregator) in states.iter_mut().zip(&self.aggregators) {
                    let value = aggregator.column.map(|index| &row[index]);
                    state.update(aggregator.func, value)?;
                }
                Ok(())
            })?;
            if let Some(first) = first {
                self.emitted_any = true;
                self.counters.tuples_out += 1;
                let key_values = self.key_values(&first)?;
                return Ok(Some(self.build_group_row(&key_values, &states)?));
            }

            // Hash mode: drain the partition held in memory, then load the
            // next one.
            if let Some(row) = self.emitting.as_mut().and_then(|rows| rows.next()) {
                self.emitted_any = true;
                self.counters.tuples_out += 1;
                return Ok(Some(row));
            }
            match self.sorted.next_partition(ctx)? {
                Some(Partition::Rows(rows)) => {
                    let groups = self.groups_in_memory(rows)?;
                    self.emitting = Some(groups.into_iter());
                }
                // It did not fit: the sorted run it became is folded by
                // `next_group` at the top of the loop.
                Some(Partition::Sorted) => {}
                // Everything is exhausted — sort mode ends here too, since it
                // has no partitions to load. With no grouping columns an
                // empty input is still one group (§2.6).
                None => {
                    if self.key_indices.is_empty() && !self.emitted_any {
                        self.emitted_any = true;
                        self.counters.tuples_out += 1;
                        let states = vec![AggregateState::default(); self.aggregators.len()];
                        return Ok(Some(self.build_group_row(&[], &states)?));
                    }
                    return Ok(None);
                }
            }
        }
    }

    fn close(&mut self, ctx: &mut Context) -> Result<(), ExecError> {
        self.sorted.close(ctx);
        self.counters.update(self.sorted.take_counters());
        self.emitting = None;
        self.child.close(ctx)
    }

    fn counters(&self) -> Counters {
        self.counters
    }

    fn children(&self) -> Vec<&dyn Operator> {
        vec![&*self.child]
    }
}

impl std::fmt::Display for GroupBy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("group_by")
    }
}

/// One aggregate to compute: the function and the input column it reads.
struct Aggregator {
    /// The function.
    func: Aggregate,
    /// The input column index, or `None` for `COUNT(*)`.
    column: Option<usize>,
}

/// Resolve one aggregate's input column and result type.
fn aggregate_column(
    schema: &Schema,
    column: &str,
    func: Aggregate,
) -> Result<(Option<usize>, ColumnType), ExecError> {
    if column == "*" && func == Aggregate::Count {
        return Ok((None, ColumnType::Int));
    }
    let index = schema.resolve(column)?;
    let ty = schema.columns()[index].ty;
    let result = match func {
        Aggregate::Count => ColumnType::Int,
        Aggregate::Sum | Aggregate::Avg => {
            if ty != ColumnType::Int {
                return Err(ExecError::Type {
                    detail: format!("{func} requires an INT column, but '{column}' is {ty}"),
                });
            }
            ColumnType::Int
        }
        Aggregate::Min | Aggregate::Max => ty,
    };
    Ok((Some(index), result))
}

/// The running state of one aggregate over one group.
#[derive(Debug, Clone, Default)]
struct AggregateState {
    /// How many input tuples this group has (the denominator of `COUNT`/`AVG`).
    count: i32,
    /// The running `SUM`, for `SUM`/`AVG`.
    sum: i32,
    /// The smallest or largest value seen, for `MIN`/`MAX`.
    extremum: Option<Value>,
}

impl AggregateState {
    /// Fold one tuple into the group. `value` is the aggregate's input column,
    /// or `None` for `COUNT(*)`.
    fn update(&mut self, aggregate: Aggregate, value: Option<&Value>) -> Result<(), ExecError> {
        match aggregate {
            Aggregate::Count => self.count += 1,
            Aggregate::Sum | Aggregate::Avg => match value {
                Some(Value::Int(v)) => {
                    self.sum += *v;
                    self.count += 1;
                }
                _ => {
                    return Err(ExecError::Type {
                        detail: format!("{aggregate} requires an INT column"),
                    });
                }
            },
            Aggregate::Min => {
                if let Some(value) = value
                    && self.extremum.as_ref().is_none_or(|min| value < min)
                {
                    self.extremum = Some(value.clone());
                }
            }
            Aggregate::Max => {
                if let Some(value) = value
                    && self.extremum.as_ref().is_none_or(|max| value > max)
                {
                    self.extremum = Some(value.clone());
                }
            }
        }
        Ok(())
    }

    /// The aggregate's value; an error if it is undefined (an empty group and
    /// an aggregate other than `COUNT`).
    fn value(&self, aggregate: Aggregate) -> Result<Value, ExecError> {
        match aggregate {
            Aggregate::Count => Ok(Value::Int(self.count)),
            Aggregate::Sum => {
                if self.count == 0 {
                    return Err(undefined(aggregate));
                }
                Ok(Value::Int(self.sum))
            }
            Aggregate::Avg => {
                if self.count == 0 {
                    return Err(undefined(aggregate));
                }
                let avg = self.sum as f64 / self.count as f64;
                let v = (avg * 10_000.0).round() as i32;
                Ok(Value::Int(v))
            }
            Aggregate::Min | Aggregate::Max => {
                self.extremum.clone().ok_or_else(|| undefined(aggregate))
            }
        }
    }
}

/// The error for `SUM`/`AVG`/`MIN`/`MAX` over an empty group.
fn undefined(aggregate: Aggregate) -> ExecError {
    ExecError::Type {
        detail: format!("{aggregate} is undefined over an empty group"),
    }
}
