# Design log — Module 3 (SQL front end)

A running record of the decisions, the wrong turns and the fixes while building
`radb-sql`. Newest sessions last.

## 2026-10-18, session 1 — the shape of the module

- **Layering.** Put the front end in `src/sql/` as `error`, `tokenizer`, `ast`,
  `parser`, `resolve`, `translate`, `plan`, one concern per file, mirroring how
  Module 1 and Module 2 are split. The binary `src/bin/radb_sql.rs` owns every
  print, so the library stays free of output and the tests can use either.
- **Shared types instead of new ones.** A comparison operator is the existing
  `CompareOp`, a sort direction the existing `Direction`, a column type the
  existing `ColumnType`. The SQL AST is separate from `src/ast.rs`'s condition
  tree because a SQL condition may hold an aggregate operand, which the shared
  tree cannot represent — but the *plan's* condition is the shared
  `Condition`, so Module 4 can parse it back with `crate::parse_condition`.
- **`Direction` gains `Serialize`.** The sort key `[column, "asc"|"desc"]` is the
  plan's JSON, so `Direction` needed serde. One derive, no wrapper type.

## 2026-10-18, session 2 — the lexical corner cases

- **Qualified names are two tokens.** The scanner emits `Ident("e") Dot
  Ident("name")`; the parser assembles `e.name`. The alternative — one `.`-aware
  identifier token, as Bonus Project 1 used — would put two-token lookahead in
  the scanner and blur §1's rule that an identifier is "letter or underscore,
  then letters, digits, underscores". Keeping them apart matches §2's
  `column ::= ident | ident "." ident` directly.
- **`!=` as a synonym for `<>`.** §2 lists only `<>`, but rejecting `!=` seemed
  gratuitous; both produce the same token. The plan always renders not-equal as
  `<>`, the canonical spelling in the M2 and M4 specifications.
- **Negative literals.** `-30` is one token, as §1 says. The parser's
  `LIMIT`/`VARCHAR(n)` positions explicitly reject a negative token, so `LIMIT
  -1` is a syntax error rather than a `u64` wrap.

## 2026-10-18, session 3 — bags, the section the spec reads first

- **Duplicates are eliminated in exactly four places**: `DISTINCT` (a `distinct`
  node above the projection), `UNION` (`union`), `INTERSECT` and `EXCEPT`. Every
  other operator keeps duplicates, and `project` in particular never adds a
  `distinct` just because it dropped columns. This is the opposite of Bonus
  Project 1, where projection deduplicated, and it is the whole point of §4.
- **`GROUP BY` gets no `distinct`.** Grouping already returns one row per key, so
  `SELECT DISTINCT did, COUNT(*) ... GROUP BY did` emits only `project` over
  `group_by`. We chose not to also reject the written `DISTINCT`; it is
  harmless, and silently omitting the redundant node is what §4 says to do.
- **The one-place rule.** `plain_select` adds `distinct` iff the SQL wrote it;
  `aggregating_select` never does; `query` picks the set node. There is no other
  `distinct` in the compiler, so the bag behaviour is easy
  to audit.

## 2026-10-19, session 1 — resolution and translation

- **One pass, scope and plan together.** `build_from` returns a plan plus the
  scope it contributes; the compiler's `select` grows nothing else. Keeping the scope
  next to the plan avoids a second tree walk and makes "the scope entry's columns
  are the plan's output columns" literally true (they are the same `Column`
  values).
- **Aggregates in the AST, rejected in the resolver.** `operand ::= column |
  aggregate | ...` lets `HAVING COUNT(*) > 5` parse; the resolver rejects an
  aggregate in `WHERE`. Rejecting it in the parser instead would report a
  `syntax` error, but §3.1 wants an `aggregate` error.
- **`HAVING` may introduce an aggregate.** If the select list does not mention
  `SUM(salary)` but `HAVING` does, the aggregate is still added to the
  `group_by`, and the condition names its output (`sum_Emp_salary > 100`).
  Aggregates are keyed by `(function, argument)` so one computed twice appears
  once.
- **`JOIN` conditions resolve against the joined scope.** A join condition may
  use either side, so the scope is extended before the condition is resolved, not
  after. This is what lets `ON e.did = d.did` see both renames.

