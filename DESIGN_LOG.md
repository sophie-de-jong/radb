# Design log

One entry per working session, dated, in roughly chronological order. Most
sessions were pair-work with an AI assistant; the things that went wrong are
called out explicitly, because those were how the mistakes surfaced.

## 2026-09-01 — First draft of the grammar

Tried to write the EBNF in one pass, leaning on an AI that had "an answer"
for the precedence table. Its first proposal made `union` bind looser than
`minus` (a "English reading" of the operators), which is fine for the spec's
example `A union B minus C`, but I could not get a consistent story out of
it for `A minus B minus C`. Writing the parse trees for both candidates on
paper showed the trees differed, which is the whole ambiguity problem. That
is when I decided all three set operators share one precedence level and are
left-associative, and wrote it into the grammar instead of leaving it to the
implementation.

## 2026-09-02 — Reading the target system

Spent the day in Relax confirming semantics: how a self join is expressed,
and what projection does to duplicates. Caught that `project` in Relax
removes duplicates (set semantics), which I would have gotten wrong.

## 2026-09-04 — Tokenizer

Asked the AI for a tokenizer. It built one that split the input on spaces,
which sailed through the pretty query and failed rows #1 (`select[x1=3](R)`,
no whitespace at all), #5 and #6 (a `)` and a `,` inside a string literal).
The failure was found by running the §7.1 tokenizer test list, where the
no-whitespace/inside-string cases are exactly the ones a whitespace splitter
cannot see. Replaced it with a per-character scanner. Second bug the same
session: the AI's string scanner treated `''` as close-then-reopen, so
`'O''Brien'` came out as the empty string; fixed to a single literal quote.

## 2026-09-06 — Qualified names

An AI lexer emitted `Emp.DID` as three tokens (`Ident`, `Dot`, `Ident`).
That forced the parser to understand qualifiers as a special grammar rule,
and the join tests kept failing on ambiguity. The fix that stuck is the
opposite decision: lex `Emp.DID` as a single `IDENT` containing the dot, so
qualification is free anywhere an attribute name is legal. Documented in
GRAMMAR.md §1.1/§"Qualified names". There was a second, subtler bug that day:
the AI suggested unqualified `DID` resolution by matching the last
component of a qualified column, which turned out to need an ambiguity check
(if both sides of a join have `DID`, `select[DID=...]` must be an *error*,
not a silently-chosen column). Found it by hand-writing the two-column
output schema of case #19.

## 2026-09-08 — Parser and tree printing

Wrote the recursive-descent parser by hand. The first version had a bug
where `project[Name, Name]` silently kept both names; the complaint was that
"the parser doesn't have the schema" — true, and the reason that check was
moved to execution time and documented in GRAMMAR.md as a decision. The tree
printer took three tries to match the example format in §6.2 (connector/│
indentation), but it is now byte-compatible with the spec's sample output.

## 2026-09-10 — The six operators

`times` and `join` shared a real bug in the first draft: the join condition
was resolved against each input's schema separately, so `Emp.DID=Dept.DID`
worked only because those names happened to exist, and a comparison like
`Name=Name` silently compared the wrong side or crashed. Fixed by resolving
the condition against the *concatenated* qualified schema (left columns as
absolute indices, right columns offset by the left arity). Caught only
because test #19 checks the output schema keeps both DID columns.

## 2026-09-12 — Error handling

Added the five error categories (§6.3) as distinct types. Running the CLI
against `select[Age>30](R` showed the "(expected `)`)" path worked, but an
unterminated string at position 0 (`'abc`) produced a Rust panic with a
backtrace — a literal violation of "never show a stack trace". The cause was
an off-by-one in the lexer's error position (`at - 1` underflowed at byte 0);
it also reported the character *before* the opening quote everywhere else,
contradicting the doc comment. Fixed to report the opening quote. This one
came from my own testing, not from the AI — the AI had never run that input.

## 2026-09-13 — Data generator and instrumentation

The generator's first version drew the `b` values uniformly but forgot that
`S` must be a set, so at a small b-domain it produced fewer than `m` rows.
Fixed by rejecting duplicate (b, c) pairs. The join counter was moved into
`Engine` alongside the examine-exactly-once selection counter, and verified
against the arithmetic (n×m comparisons, n examinations) rather than
trusted by eye.

## 2026-09-14 — Round trip and cleanup

