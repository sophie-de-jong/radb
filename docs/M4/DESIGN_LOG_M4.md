# Design Log M4

This log records the optimizer work, including the places where AI assistance
produced something that looked reasonable but was wrong. Each entry says how the
problem was found and what replaced it.

## Session 1 — statistics, rewrites, enumeration

- Added the catalog `Stats` blob, exact `ANALYZE` with per-column external sorts,
  equi-depth histograms, the §2 selectivity formulas, the rewrite rules, the
  §4 cost formulas and Selinger-style DP over left-deep trees.
- The first rule set included a `ResolveAliases` rule that rewrote every logical
  alias to a physical name before conversion. It made simple plans look right,
  but it made the logical plan internally inconsistent: a projected output name
  stayed `n` while its input had already become `count_star`, and later rules
  such as projection pushdown then used the stale name.

## Session 2 — aliases and executable plans

**AI mistake 1: resolving aliases by globally rewriting the logical tree.**
AI suggested walking the logical plan once, replacing every alias reference with
the name a scan or aggregate would produce, and deleting the renames. It passed
the plan-shape tests. Running a `FROM (SELECT did, COUNT(*) AS n ...) AS t JOIN
Dept d` query through `radb-opt plan` and `radb-exec run` produced
`unknown column 'count_star'`: the outer projection had been rewritten while the
subquery's projection output had not. The fix is to keep the logical plan in
logical names and translate at the boundary to physical names, using
`Optimizer::name_map` (logical output name to physical output name) for every
dependent predicate, grouping key, aggregate argument, projection input and sort
key. Base aliases still become M2's optional `seqscan.alias`.

**AI mistake 2: a hardcoded `explain` in the binary.**
The first `explain` implementation in `src/bin/radb_opt.rs` printed fixed
fallback estimates (`seqscan -> 10000 rows, 200 pages`, a join `-> 2000, 330`)
whenever it could not match a DP subset. It looked plausible and was never
tested against the statistics. A scan of a four-row analyzed table still printed
`est_rows=10000`. Explain now lives in `src/optimize/explain.rs`, estimates every
node recursively from catalog statistics and the same §2/§4 formulas, and the
binary only prints its string.

**AI mistake 3: DP cost double-counting the outer base relation.**
The first `join_cost` added `left.cost` unconditionally to the join formula. For
the §5.1 worked example that gives `{A,B'} = 10 + (10 + 100) = 120`, not the
specified `110`. The cause is that a join formula already includes reading its
outer input. The fix distinguishes a base outer (its pages are the formula's `M`
term) from an intermediate outer (its production cost is added, and its pages
are read again), with `PlanCost::is_base`. The table now reproduces `110`, `410`,
`530` and the `550` alternative.

## Session 3 — rules and cleanup

- Rule 4 now has a real implementation: a select is pushed below a `rename`
  (rewriting the alias through the rename) and into both branches of a `union` or
  `union_all` (translated position by position).
- Rule 7's two halves run at different times. Removing a projection that drops
  nothing happens in projection pushdown; collapsing adjacent selects runs once
  after the rewrite loop. Running the collapse inside the loop would undo rule 1
  every pass. This is noted in `docs/EXCEPTIONS.md`.
- Equality selectivity now uses the bucket formula
  `count(bucket) / (n_tuples × ndv(bucket))` when a histogram holds the value,
  falling back to `1/ndv`. The earlier implementation always used `1/ndv`, which
  made the histogram decorative.