## 2026-10-19, session 2 — `ORDER BY` off the select list

- **The hard one.** §3 allows `ORDER BY` on a column that is not in the select
  list, but §5's canonical order puts `sort` above `project`. A sort above a
  projection cannot see a column the projection dropped. Two shapes were
  possible: (a) sort on the fully qualified input column, leaving the projection
  unchanged; or (b) keep the key in the projection, sort, then add a second
  projection to drop it again.
- **Chose (a).** It is the literal reading of §5's order and it keeps the plan's
  output exactly the columns the SQL asked for. Shape (b) adds a `project` node
  that is not in §5's canonical order and would make the plan's output schema
  differ from the select list before the trailing projection. Making (a)
  executable is a Module 4 rewrite (project the key, sort, project it away) —
  exactly the kind of decision §5 says belongs to Module 4. Documented in
  `DESIGN_M3.md` §3.3.

## 2026-10-19, session 3 — `CREATE`, `INSERT`, and the CLI

- **`CREATE TABLE` and `INSERT INTO` have no operator in §5.** The translator
  carries them through as `Translated::Create` / `Translated::Insert` and the CLI
  emits an ad-hoc `{"op":"create_table",...}` / `{"op":"insert",...}`. `check`
  accepts them and exits 0. They are in the §2 grammar, so they must parse and
  check; they have no relational meaning, so they cannot become a plan.
- **`parse` prints the token stream without parsing.** The subcommand originally
  parsed before deciding whether to explain, which meant a syntactically invalid
  query never showed its tokens. The token stream is the scanner's output alone,
  so `parse` (no `--explain`) now only scans. `--explain` still parses and prints
  the tree.

## 2026-10-19, session 4 — tests, docs, clippy

- **Tests for all fifteen cases.** `tests/sql.rs` builds a catalog with four
  tables that deliberately share column names (`did` in all four, `name` in
  two), then drives `radb-sql` end to end. Plans are compared structurally with
  `serde_json::Value`, so key order is irrelevant; errors are compared by
  category prefix and the names they mention.
- **Clippy found three things**, all fixed rather than silenced:
  - `FromItem` was a 320-byte enum because a `Cond` is large; boxing the join's
    `on` field brought the variant down. The condition is the biggest thing a
    `FROM` entry carries, so it is the right thing to box.
  - The table-reference alias parser had two identical `Some(parse_ident(...))`
    branches; folded them into one condition (`AS` or a leading identifier).
  - `compile` took `&PathBuf`; changed to `&Path`.
- **A stale doc comment.** `parse_program` claimed at least one statement was
  required; it actually accepts an empty program. Fixed the comment rather than
  the behaviour, since an empty file is a valid (if useless) input.

## 2026-10-19, session 5 — the condition language moves under `src/sql/`

- **Why.** The old Bonus Project 1 files `src/tokenizer.rs` and `src/parser.rs`
  could not simply be deleted: Modules 2, 3 and 4 all turn a plan's condition
  string into a tree at runtime (`crate::parse_condition`), and they carry their
  positions in `crate::tokenizer::Position`. Rather than leave a half-used
  "old" layer at the crate root, the whole condition language — the lexer
  (`tokenizer.rs`), the condition tree (`ast.rs`), the parser (`parser.rs`) and
  the compiled `FilterCondition`/`JoinCondition` (`compiled.rs`) — moved to
  `src/sql/ast.rs` and `src/sql/parser.rs`, where the front end that emits
  condition strings owns it.
- **Two lexers, deliberately.** SQL's scanner keeps `e.name` as two tokens and
  reads `--` comments; the condition scanner makes `R.b` one token and reads
  `//`. They lex different languages, so nothing was merged — they now sit side
  by side as `sql::tokenizer` and `sql::condition::tokenizer`.
- **Crate-root re-exports kept.** `radb::{parse_condition, ParseError, Condition,
  Operand, CompareOp, FilterCondition, JoinCondition, Position, Keyword,
  TokenKind, ...}` all still resolve, so `src/exec/` and `tests/tokenizer.rs`
  only changed where they had spelled a path (`use crate::parser::parse_condition`
  → `use crate::parse_condition`, `use crate::ast::Condition` → `use
  crate::Condition`). Module 2 still does not depend on the front end's layout.
