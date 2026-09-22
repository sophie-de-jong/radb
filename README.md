# radb

A small relational-algebra query engine in Rust, built against the table in
the project spec. See `GRAMMAR.md` for the tokenization, precedence, and
associativity rules the parser follows.

## Running a query

```
cargo run --bin radb -- --tree "project[Name](select[Age>30](Employees))"
```

prints the parse tree for a query without executing it:

```
Project(attrs=[Name])
└── Select(cond=Gt(Attr(Age), Num(30)))
    └── Relation(Employees)
```

To actually execute a query, load relation files first. They use the
relation-definition syntax from spec §4.1 (lines starting with `//` are
comments, blank lines are ignored, duplicate tuples collapse, and strings
are quoted with `''` for a literal quote):

```
cargo run --bin radb -- -r R.txt -r S.txt "R join[R.b=S.b] S"
```

Every error is printed as `Error: <message>` with a position where they have
one; the program never prints a stack trace (§6.3).

## Running the tests

```
cargo test
```

- `tests/1_tokenizer.rs` — spec rows #1–#9
- `tests/2_grammar.rs` — spec rows #10–#17
- `tests/3_semantics.rs` — spec rows #18–#25

`tests/` holds exactly the §7 rows. The
additional tests live next to the code as unit tests:

- `src/parser.rs` — §4.1 relation-definition loading and positioned errors
- `src/engine.rs` — `Relation::push` checks, set-op type edge cases, and
  the §8.2 instrumentation counters
- `src/tokenizer.rs` — lexer edge cases
- `src/bin/radb_study.rs` — §8.1 data-generator tests

All 25 rows of the §7 table are covered.

## Section 8 — the performance study

```
cargo run --release --bin radb-study -- write --n 1000 --m 1000 --matches 5
cargo run --release --bin radb-study -- study
```

`radb-study write` writes `R.txt` and `S.txt` in the §4.1 relation-definition
syntax (load them with `radb -r`). `radb-study study` runs the §8.3 join
experiment at sizes 1000 through 64000 tuples per relation, then measures
select and project at the same sizes (§8.4 q3). Progress streams live to
stderr (each size's result appears the moment it finishes), and stdout
carries only the two paste-ready tables — the join table exactly as
INSTRUCTIONS.txt defines it (`n | m | comparisons | wall time (s) |
output tuples`) plus the select/project table, aligned with thousands
separators — so `radb-study study > results.md` captures just the
deliverables. The whole sweep takes about a minute; run it with
`--release`.

The engine joins by nested loop (one comparison counter per pair examined,
§8.2). Conditions are resolved once per query into a small tree and then
evaluated per pair through borrowed operands: a column comparison reads
both Values by reference, and a string constant lives in the tree and is
borrowed, never cloned — so evaluating a comparison allocates nothing, for
any condition shape (the study's `R.b=S.b`, `and`/`or`/`not`, constants).
The six comparison operators are compiled the same way, down to a
three-way `Ordering` plus (for `>=`, `<=`, `!=`) a not-flag, so the
per-pair loop never dispatches on the operator. The optimization is
constant-factor only: every pair is still examined and counted exactly
once.

## Self-join: why `rename` is required

Case #20 is `rename[E2](Emp) join[Emp.MgrID=E2.EID] Emp` — "who is each
employee's manager". To answer it you need *two distinct copies* of `Emp`:
one for the employee, one for their manager.

If you naively write `Emp join[Emp.MgrID=Emp.EID] Emp`, the two operands are
literally the same relation, so after qualification (§4.3: "`times`:
all attributes of both inputs, qualified by relation name") every output
column is named `Emp.EID`, `Emp.Name`, … — the *same* names on both sides.
That makes the join's output schema unusable (the two `DID` columns are no
longer distinguishable, and the engine reports the duplicate as an error),
and worse, the join condition `Emp.MgrID=Emp.EID` is ambiguous: both sides
contribute an `Emp.MgrID` and an `Emp.EID`, so there is no way to say
"this employee's MgrID equals *that* copy's EID".

`rename[E2]` gives the second copy its own name, so its columns come out as
`E2.EID`, `E2.Name`, … (spec §4.3: "rename: same attributes under a new
relation name"). Now the two occurrences have two distinct names, the
condition `Emp.MgrID=E2.EID` unambiguously compares the employee copy's
`MgrID` with the manager copy's `EID`, and the output schema keeps both
sides apart. This is the general self-join problem: any time a query needs a
relation twice, the two uses must be distinguishable, and `rename` is the
mechanism that makes it possible — without it, self joins simply cannot be
expressed.

## Known limitations

- Relation files are read whole into memory (loading is in scope only for
  the `radb` front end; the spec says data storage is out of scope).
- The join is a nested loop; no indexes or hash/sort-merge joins. That is
  deliberate — §3 and §8 ask you to measure exactly this.
- Relation names must be identifiers; a relation named after a keyword
  (e.g. `union`) cannot be referenced. Attribute names *can* be keywords —
  see GRAMMAR.md, "Keywords as attribute names".