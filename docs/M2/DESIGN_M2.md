# Design M2: Execution Engine

## Overview

This implementation provides a pull-based iterator model for executing relational algebra plans. The engine is built on top of Module 1's storage layer (buffer pool, heap files, catalog). All operators implement a common `Operator` trait with `open()`, `next()`, `close()`, and `counters()` methods. The engine enforces a global memory budget `B` (frames) and ensures no pages remain pinned and no temp files remain after `close()`.

## Iterator Model

Each operator is a pull iterator. `next()` produces the next output tuple or `None` on end. Operators call `next()` on their children as needed. On `open()`, operators acquire resources and may do blocking work. On `close()`, operators unpin all pages, delete all temporary files they created, and release resources. Counters track tuples_out, comparisons, page_reads, and page_writes attributed to each operator.

## Memory Budget

The whole plan receives `B` frames via `--frames`. The budget is managed by `Budget` and enforced through `Context`. At any moment, the number of frames held by operators must not exceed `B`. We use a simple division rule: each pipelined operator is allocated 1 frame (or uses minimal resources), and each blocking operator is allocated up to its natural requirement bounded by the budget; joins and blocking operators explicitly request frames from the budget. The budget tracks peak usage across the entire plan execution.

## External Sort

`external_sort` implements a multi-pass merge sort. In pass 0, it fills up to `B` frames with input tuples, sorts them in memory, and writes a run to a temporary file. This repeats until the input is exhausted, producing `R0 = ceil(N/B)` runs. Subsequent passes merge `B-1` runs at a time into new runs, writing intermediate runs; each merge uses 1 output frame and reads from the input runs. The final merge pass streams tuples directly to the caller via `MergeStream` without writing a final run - this is the streaming final merge optimization (matching the note in the instructions). Runs are managed as `RunReader`s over temporary page files; `MergeStream` reads the last set of runs lazily. A pass that has fewer runs than the fan-in carries the untouched runs forward without rewriting them.

Number of passes is `1 + ceil(log(ceil(N/B)) / log(B-1))` for N > B. The sort engine is shared by `sort`, `distinct`, `group_by` and the set operators, so they all use the same pass and counter rules. A standalone sort streams its final merge lazily; the set operators and sort-merge join request a *materialised* final run, so two sorts in one plan never hold their merge readers simultaneously. The lazy merge's reader pages are recorded in the budget's peak with `Budget::observe`.

## Join Algorithms

**Nested Loop Join (BNL):** Block nested loop reads `B - 2` pages of the outer into memory, then streams the entire inner one page at a time, testing every pair. This gives comparisons = |R| × |S| regardless of B (matches specification). Frames: up to B-2 outer blocks + 1 inner page + 1 output.

**Sort-Merge Join (SMJ):** Equality joins only. Both inputs are sorted by the join key(s) using materialised sorts, so the left sort's run is on disk while the right sort runs. The merge cursor advances through both sorted streams. When keys match, the right input's equal-key run is spilled to a temporary `RunReader`; each outer tuple of the run then rewinds that inner run and emits every pair - this produces every pairing for duplicate keys (correct per §2.8) while allowing an inner run larger than `B`. The merge itself holds three frames.

**Hash Join (HJ):** Grace hash join with two phases. Partition phase chooses `P = B - 1` partitions and hashes tuples of both inputs into partitions on disk using a fixed-seed hash of the join key. In the probe phase, for each partition pair, build an in-memory hash table on the left (build) partition and probe with the right partition, emitting matches. If the build partition is still larger than the `B-1` partition budget, the pair falls back to a block nested loop join over the two spilled partitions; this is the documented overflow choice of §2.9. The build side is the left input as specified.

## Distinct and Group By

**Distinct:** Implemented as sort-based or hash-based (mode selected by plan). Sort-based sorts the input and drops adjacent equal tuples. Hash-based builds a hash table of seen keys and partitions to disk on overflow.

**Group By:** Supports COUNT, SUM, AVG, MIN, MAX. Edge cases: no grouping columns and empty input produces one row with COUNT 0 and other aggregates undefined (errors if requested); grouping columns with empty input produces zero rows. AVG is computed as `round(sum / count × 10⁴)` and returned as an `INT` scaled by 10⁴ (the value times 10,000), per §2.6; there is no separate real type.

## Set Operators

Union_all concatenates streams. Union removes duplicates (via distinct-like behavior). Intersect and Except use set semantics and work when both inputs exceed memory by appropriate partitioning/sorting strategies.

## Schema and Comparisons

Schemas preserve qualified column names. Join output is left schema followed by right schema. Column types are Module 1's `ColumnType`, reused unchanged — the execution engine adds no type of its own. Comparisons follow Module 1 rules: same type only; INT vs VARCHAR is an error. VARCHAR comparison is byte-wise UTF-8.

## Counters and Tracing

Each operator maintains counters for tuples_out, comparisons, page_reads, page_writes. Trace output lists each operator with depth and its counters. Run output includes the schema, the rows, and totals across all operators; the budget's peak is available through `Budget::peak` for tests and reports rather than in the §4 JSON.
