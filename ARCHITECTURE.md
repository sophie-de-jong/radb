# Architecture

## Overview

`radb` is a relational database in Rust. The four modules are library layers
with JSON contracts between them, so each can be built and graded on its own:

```
SQL text
  │
  ▼
[M3] scanner → parser → name/type/aggregate checking → logical plan JSON
  │
  ▼
[M4] rewrites → statistics/selectivity → cost model → DP join order → physical plan JSON
  │
  ▼
[M2] iterator tree: open / next / close under a B-frame budget
  │
  ▼
[M1] buffer pool → heap files → 4096-byte pages on disk
```

The catalog and Module 4 statistics live in M1's `catalog.db` and are read by
M3 (names and types) and M4 (row counts, histograms).

## Layers and ownership

| Layer | Code | Contract |
| ----- | ---- | -------- |
| Shared types | `src/schema.rs`, `src/value.rs` | `Schema`/`Column`/`ColumnType`, `Value`/`Row`, `SemanticError` |
| M1 storage | `src/storage/` | `Disk`, `BufferPool`, `HeapFile`, `Catalog`; `catalog.db` |
| M2 execution | `src/exec/` | physical plan JSON in, rows and per-operator counters out |
| M3 SQL front end | `src/sql/` | SQL text in, logical plan JSON out |
| M4 optimizer | `src/optimize/` | logical plan JSON in, physical plan JSON out |
| Integration | `src/database.rs`, `src/bin/radb.rs` | SQL text in, rows out; `Database` wires M3→M4→M2→M1 |
| Bulk loader | `src/csv.rs` | typed rows from CSV; shared by M1's `load` and the shell |

Each `src/bin/radb_*.rs` is only a command-line adapter: clap arguments,
reading an input file, and printing a result the library produced. The shell's
command parser lives in `src/bin/radb.rs`, also an interface layer.

## Interfaces between modules

- **M3 → M4** is a logical `Plan`: `scan`, `select`, `project`, `join`,
  `cross`, `rename`, `group_by`, `distinct`, `sort`, `limit`, `union`,
  `union_all`, `intersect`, `except`. A condition is a typed `Condition` whose
  JSON form is a string in the shared predicate language. No physical name
  appears.
- **M4 → M2** is a physical plan: `seqscan` (with an optional self-join
  `alias`), `filter`, `project`, `distinct`, `sort`, `group_by`,
  `nested_loop_join`, `sort_merge_join`, `hash_join`, `union_all`, `union`,
  `intersect`, `except`, `limit`. It is exactly the JSON M2 parses.
- **M2 → M1** goes through `Context`: a child operator fetches pages through the
  buffer pool and asks the catalog for table metadata. Temporary runs are
  `TempFileDisk`s deleted on drop.
- **M1 → M3/M4** is the `Catalog`: table names and schemas for resolution, and
  the `Stats` blob stored beside each table for estimation.

## Design decisions and lessons

The main boundaries were chosen so a module can be tested without later ones:
M2 consumes hand-written plans, and M4 produces the same JSON without running
an executor. The two places this pays off most are the plan JSON codecs and the
shared typed condition tree; the two places it costs are the self-join scan
alias (needed because the physical plan has no `rename`) and the need for M4's
estimator to reproduce M2's cost formulas exactly.

If starting again, I would:

1. fix the JSON contracts before writing any operator;
2. write the memory account before the first blocking operator, not after;
3. keep statistics and the catalog versioned from day one;
4. make every command return a value from the library and leave printing to the
   binary, which is the shape the code now has;
5. test each module against the worked examples in its own specification before
   moving on.
