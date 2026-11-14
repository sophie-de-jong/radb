# Design M4: Query Optimizer

## 1. Statistics

Statistics are stored in the catalog as `TableMeta.stats: Option<Stats>`, part
of the bincode-encoded table metadata (fixed-width little-endian, catalog
version 3). The `ANALYZE` command computes them by scanning the table.

The serialized layout is:

```
Stats {
    table: TableStats { n_tuples: u64, n_pages: u64, width: u32 },
    columns: Vec<ColumnStats {
        ndv: u64, min: Value, max: Value,
        buckets: Vec<Bucket { min: Value, max: Value, count: u64, ndv: u64 }>,
    }>,
}
```

`Value` is the same `INT`/`VARCHAR` enum used everywhere else, so a string
column has lexicographic bounds and an equi-depth histogram just like an integer
column. `columns` is in schema order; `buckets` is in value order. The field
order and enum order are part of catalog version 3.

For each table we store:
- `n_tuples(T)`: total number of live tuples
- `n_pages(T)`: number of data pages
- `width(T)`: average record length in bytes

For each column we store:
- `ndv(T.c)`: number of distinct values
- `min(T.c)`, `max(T.c)`: lowest and highest value, in the column's own type
- `hist(T.c)`: equi-depth histogram with configurable bucket count (default 20). Each bucket stores min, max, count of tuples, and number of distinct values in the bucket.

ANALYZE scans the heap once, copying the live rows to one temporary run. It then
sorts each column of that run with Module 2's external sort, so no column is
ever held in memory and there is only one pass over the table.

## 2. Selectivity Estimation

We use the formulas from Section 2 of the specification:

- `c = v`: If a histogram holds `v`, find its bucket and use `count(bucket) / (n_tuples * ndv(bucket))`. With no bucket: `1/ndv(c)`.
- `c < v, c > v`: Sum whole buckets on the appropriate side plus a linear fraction of the bucket containing `v`. `INT` ranges interpolate numerically; `VARCHAR` ranges use the bucket midpoint because strings have no arithmetic distance (their bounds and histogram are still stored and compared lexicographically).
- `c <> v`: `1 - sel(c = v)`
- `p AND q`: `sel(p) * sel(q)` (independence assumption)
- `p OR q`: `sel(p) + sel(q) - sel(p) * sel(q)`
- `NOT p`: `1 - sel(p)`
- `R.a = S.b`: `n(R) * n(S) / max(ndv(R.a), ndv(S.b))`
- Other join predicates: `0.1` by default

Cardinality of output is `input_card * selectivity` (rounded up, never below 1). Output pages = `ceil(rows * width / 4096)`. We assume independence between predicates on different columns, which can lead to estimation errors when columns are correlated.

## 3. Rewrite Rules

Applied to the logical plan before costing, in order:

1. **Split conjunctions**: `select[p AND q]` becomes two stacked selects. Always safe.
2. **Push select below join/cross**: If predicate references columns from one side only, push it to that side.
3. **Turn cross plus select into join**: If predicate references both sides and is an equality, convert cross+select to join.
4. **Push select below rename/union/union_all**: Rewrite column names through rename. For unions, push into both branches.
5. **Push select below group_by**: Only if predicate references grouping columns alone. Predicates on aggregates cannot move (become HAVING).
6. **Push projections down**: Insert projections below operators keeping only needed columns.
7. **Remove useless projections and collapse adjacent selects**: Cosmetic cleanup.

## 4. Cost Model

Costs are page I/O only (reads + writes), CPU ignored. `M, N` are estimated pages of inputs, `B` is memory budget.

- seqscan: `M`
- filter, project, limit: `0` (pipelined)
- sort: `2 * M * (passes - 1) + M` where `passes = 1 + ceil(log(ceil(M/B)) / log(B-1))`
- distinct/group_by: cost of underlying sort or hash partitioning
- nested_loop_join: `M + ceil(M/(B-2)) * N`
- sort_merge_join: `sort(M) + sort(N) + M + N`
- hash_join: `3*(M+N)`

## 5. Join Enumeration

Dynamic programming over left-deep trees (Selinger-style), for queries with up to 6 relations:
- `best[{r}]` = cheapest access path for single relation r
- For each subset S of size k>=2, try to join each relation r in S with best[S-{r}] using each join algorithm, skipping pairs with no join predicate unless no alternative
- Left-deep trees mean right input of every join is a base relation. This restricts search space but enables pipelining of the outer side.

## Implementation Notes

The optimizer reads statistics from the catalog, applies rewrites, does
cost-based join enumeration, and produces a physical plan in the exact JSON
format consumed by Module 2. A base scan costs `n_pages` from the statistics,
even when a pushed filter reduces its output; the filtered output's page count
is estimated from its rows and width for the next join. The DP table is built
from base relations and their join predicates: a base outer's pages are counted
once by the join algorithm, while an intermediate outer's production cost is
added and its pages are read again by the join, which reproduces M4 §5.1's
worked table (asserted by the `worked_example_costs_reproduce` unit test).

`explain` and `dp-table` rendering live in `src/optimize/explain.rs`, not in the
binary. `explain` prints each node's own page-I/O cost from §4 and its estimated
row count; `dp-table` prints every subset the enumerator considered, plus the
subsets it skipped for having no predicate. Distinct cardinality uses the product
of the output columns' `ndv` (bounded by its input), `UNION` uses the same
estimate over the union of the branch names, and each aggregate output gets a
coarse `[0, groups]` range so a `HAVING` predicate is not estimated from the
default 0.1.

The logical plan can carry names the physical plan cannot: a table alias
(`FROM Emp e`), a subquery alias (`FROM (...) AS t`), a select alias
(`SELECT name AS n`) or an aggregate alias (`COUNT(*) AS n`). Conversion keeps a
logical-to-physical name map (`Optimizer::name_map`) and rewrites every
dependent predicate, grouping key, aggregate argument, projection input and sort
key through it. A base-table alias is carried by M2's optional `seqscan.alias`;
a subquery alias is resolved into its child's names. This is what lets a query
with aliases or a `FROM` subquery run unedited in Module 2 while M2's JSON stays
the shape in its specification.