Tied the pieces together: `radb-study write` emits §4.1-format files, `ra -r` loads
them, and a query runs on them end to end. Writing the loader exposed a
third AI-only-wrong turn: an early tokenizer draft I kept for the loader
also did whitespace splitting, so a bare value with a space (`John Smith`)
silently became two values. The spec's rule ("bare strings must be quoted if
they contain a space") is now enforced as an error in `parse_tuple`.
Finished GRAMMAR.md, README.md (self-join explanation) and this log.

## 2026-09-21 — Relation API rework

Re-read the spec looking for where the public `ColType` and the load-time
column-homogeneity check are actually demanded. They are not: §4.1 only says
a value is a number or a string, and the only type rules the spec forces are
"compatible types position by position" for set operations (§4.3) and the
int-vs-string comparison error (§4.3, case #22). So `ColType` is gone and
`Relation`'s schema is again just `Arc<[String]>` — column names, nothing
else. Types are checked by comparing `Value` variants directly
(`Value::kind()` returns "int"/"str"), and the trick that keeps it cheap:
since every column is homogeneous (`push` maintains that invariant), a new
tuple only has to be compared against *one* previous tuple, and the set-op
compatibility check against *one* tuple from each side — O(arity), not
O(rows). Structure: the old tuple-vector `Relation::new` became
`Relation::new(schema)` plus `push(row) -> Result<bool, RowError>`, where
the bool reports whether the tuple was a duplicate (set semantics) and the
error is arity or a type mismatch; the loader maps those `RowError`s to the
same positioned `ParseError`s, so all 25 rows stayed green with no message
changes. My first attempt at this rework kept a private per-column `Kind`
enum in the schema — that was over-engineering; the representative-row
variant check is what actually removes the type plumbing for good.

Follow-up in the same session: the operators were re-implemented as methods
on `Relation` (`select`, `project`, `rename`, `times`, `join`, `union`,
`minus`, `intersect`) instead of being hand-rolled inside the `Engine`'s
`execute` loop, so the loop is now just "resolve names, build a condition
closure, call a method". The condition-taking ops (`select`, `join`) take a
closure: `FnMut(&[Value]) -> Result<bool, SemanticError>` for selection and
`FnMut(&[Value], &[Value]) -> Result<bool, SemanticError>` for the join
pair. That moved the §4.3 set-op compatibility check and the colliding-
qualified-name check into the relation type where they belong. Two things
worth noting: (1) the `Engine` still resolves the parsed `RCond` against the
schema before calling `join`, and still runs the duplicate-column check
*before* that resolution, so a colliding qualified name still fails as a
schema error instead of an ambiguous-attribute error; (2) the §8.2 counters
live in the closures (`stats.join_comparisons += 1` per pair, `selection_
examinations += 1` per tuple) so they are incremented by the same code that
performs the op, not approximated around it.

Same session, second pass: `Relation::project` now takes the column *names*
(`impl IntoIterator<Item = I>, I: Into<String>`) instead of pre-resolved
index/name pairs, and resolves them against the relation's own schema
(including the case-#24 duplicate-projection error). The engine's project
arm collapsed to `self.execute(input)?.project(attrs)`. I also re-checked
`rename` against §4.3 ("same attributes under a new relation name") by
hand: the strip-one-qualifier logic is sound because the grammar's `WORD`
cannot contain a dot (`IDENT ::= WORD ( "." WORD )?`), so a schema column
name has at most one qualifier dot and renaming can never mangle a bare
name. The one corner the spec does not pin down is renaming an
already-qualified result such as `rename[E2](Emp times Dept)`: both `DID`
columns collapse to `E2.DID`, and the engine leaves that (deterministic,
and it shows up as a duplicate-column error if the result feeds a join or
times).

Re-examined the join arm on request: the row mechanics were already in
`join`; what stayed in the engine was query-driven (qualification, condition
resolution) plus a duplicate pre-check that must run before the condition
resolves, so a naive self-join reports the documented schema error instead
of an ambiguous-attribute one. The one genuine redundancy — `combined`, the
two schemas concatenated — was computed in both places; it is now a single
`Relation::combined_schema` helper used by `times`, `join`, and the engine
arm.

Third pass, requested by the owner: the relation-definition parser stopped
looking like the query parser. Fixed in three ways. (1) *Style*: the §4.1
header no longer uses its own `_def`-suffixed helper clones (`expect_def`,
`parse_attr_name_def`) and broad `MalformedHeader` catch-all; it goes
through the same `expect`/`unexpected`/`parse_attr_name` machinery as
queries, so `R a, b)` is an `UnexpectedToken` just like a malformed query
is. (2) *Positions*: every location-carrying `ParseError` variant now has
one shape, `at: Position` (line + column) — previously `MalformedHeader`
had only `line`, `ColumnTypeMismatch`/`EmptyValue`/`MustQuote` had line and
a separately-reckoned column, and `DuplicateAttribute` had none. `Position`
is the query/lexer's 0-based-column type, so relation errors now match the
query errors column-for-column. (3) *Build as you parse*: `relation_from_parts`
is gone. `parse_relation_def` returns `(String, Relation)` and pushes each
tuple into the `Relation` the moment it is read (`Tuple ::= ...` is enforced
inline), so arity and column-type violations fail at the exact offending
value's position — `ColumnTypeMismatch` even points at the bad token, having
kept each [`Value`]'s [`Position`] while scanning the line — and duplicate
attributes are caught at the second occurrence. GRAMMAR.md §1.2 was updated
to say the load-time rules are enforced by the parser itself as it loads,
and the stale `Relation::add_row` naming (now `push`) was fixed.

Fourth pass: after re-reading the brief (INSTRUCTIONS.txt §4.1, §4.2, row
#8), decided keywords-as-names split into two rules. *Loose:* `AttrName ::
= IDENT | KEYWORD` everywhere it appears — condition operands (row #8
requires `select[union=3](R)` to parse), `project[]`/`rename[]`, and the
relation-definition header's attribute list, so a column really can be
named `union` and row #8 executes against it (a documented widening of
§4.1's "attribute names are identifiers"). *Strict:* `NAME ::= IDENT` —
the header no longer accepts a keyword as a relation name. This fixed a
doc/code drift: GRAMMAR.md already claimed `NAME ::= IDENT` while
`parse_rel_name` accepted keyword tokens, and a keyword-named relation was
unreachable anyway because query atoms accept only `IDENT`. Tests: new
`keyword_spelled_relation_name_is_rejected`; the existing
`keyword_spelled_attributes_are_accepted` (header *columns*) stayed green.

## 2026-09-21 — Tuple parsing is comma-driven, not line-driven

On request, `parse_tuple` no longer owns the relation it fills: it returns
the parsed `Vec<Value>`, and `parse_tuple_list` pushes it — the 
`Relation` now lives in exactly one place, the caller. The per-value
`Position` vector is gone; a tuple's *starting* position is the single
position reported for its arity or column-type error, so the positioned
tests for those errors stayed green unchanged. And tuple separation is now
the grammar's, not the file format's: there is no "one tuple per line"
rule anymore. Whitespace is insignificant and a tuple ends when its last
value is not followed by a comma (`Tuple ::= Value ( "," Value )*`), so
`Hello World` is two tuples (`Hello`, `World`) instead of a must-quote
error, and `1, 2, 3, 4` is one tuple even across lines. Two consequences
were locked in as tests: a dangling comma before `}` is now the documented
`EmptyValue` error instead of being silently accepted, and a bare value
containing `(` still fails with `MustQuote` (the paren cannot continue a
tuple). GRAMMAR.md §1.2 and the relations tests were updated to match.

## 2026-09-21 — Public `Row` type; `Relation` fields fully private

`Relation`'s `schema`/`rows` fields are now private, with the public surface
grown to match: a new `Row` type wraps the row storage as an `Arc` so rows
clone in O(1). Note the brief said `Arc<[String]>`, but rows hold typed
[`Value`]s (Int/Str — the §4.3 type checks and §8 study depend on it), so
`Row` wraps `Arc<[Value]>`. `Row` derefs to `[Value]` and implements
`AsRef<[Value]>`, plus `From`/`FromIterator` for `Vec<Value>`,
`[Value; N]`, `&[Value]`, and `Arc<[Value]>`, so `push` now takes
`impl Into<Row>` (call sites just move their `Vec`/array). New
`Relation::iter()` yields each tuple as a cheap `Row` clone and
`Relation::contains(impl Into<Row>)` is an O(1) `HashSet` membership test.
All external `.rows`/`.schema` field uses (parser unit tests, §7
integration tests, the `radb-study` binary) were converted to the accessor
API; the engine's own tests use it too. Test count unchanged: 65 green
(30 unit + 5 bin + 30 integration), clippy clean.

## 2026-09-21 — Study output matches §8.3; tree-printing tests removed

On request, `tests/tree.rs` was deleted: the §6.2 parse-tree example is
demonstrated by hand instead of pinned as a test, and rows #10/#11/#15
keep their executable coverage in `tests/2_grammar.rs` (AST-shape tests),
so the §7 table is still fully tested (61 tests: 30 unit + 5 bin + 26
integration). README's test layout and GRAMMAR.md's
`tests/grammar.rs`/`tests/semantics.rs` references were updated to the
renumbered files.

`radb-study study` printed its own merged table
(`n | join (ms) | comparisons | join tuples | select (ms) | examinations |
project (ms)`) — unit and mix-up, since §8.3 defines a single table with
columns `n | m | comparisons | wall time (s) | output tuples`, and §8.4 q3
asks for select/project *separately at the same sizes*. The binary now
prints exactly the §8.3 join table (wall time in seconds, not ms) followed
by the §8.4 q3 select/project table, both paste-ready into REPORT.md as we
already had them. Only the instruction-defined measurements run — no extra
experiments. The stale `--dir` flag mention in the usage comment was
dropped (the `Write` subcommand no longer has it).

## 2026-09-21 — `study` no longer looks hung

The full `study` sweep (1000..64000) takes about two minutes wall time —
the 64k×64k join alone is ~110 s here — but a restructure the same session
had buffered every table row and printed *nothing* until all seven sizes
finished. Two minutes of dead-silent terminal reads as "hanging
indefinitely", and it was: the code was fine, the feedback was not. Fixed
by streaming: the §8.3 table header prints first, each join row prints the
moment its size completes (with an explicit `stdout().flush()` so it shows
even when piped), and a `join@{n}: running…` progress note goes to stderr
before each join so the multi-minute gap between sizes has an alive
signal. The binary also announces the sizes it is running and warns when
it is a debug build (`cfg!(debug_assertions)`), where the 64k join can
take many minutes. Root-cause note for next time: with buffered output, a
slow-but-finite O(n²) benchmark is indistinguishable from a hang.

Second fix in the same session: the generator's S-tuple loop
(`while s_rows.len() < m`, rejecting duplicate (b, c) pairs) is the one
genuinely unbounded loop in the codebase. It terminates whenever the
(b, c) space (b-domain × 1,000,000) is at least m, which the defaults
satisfy easily — but `write --m 2000000 --matches 2000000` makes the
b-domain collapse to 1, leaving only 1M possible pairs for 2M rows, and
the loop spun forever. `generate_relations` now returns
`Result<(Relation, Relation), String>` and checks that space up front;
infeasible parameters fail with a clean message instead of hanging. Call
sites updated (`write` maps it to an `io::Error::InvalidInput`, `study`
expects, tests `.unwrap()`).

## 2026-09-21 — Join evaluation rework: fast path tried, rejected, replaced

The study ran correctly but slowly (64k join ≈ 109 s; the general
condition interpreter clones both operand `Value`s for every pair — a
`String` allocation per pair on string columns — and re-decides which side
of the join each column is on, every evaluation, ~27 ns/pair). First
attempt at a fix was a fast path: when a join condition was exactly
`left_col OP right_col` — the study's own `R.b=S.b` — the engine compiled
it to a direct per-pair column compare (`Relation::join_columns`), ~6
ns/pair. It worked (64k join 108.9 → 25.3 s, full sweep ~131 → 33 s), and
the owner rejected it on the right grounds: it is overfit. It helps
precisely the one condition shape §8.3 measures, so most of the speedup
was matching the benchmark rather than improving the engine. Removed on
request: `join_columns`, the engine's shape-detection in the join arm, and
its two tests.

The real problem the fast path was hiding is the *general* interpreter's
per-pair allocation, and fixing that fixes every condition shape at once.
`RExpr::eval_single`/`eval_pair` (which returned owned `Value`s) are gone;
`RExpr::operand` now returns a borrow-based `ROperand` — a column value
read by reference from the tuple, or a constant borrowed from the
condition tree, so string constants stop being cloned per pair too.
`compare_operands` reduces each side to a Copy `Scalar` (int or borrowed
str) and compares with no clones and no allocation; `Value::compare` was
folded into it, and the int-vs-str `TypeError` is unchanged (§7 #22 and
the join-type-error unit test both still pass). Select (`eval_single`)
reuses the same borrow machinery, so it gains too. The §8.2 counter still
increments exactly once per pair examined, and the n×m comparison counts
are byte-identical to every earlier run. Measured on this machine: 64k
join 108.9 → 56.2 s (1.9×, ~13.7 ns/pair), full 1000..64000 sweep ~131 →
69 s. Not the ~25 s the fast path reached — that is the point: this
speedup applies to *any* condition, and `R.b=S.b` benefits only as one
instance of the general rule. REPORT.md was re-run and refreshed in the
same session (new §8.3/q3 tables, new q5 match-rate run 0.572/0.576/0.812
s at rates 1/5/50, slope 2.04, `n = 10⁶` prediction ≈ 3.5–4 h).

Output pass (same session): progress goes to stderr (streaming, with
thousands separators — each row appears the moment its size finishes), and
stdout carries only the two paste-ready tables, aligned and
right-justified (`---:`), so `radb-study study > results.md` captures
exactly the deliverable. A first draft of this restructure lost the
`stdout().flush()` from the old streaming code (tables appeared mid-run
only after the separator row); the rewrite made the stderr/stdout split
explicit instead of trying to stream the table itself.

## 2026-09-21 — Condition evaluator de-cluttered

Owner liked the speed but not the plumbing around it: the borrow-based
evaluator staged operands through *two* intermediate types
(`RExpr` → `ROperand` → `Scalar`) plus four loose helper functions
(`scalar_of`, `compare_operands`, `cmp_ints`, `cmp_strs`). Collapsed to
one eval-time type: `RExpr::operand` is now `RExpr::value`, which reads a
column value straight into a `Scalar` (int or borrowed str), and
`Scalar::compare(op, other)` does the whole comparison including the
spec's int-vs-str `TypeError` (the helper functions were folded in). The
hot calls read as `l.value(left, right, base).compare(*op,
r.value(left, right, base))` — one conversion instead of two. Behavior is
identical (tests unchanged, output tuples and comparison counts
byte-identical), and — a bonus this pass was not aiming for — the
resulting code measures *faster*, not just the same: the intermediate
`ROperand` staging and loose free functions had been stopping LLVM from
fully inlining the comparison chain into the join loop. Re-measured on
this machine the 64k join dropped from 56.2/60.0 s (two runs) to
36.6/39.2 s (two runs, ~9 ns/pair), all sizes ~1.5×, and the full
1000..64000 sweep is ~50 s. REPORT.md and README were refreshed with the
new tables and timings.

## 2026-09-21 — Compare operators compiled to three-way Ordering at resolve time

Follow-through on the de-cluttering pass: `Scalar::compare` (and
`cmp_int`/`cmp_str`) still matched the grammar's `CompareOp` six ways *per
pair*. Compiled the operator out in `RCond::resolve` instead. The engine's
private condition AST now carries `std::cmp::Ordering`: `<`, `=`, `>`
become `RCond::Cmp(l, r, want)` (evaluates `a.cmp(b) == want`), and `>=`,
`<=`, `!=` become `RCond::InvertCmp(l, r, want)` (`a.cmp(b) != want`);
`Scalar::compare(op, other)` is now `Scalar::matches(want, other)`. Grammar
side untouched — `CompareOp` still lives in the parser and in the §7
parse-tree tests — and select reuses the same machinery. Added a unit test
pinning all six operators on both int and str columns.

Measured two encodings because they do not compile identically in the hot
loop. The box was under memory pressure during measurement (≈2 GB of swap
in use; a single run was meaningless), so each was measured pinned
(`taskset -c 3`), round-robin, best-of-N at 64k:

    baseline, six-way `match op`  : min 37.4 s  (spread 37–135)
    (1) Cmp + InvertCmp variants  : min 39.5 s  (two clean back-to-backs)
    (2) one Cmp + invert `bool`   : min 46.0 s  (consistently slowest)

(2) loses because the invert flag is a per-pair load+branch in the hottest
loop that LLVM cannot hoist. (1) is within noise of the baseline (5% on
best-of-N, indistinguishable at 32k) and, unlike the baseline, leaves no
six-way dispatch in the inner loop, so it was taken. Final full sweep
(best of two, same pinned conditions): 64k 43.0 s (~10.5 ns/pair), slope
2.04, `n = 10⁶` prediction ≈ 2.8–2.9 h; q5 match-rate run 0.477/0.493/0.716
s at rates 1/5/50. REPORT.md and README refreshed; run-to-run spread noted
in the report.