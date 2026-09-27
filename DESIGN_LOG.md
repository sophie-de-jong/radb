# Design log

One short entry per working session, dated. Most sessions are pair-work with an AI assistant. Each entry says what I was after, what I tried, and what broke. The section at the end collects the occasions the AI gave me something wrong, and how I found out.

## 2026-09-14, session 1 — tests, grammar, engine, study binary

Tried to finish the §7 test rows and get the study actually running: had the
AI write `tests/grammar.rs` (#10–#17) and `tests/semantics.rs` (#18–#25),
write GRAMMAR.md, implement `Engine::execute` and `datagen::generate`
with the §8.2 counters, rewrite `src/bin/gen.rs`, and write REPORT.md from a
real sweep. Then implement `parse` once there were tests to aim at. Broke on
my side: my hand-written tokenizer's string arm never consumed the opening
quote, so `select[DID='D1'](R)` lexed as `Str("")`. Broke on the AI's twice
(see both below). 26 tests.

## 2026-09-14, session 2 — spec-conformance pass

Asked only whether the project matched the specs so far. Two §6.3 "never show
a stack trace" violations were live and both were panics: `tokenize("'abc")`
underflowed at `at - 1`, and `Position::new` panicked on a multi-line input
ending in a newline. Added what was missing, `tree_string()` (§6.2, matched
the spec's example first try), `load_relation()` (§4.1) and the `ra` CLI. 
56 tests.

## 2026-09-20, session 1 — splitting `lib.rs` into four modules

Asked for tokenization, parsing and the engine to be separate modules, with
the parser owning a `Tokenizer` type instead of a `Vec<Token>`. Everything had
to be re-fitted to the `Relation` I had changed in the meantime. Rows now were
`HashSet<Arc<[Value]>>`, which makes set semantics a property of the type.
One instrumentation test that loaded ten identical tuples to get 10×10
pairs quietly collapsed to one row, because the type now does what §4.1 says.
The refactor's own test run then exposed a latent bug: the lexer emits
`TokenKind::QualIdent` for `R.b`, and the AI's parser only ever accepted
`TokenKind::Ident`. 51 tests.

## 2026-09-20, session 2 — relation parsing, clap, trees in `Display`, column types

Moved §4.1 relation parsing out of `engine.rs` into `parser.rs` on top of the
`Tokenizer`, then had it rewritten to mirror the GRAMMAR.md rule instead of
being a second dialect of the query parser, merged `DefError` into
`ParseError`, put both binaries on `clap`, and deleted `tree_string()` so each
AST type's `Display` owns its own node name and connectors. The five grammar
tests that failed during the move were already broken (see below); after fixing
the real cause I had it encode per-column types in the schema
(`ColType`, `Arc<[Column]>`) and delete one compatibility check the `Relation`
invariants already covered. Between turns I renamed `ra` to `radb` and `gen` to
`radb-study`, which left the docs stale. 61 tests.

## 2026-09-21, session 1 — relation rework, then the join hot loop

The long one. Morning: re-read the spec, found the `ColType` I had added the
previous evening is not demanded anywhere, and removed it. `Value::kind()`
comparisons are enough, so the schema is `Arc<[String]>` again and `push`
maintains column homogeneity by itself. Then the relation operators became methods on
`Relation` taking closures, the relation-definition parser was made to match
the query parser's style and error positions, and `Row` was made a public
O(1)-clone type. Afternoon: the study looked hung (buffered output over a
two-minute sweep), then looked slow (≈109 s for the 64k join), which produced
two rejected attempts and three accepted ones: the fast path out as overfit,
then borrowed operands, one `Scalar` type, and the six comparison operators
compiled to `Ordering` at resolve time. 64k join 109 s → 43 s.

## 2026-09-21, session 2 — tuple parsing

Asked for `parse_tuple` to stop owning the relation it fills, and later for it
as a post-condition loop. It now returns `Vec<Value>` and `parse_tuple_list`
pushes; a tuple ends when its last value is not followed by a comma, so
`Hello World` is two tuples and `1, 2, 3, 4` is one tuple across lines. The
suite would not compile at all when I got there: `tests/tokenizer.rs` still
expected the pre-keyword-split token API, and `Keyword` was never re-exported
from `lib.rs`. 66 tests.

## 2026-09-22, session 1 — pre-submission audit, and the first commit

Went through the specs against the project and found four real gaps: no
git commit at all (§9 wants a link), README commands naming a `ra` binary that
had been renamed two days earlier and a `data/` directory that never existed,
REPORT.md stating the log–log slope with no plot, and a `union-or-minus-etc.`
placeholder that AI had generated in GRAMMAR.md §3's ambiguity demonstration. Fixed all four; the plot is a stdlib-only Python script writing a two-panel SVG. Committed the repository's first commit.

## 2026-09-26, session 1 — spec revision, re-measurement, docs pass

Worked on this one alone, no AI. I renamed `Stats::selection_examinations` to
`select_comparisons`, moved the select and project progress lines to stderr,
and left stdout carrying only the §8.3 join table. I re-ran the sweep and
pasted the binary's own output into REPORT.md, which moved the 64k join from
42.960 s to 30.627 s and q4's estimate from about 2.9 hours to about 2. Broke
the §4.1 and §6.3 error coverage: taking the inline documentation and the ~20
unit tests out of `src/parser.rs` left `ParseError::MustQuote` and
`EmptyValue` unreachable. The docs had drifted from the code, README
still listed the deleted parser tests and a second stdout table, and the
log–log plot was still drawn from the old numbers. Fixed all of it, redrew
the plot from the new table, refitted the slope to 2.03, and moved the plot
generator into the repository as `tools/plot_loglog.py` on matplotlib (the
brief allows a plotting library for the report), because the only previous
copy lived in a scratch directory and I had already lost it once — which is
how the figure came to disagree with the table in the first place.

## Where the AI was wrong

**The AI's own semantics tests asserted the wrong thing (09-14).** The tests it
wrote expected unqualified output schemas for `times`/`join`
(`EID, Name, Age, Emp.DID, Dept.DID, DName`), but §4.3 qualifies every
attribute by relation name. Found by checking the engine's actual output
against §4.3 while wiring the engine up: the engine was right, the tests were
wrong, so the tests changed.

**The study binary was over-built and printed things §8 never asked for (09-14).** The first `gen.rs` was 261 lines, built its queries as hand-made
ASTs instead of parsing them, and appended a "Derived analysis" block with a
least-squares slope and a predicted wall time at n = 10⁶, a number with no
measured run behind it. Found by putting §8.3's table definition next to the
binary's own stdout; rewritten to parse through `radb::parse` and cut to 173
lines.

**Five grammar tests were broken before I touched anything (09-20).** When the
relation parser moved, five tests failed with "unexpected `)`... expected end
of input", and the obvious reading was that the move broke them. This ended up being a bug in the AI's own query parser from two days earlier, found by looking through
the AI chat log.

**The first join speedup was overfit to the benchmark (09-21).** Asked why the
study took ≈109 s for the 64k join, and the AI added a fast path for exactly
`left_col OP right_col`, the one condition shape §8.3 measures, which took
it to 25.3 s. This is faster because it matches the benchmark, so I had it
removed; the general fix (borrowed operands, no per-pair `String` allocation)
reached 56.2 s and helps every condition shape, not just `R.b=S.b`. Found by
asking what the speedup would do for a condition the study never runs.
