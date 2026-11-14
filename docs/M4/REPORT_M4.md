# REPORT_M4.md — Query Optimizer

The measurements are produced by `bench/measure.py` (mode `m4`) against the
release binaries:

```
cargo build --release
python3 bench/measure.py m4
```

The harness creates and `ANALYZE`s four tables (`Emp` 12,000 rows, `Dept` 50,
`Proj` 3,000, `Loc` 500) with `age` and `salary` strongly correlated, then runs
twenty queries at `B = 32`. Estimates come from `radb-opt explain`; actual tuple
counts and I/O come from `radb-exec trace`. `q-error` is
`max(estimate, actual) / min(estimate, actual)`, using `max(1, value)` when a
count is zero. Scan nodes that are reopened by a block nested loop report
cumulative tuples in the trace; the correlated table below uses the single-pass
table cardinality for those scans, since a rescan is not a cardinality change.

## 1. Estimation quality

| metric | value |
|---|---:|
| median q-error across all nodes | 1.00 |
| worst q-error across all nodes | 1200.00 |
| worst non-LIMIT q-error | 50.00 |

The median is exact because the scan and filter estimates use the catalog’s
exact `ndv`, `min`, `max` and histogram. The 1200 outlier is query 14,
`SELECT name FROM Emp ORDER BY salary DESC LIMIT 10`: the sort and projection
below the limit are estimated over all 12,000 rows, while they actually produce
10 because the limit stops the pull. Cardinality estimation below a `LIMIT` is
intentionally not propagated downward; the limit node itself is exact. The other
large values are the `HAVING COUNT(*) > 50` queries (50×), where the histogram
has no information about the distribution of an aggregate output.

| # | result rows | optimized I/O | baseline I/O | ratio | worst q-error |
|---:|---:|---:|---:|---:|---:|
| 1 | 17,848 | 121 | 121 | 1.00 | 1.03 |
| 2 | 22,336 | 121 | 121 | 1.00 | 2.00 |
| 3 | 12,001 | 599 | 599 | 1.00 | 1.00 |
| 4 | 12,060 | 423 | 423 | 1.00 | 10.00 |
| 5 | 29,582 | 122 | 122 | 1.00 | 1.03 |
| 6 | 31,330 | 122 | 122 | 1.00 | 1.00 |
| 7 | 467,772 | 252 | 131 | 1.92 | 2.00 |
| 8 | 1,482,100 | 375 | 132 | 2.84 | 3.00 |
| 9 | 14,449,600 | 1,412 | 135 | 10.46 | 3.00 |
| 10 | 83,392 | 580 | 1,276 | 0.45 | 4.00 |
| 11 | 33,192 | 122 | 122 | 1.00 | 1.03 |
| 12 | 12,100 | 421 | 421 | 1.00 | 50.00 |
| 13 | 26,676 | 179 | 179 | 1.00 | 1.03 |
| 14 | 24,030 | 169 | 169 | 1.00 | 1200.00 |
| 15 | 12,150 | 421 | 421 | 1.00 | 50.00 |
| 16 | 26,776 | 158 | 158 | 1.00 | 1.03 |
| 17 | 34,089 | 122 | 122 | 1.00 | 1.03 |
| 18 | 23,796 | 183 | 275 | 0.67 | 1.03 |
| 19 | 31,883 | 122 | 122 | 1.00 | 2.60 |
| 20 | 13,141 | 121 | 121 | 1.00 | 3.17 |

## 2. Does optimization pay?

The baseline is `radb-opt plan --no-optimize`: no rewrites, joins in written
order, every join a block nested loop. The optimized plan uses the standard rule
set, DP join order and algorithm choice.

| metric | value |
|---|---:|
| optimized I/O over all queries | 6,145 |
| baseline I/O over all queries | 5,292 |
| overall optimized/baseline | 1.16 |
| best query ratio | 0.45 (query 10) |
| worst query ratio | 10.46 (query 9) |

The optimizer wins clearly on the self-join (query 10, 0.45) and the grouped
aggregate (query 18, 0.67). Most other queries fit entirely in the 32-frame pool,
so the baseline’s block nested loop is already reading each page once; there is
no I/O left to remove and the ratio is 1.0. On this small, cached workload the
optimizer is 16% worse overall because its chosen nested-loop order for the
four-way queries (7–9) reopens the inner more often than the written order. That
is the cost model ignoring buffer retention across repeated scans, and it is a
useful warning against reading ratio > 1 as a general result.

