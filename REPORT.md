# Performance study: `R join[R.b=S.b] S`

**Machine:** Linux 6.18.51-1-lts x86_64, 16 CPUs
**Language:** Rust 1.97.1 (8bab26f4f 2026-07-14)
**Binary:** `cargo run --release --bin radb-study -- study`

All timing is wall-clock time of the `Engine::execute` call only (data
generation and relation loading excluded). Comparisons are exact counts
from the instrumented nested-loop join (`Stats::join_comparisons`),
not estimates. Match rate is 5 (each R tuple matches ~5 S tuples on
average).

One implementation note: join conditions are resolved once per query into a
condition tree, and the tree is then evaluated per pair through *borrowed*
operands — a column comparison reads both values by reference, and a string
constant lives in the tree and is borrowed, never cloned. So the condition
evaluator allocates and clones nothing, for any condition shape (the
study's lone `R.b=S.b`, `and`/`or`/`not`, constants, same-side columns).
Operators are compiled at resolve time too: the six grammar comparisons
reduce to a three-way `Ordering` "want" plus (for `>=`, `<=`, `!=`) a
not-flag, so the inner loop never sees the operator. This is a
constant-factor optimization only: it is still a nested loop that examines
every (left, right) pair and counts each one exactly once, so the
comparison counts are exactly n × m. Measured at ~10 ns per pair at 64k —
down from ~27 ns for the original clone-based interpreter — and the 64k
join itself runs in ~43 s against ~109 s before. Run-to-run spread is
wide: this box was under memory pressure (≈2 GB of swap in use) during
measurement, and the 64k join landed between ~37 s and ~55 s depending on
the moment; best-of-N is reported.

## 8.3  Join table

Generated with `radb-study study --sizes 1000,2000,4000,8000,16000,32000,64000 --matches 5 --seed 7`.

| n | m | comparisons | wall time (s) | output tuples |
|---|---:|---:|---:|---:|
| 1000 | 1000 | 1,000,000 | 0.009 | 5,113 |
| 2000 | 2000 | 4,000,000 | 0.028 | 10,069 |
| 4000 | 4000 | 16,000,000 | 0.124 | 20,103 |
| 8000 | 8000 | 64,000,000 | 0.542 | 39,837 |
| 16000 | 16000 | 256,000,000 | 2.067 | 79,771 |
| 32000 | 32000 | 1,024,000,000 | 8.257 | 160,006 |
| 64000 | 64000 | 4,096,000,000 | 42.960 | 319,469 |

## 8.4  Questions

### 1. What is the exact relationship between n, m and the comparison count?

The join comparison count is **exactly n × m** for every row of the table.
The measured count matches the formula precisely with no discrepancy:

- n = 1000, m = 1000 → 1,000,000 = 1000² ✓
- n = 64000, m = 64000 → 4,096,000,000 = 64000² ✓

This is a direct consequence of the nested-loop join algorithm, which
iterates over every (left_row, right_row) pair and increments the counter
once per pair regardless of whether the join condition holds or whether
the condition short-circuits.

### 2. What is the log-log slope and what does it tell us?

Plotting wall time against n on log-log axes and fitting a line by least
squares yields a **slope of 2.04**.  A slope of 2.0 indicates O(n²)
growth: doubling n quadruples the time.  The measured slope of 2.04 is
very close to the theoretical value of 2.0, confirming that the
nested-loop join scales as the product of the two relation sizes.  The
small excess over 2.0 is measurement noise (the largest sizes run with
their working set spreading beyond cache, slightly raising the constant
factor per comparison as n grows).

![Two-panel log–log plot of the measurements. Left: join wall time against n, with the least-squares fit line (measured slope ≈ 2.04). Right: join vs select vs project at the same sizes — the join climbs with slope ≈ 2.0 while select and project stay ≈ 1.0–1.1.](report_loglog.svg)

### 3. Select and project at the same sizes

| n | select time (s) | select examinations | project time (s) |
|---|---:|---:|---:|
| 1000 | 0.0001 | 1000 | 0.0001 |
| 2000 | 0.0002 | 2000 | 0.0003 |
| 4000 | 0.0003 | 4000 | 0.0003 |
| 8000 | 0.0008 | 8000 | 0.0007 |
| 16000 | 0.0027 | 16000 | 0.0018 |
| 32000 | 0.0039 | 32000 | 0.0031 |
| 64000 | 0.0106 | 64000 | 0.0107 |

Select and project both examine each tuple exactly once (the counter
confirms `selection_examinations = n`), so their curves are **linear** in n,
not quadratic.  On log-log axes their slope is ≈ 1.0, compared to the
join's ≈ 2.0.  The select is essentially free: it walks the n tuples,
evaluates a single comparison per tuple, and copies the matching rows
(right panel of the figure above: select and project both rise with
slope ≈ 1, against the join's ≈ 2). The project adds the cost of
hashing each projected row for deduplication,
which is also linear but with a larger constant factor (hash allocation,
HashSet insert).  Both are thousands of times faster than the join at
large sizes because they never nest an inner loop.

### 4. Predicted time for n = 1,000,000

Using the least-squares fit over the seven data points:

```
  log10(time) = -8.24 + 2.04 × log10(n)
  log10(time) = -8.24 + 2.04 × 6.0  = 4.00
  time = 10^4.00 ≈ 10,100 seconds ≈ 2.8 hours
```

**Arithmetic:**
64,000 tuples took 42.96 s.  A million is (1,000,000 / 64,000) = 15.625×
larger.  Under O(n²): 15.625² = 244.14× more time.
42.96 × 244.14 ≈ 10,500 s ≈ 2.9 hours.  The two estimates land within a
few percent of each other, as expected once the per-comparison cost is
flat across sizes: **≈ 2.8–2.9 hours** for the million-tuple join.

### 5. Does changing the match rate change comparisons or wall time?

Measured at n = m = 8,000:

| match rate | comparisons | wall time (s) | output tuples |
|---|---:|---:|---:|
| 1 | 64,000,000 | 0.477 | 8,025 |
| 5 | 64,000,000 | 0.493 | 39,837 |
| 50 | 64,000,000 | 0.716 | 400,400 |

**Comparison count is identical** at all three rates (64,000,000 = 8000²).
The nested-loop join compares every pair regardless of how many match, so
the counter is completely independent of the data distribution.

**Wall time grows with the number of matches**: 0.477 s at rate 1 vs
0.716 s at rate 50 — a 50% increase.  The reason is that each matching
pair produces an output row that must be heap-allocated, filled, and
pushed into a Vec.  At rate 1 there are ~8,000 output rows; at rate 50
there are ~400,000 — 50× more materialization work.  Rate 1 and rate 5
(8k vs 40k output rows) are too close to separate in a single run; the
growth is unambiguous by rate 50, whose extra ~360,000 output rows
dominate the difference.  The comparison loop itself stays cheap (a
borrow-based compare, ~10 ns per pair) for every rate.

### 6. What would make the million-tuple join feasible?

The current nested-loop join compares n × m = 10¹² pairs and takes ≈ 2.9
hours (§8.4 q4).  To make a million-tuple join feasible
(say, under 10 seconds), the comparison count must be reduced from O(n²)
to sub-quadratic.  The two main approaches are a **sort-merge join** and
a **hash join**.  A hash join builds a hash table on the smaller
relation's join attribute (O(m) time and space), then probes it for each
tuple of the larger relation (O(n) time), for a total of O(n + m) expected
time.  With m = 10⁶ the hash table requires ~16 GB of RAM (a few bytes
per bucket), which is within reach of modern servers.  A sort-merge join
sorts both relations (O(n log n + m log m)) then merges in a single pass,
trading memory for deterministic behaviour.  Both require O(m) working
memory for the hash table or sort buffer, which is the fundamental cost
that the nested-loop design avoids.
