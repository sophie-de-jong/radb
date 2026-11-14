# radb

A database system in Rust, built one module at a time (Project 2). Each module
is defined by its own contract and runs through its own command-line tool; the
last binary wires all four together.

## Modules

- **Module 1 — storage engine** (`radb-storage`, `src/storage/`): 4096-byte
  pages, a slotted data-page layout, heap files of directory and data pages, a
  buffer pool with LRU and CLOCK, and a `catalog.db`.
- **Module 2 — execution engine** (`radb-exec`, `src/exec/`): a pull-based
  iterator model over physical plans, external sort, three join algorithms, and
  a single memory budget `B`.
- **Module 3 — SQL front end** (`radb-sql`, `src/sql/`): a hand-written scanner
  and parser, name resolution, type and aggregate checking, and translation to
  a logical plan. It never chooses a physical algorithm and never moves a
  predicate.
- **Module 4 — optimizer** (`radb-opt`, `src/optimize/`): catalog statistics,
  selectivity and cost estimation, logical rewrites, dynamic-programming join
  ordering, and conversion to the physical plan Module 2 consumes.
- **Integration** (`radb`, `src/bin/radb/`): a shell around `Database` with the
  SQL subset plus `EXPLAIN`, `EXPLAIN ANALYZE`, `ANALYZE TABLE`, `SET frames`,
  and `LOAD TABLE ... FROM`. `Database` also owns the shared bulk loader.

The shared core types are `src/schema.rs` (`Schema`, `Column`, `ColumnType`,
`SemanticError`), `src/value.rs` (`Value`, `Row`), and `src/csv.rs` (the bulk
loader's CSV reader). A logical plan carries typed `Condition` trees; their
JSON form is the condition string M2 and M4 share.

## Building and testing

```
cargo build --release
cargo test
cargo clippy --all-targets -- -D warnings
```

## The storage engine (Module 1)

```
cargo run --release --bin radb-storage -- create-table --db data --name R \
    --schema "a:INT,b:VARCHAR(20),c:INT"
cargo run --release --bin radb-storage -- load --db data --table R --file r.csv
cargo run --release --bin radb-storage -- scan --db data --table R --frames 8 --json
cargo run --release --bin radb-storage -- layout --db data --table R --page 1
cargo run --release --bin radb-storage -- trace --db data --file trace.txt \
    --frames 16 --policy lru --json
```

The CSV file has no header line, one record per line; a field containing a comma
must be quoted. `scan --json` and `trace --json` print the shapes of M1 §7.
`docs/M1/DESIGN_M1.md` gives the page, record, catalog and heap formats;
`docs/M1/REPORT_M1.md` gives the required measurements; `traces/` holds the five
published traces.

## The execution engine (Module 2)

```
cargo run --release --bin radb-exec -- run   --db data --plan plan.json \
    --frames 32 --json
cargo run --release --bin radb-exec -- trace --db data --plan plan.json \
    --frames 32
```

`run` prints the result (JSON with `--json`); `trace` prints one line of
counters per operator. See `docs/M2/DESIGN_M2.md` and `docs/M2/REPORT_M2.md`.

## The SQL front end (Module 3)

```
cargo run --release --bin radb-sql -- parse   --query "SELECT ..." --explain
cargo run --release --bin radb-sql -- compile --db data --file q.sql --json
cargo run --release --bin radb-sql -- check   --db data --file q.sql
```

`parse` needs no catalog: with `--explain` it prints the syntax tree, without it
the token stream. `compile` prints the logical plan (JSON with `--json`,
otherwise an indented tree). `check` prints only the first error, in the one
format of M3 §6, and exits 1.

`docs/M3/GRAMMAR_SQL.md` is the EBNF and precedence table;
`docs/M3/DESIGN_M3.md` covers scope resolution, bag semantics and aggregate
naming; `docs/M3/REPORT_M3.md` maps the required cases to tests.

## The optimizer (Module 4)

```
cargo run --release --bin radb-opt -- analyze  --db data --table R --buckets 20
cargo run --release --bin radb-opt -- plan     --db data --logical q.json \
    --frames 32 --json
cargo run --release --bin radb-opt -- explain  --db data --logical q.json \
    --frames 32
cargo run --release --bin radb-opt -- dp-table --db data --logical q.json \
    --frames 32
```

`plan` emits the physical plan JSON consumed by `radb-exec` without editing.
`explain` prints one line per node with its estimated rows and own page-I/O
cost; `dp-table` prints the enumeration table, one row per subset. See
`docs/M4/DESIGN_M4.md` and `docs/M4/REPORT_M4.md`.

## The integrated shell

```
cargo run --release --bin radb -- --db data
```

The prompt accepts the SQL subset, `EXPLAIN [ANALYZE] <query>`,
`ANALYZE TABLE name`, `SET frames = n`, `LOAD TABLE name FROM 'file.csv'`,
`help`, and `exit`/`quit`.

## Measurements

The M2 and M4 reports are generated from real CLI runs by a reproducible
harness:

```
cargo build --release
python3 bench/measure.py m2
python3 bench/measure.py m4
```

It writes only under `bench/data/` (ignored by git) and prints JSON. The tables
in `docs/M2/REPORT_M2.md` and `docs/M4/REPORT_M4.md` come from those runs.

## Tests

- `tests/storage.rs` — Module 1 integration (pages, heap files, buffer pool,
  the `radb-storage` binary).
- `tests/exec.rs` — Module 2 integration (the `radb-exec` binary).
- `tests/sql.rs` — Module 3 integration (the fifteen required cases and the §5
  examples, through the `radb-sql` binary).
- `tests/optimize.rs` — Module 4 integration (`radb-opt`, explain and DP table).
- `tests/integration.rs` — SQL text through all four modules, including select
  aliases, aggregate aliases and a `FROM` subquery.
- `tests/compiler.rs`, `tests/tokenizer.rs` — the streaming compiler and the
  condition-language lexer.
- Unit tests live beside the code in each library module.

## Reading guide

`ARCHITECTURE.md` describes the system and the interface between each pair of
modules. `docs/` holds the specification, design documents, reports and design
log for each module. `docs/EXCEPTIONS.md` records every deliberate deviation
from the specification so it stays visible.

The integration binary lives in `src/bin/radb/`; the four module binaries are
thin command-line layers (`src/bin/radb_*.rs`) over the library modules.