## 3. Which rewrite rule mattered

Each rule was disabled in turn with
`radb-opt plan --without-rule N`, and the twenty queries were rerun. The values
are total I/O over the workload; the all-rules total is 6,145.

| disabled rule | total I/O | difference |
|---|---:|---:|
| 1. split conjunctions | 6,145 | 0 |
| 2. push selects | 6,841 | +696 |
| 3. cross to join | 6,145 | 0 |
| 4. push through rename/union | 6,145 | 0 |
| 5. push below group_by | 6,145 | 0 |
| 6. projection pushdown | 6,333 | +188 |
| 7. cleanup | 6,145 | 0 |

Rule 2 is the only one with a large effect on this data: without it the filters
stay above the first join and more tuples reach the join. Projection pushdown
saves 188 pages by dropping columns early. Rules 1, 3 and 4 matter for plan shape
but not for this I/O count because the DP finds the same join order either way.
Rule 5 has no measurable effect because these queries do not put a
grouping-column predicate above a `group_by`. Rule 7 is cosmetic.

## 4. Where it breaks: correlated columns

The dataset makes `age` and `salary` strongly positively correlated. The
following query filters on both:

```sql
SELECT e.name, p.name, l.addr
  FROM Emp e JOIN Proj p ON e.did = p.did JOIN Loc l ON p.did = l.did
 WHERE e.age > 40 AND e.salary > 6500;
```

| node | estimate | actual | q-error |
|---|---:|---:|---:|
| project | 60,600 | 57,600 | 1.05 |
| nested loop (p.did = l.did) | 60,600 | 57,600 | 1.05 |
| nested loop (e.did = p.did) | 6,060 | 5,760 | 1.05 |
| seqscan Loc | 500 | 500 | 1.00 |
| seqscan Proj | 3,000 | 3,000 | 1.00 |
| filter `e.age > 40` | 101 | 192 | 1.90 |
| filter `e.salary > 6500` | 201 | 192 | 1.05 |
| seqscan Emp | 12,000 | 12,000 | 1.00 |

The failed assumption is independence inside the conjunction:
`sel(age > 40 AND salary > 6500) = sel(age > 40) × sel(salary > 6500)`. The two
columns rise together, so the true combined selectivity is higher than the
product; the first filter alone is off by 1.9×. The resulting estimate of
filtered `Emp` is 101 rows instead of 192.

That error changes the join order: the optimizer chooses `Proj × Loc` first and
then joins `Emp` last, while the written-order block nested loop keeps the small
inputs together. The optimized plan costs 267 page I/O against 134 for the
baseline, 1.99× worse for this query. A multi-column or correlated histogram
(joint distribution of `age` and `salary`) would be the statistic that prevents
this: single-column equi-depth histograms cannot represent the overlap.

## 5. DP table for a four-relation query

Query: `Emp e JOIN Dept d ON e.did = d.did JOIN Proj p ON d.did = p.did
JOIN Loc l ON p.did = l.did WHERE e.age > 40`, `B = 32`.

```
subset	plan	cost
{d}	scan Dept as d	1
{d,e}	nested_loop_join (e.did = d.did) [scan Dept as d, filter (e.age > 40) -> scan Emp as e]	54
{d,e,l,p}	nested_loop_join (e.did = d.did) [nested_loop_join (p.did = l.did) [nested_loop_join (d.did = p.did) [scan Proj as p, scan Dept as d], scan Loc as l], filter (e.age > 40) -> scan Emp as e]	935
{d,e,p}	nested_loop_join (e.did = d.did) [nested_loop_join (d.did = p.did) [scan Proj as p, scan Dept as d], filter (e.age > 40) -> scan Emp as e]	87
{d,l,p}	nested_loop_join (p.did = l.did) [nested_loop_join (d.did = p.did) [scan Proj as p, scan Dept as d], scan Loc as l]	37
{d,p}	nested_loop_join (d.did = p.did) [scan Proj as p, scan Dept as d]	11
{e}	filter (e.age > 40) -> scan Emp as e	121
{l}	scan Loc as l	3
{l,p}	nested_loop_join (p.did = l.did) [scan Loc as l, scan Proj as p]	13
{p}	scan Proj as p	10
{e,p}	no predicate, skipped	-
{e,l}	no predicate, skipped	-
{d,l}	no predicate, skipped	-
{d,e,l}	no predicate, skipped	-
{e,l,p}	no predicate, skipped	-
```
