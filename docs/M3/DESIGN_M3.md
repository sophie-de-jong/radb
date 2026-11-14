# Design M3: the SQL front end

## 1. What the module is

`radb-sql` reads SQL text and writes a logical plan as JSON. It never names a
physical algorithm, and it never moves a predicate. The pipeline has the layers
the spec asks for, each its own file with its own tests:

```
text ──compiler (scanner → parser → check + build)──▶ compiled statement ──▶ JSON
                                              (scope + types + aggregates)   (plan or DDL)
```

| Layer | File | Responsibility |
| ----- | ---- | -------------- |
| scanner | `src/sql/tokenizer.rs` | characters → tokens, positions, `lexical` errors |
| AST | `src/sql/ast.rs` | one tree per statement; names as written; positioned nodes use `Located<T>` |
| parser | `src/sql/parser.rs` | recursive descent, `syntax` errors |
| scope | `src/sql/resolve.rs` | the `FROM` scope and the resolution rules |
| compiler | `src/sql/compiler.rs` | streaming parse, check, and plan construction, one statement at a time |
| plan | `src/sql/plan.rs` | the operator types and their JSON |
| errors | `src/sql/error.rs` | the one §6 error format |
| predicates | `src/sql/ast.rs` | the typed `Condition` tree every plan carries; its JSON string codec is in `src/sql/parser.rs` |

The parser is purely syntactic and internal. `Compiler` streams statements
through it, taking the catalog per compile call so the caller can execute a
statement before compiling the next one. Checking and plan construction happen
in the same pass: each check method resolves names and types and returns the
logical plan and output schema it builds, so the AST carries no semantic
annotations. `Compiler::compile` returns one `Statement` (`Create`, `Insert`, or
`Select(plan)`); `compile_select`, `compile_create`, and `compile_insert` check
the statement kind. The AST stays internal. `render_syntax` is the syntax-only
entry point for the `parse` command and performs no checking.

Plans carry typed `Condition` trees. The string form is only the JSON wire
representation: `Condition`'s `Serialize`/`Deserialize` use the same condition
descent as SQL, so a plan's JSON still writes `"condition": "R.a > 1"` while
every operator passes the typed tree. Modules 2 and 4 import the type and the
codec through the crate-root re-exports (`radb::Condition`, `radb::Position`, …)
and do not depend on the front end's layout.

## 2. Bag semantics: where duplicates are eliminated, and why

**This is the section the spec says it will read first, so it comes first.**

Relational algebra is defined on sets; SQL is defined on bags. Module 3 follows
SQL. The translator therefore emits a `distinct` node in exactly four places and
nowhere else:

| SQL | Plan | Why |
| --- | ---- | --- |
| `SELECT DISTINCT ...` | `distinct` directly above the `project` | the user asked for it |
| `UNION` | `union` (deduplicating) | §4: set semantics for plain `UNION` |
| `INTERSECT` | `intersect` | §4: set semantics |
| `EXCEPT` | `except` | §4: set semantics |

Every other operator keeps duplicates:

- **`project` never deduplicates.** `SELECT name, age` over a table with two
  equal `(name, age)` rows returns two rows. The translator never wraps a
  projection in a `distinct` because the projection dropped columns.
- **`UNION ALL` is `union_all` alone**, with no `distinct` above it.
- **`GROUP BY` is never followed by a `distinct`.** A group-by already returns
  one row per distinct key, so even `SELECT DISTINCT did, COUNT(*) ... GROUP BY
  did` emits only `project` over `group_by`; adding `distinct` would be
  redundant and is not emitted. (The plan builder does not reject the written
  `DISTINCT`; it simply does not add a node for it when the query is
  aggregating.)
- **`scan`, `select`, `join`, `cross`, `rename`, `sort`, `limit` are all
  bag-preserving.**

The code that decides this is small and in one place: the compiler's
non-aggregating select path adds the `distinct` node only for a non-aggregating
query, and its query path chooses the binary node from the `SetOp`.

## 3. Name resolution

### 3.1 The scope

The `FROM` clause builds a `Scope` (`src/sql/resolve.rs`): an ordered list of
entries, each a key plus that entry's columns, fully qualified. The key is the
alias if the reference has one, otherwise the table name. A `FROM` subquery is
checked on its own first; its output columns are re-qualified under its alias
and that becomes the entry's columns.

Two entries may not share a key: `Scope::push` returns a `name` error for a
duplicate, which is the `FROM Emp, Emp` rule of §3. `FROM Emp e, Emp m` is fine
because the keys differ.

The entry's columns are re-qualified with `requalify`: a scan's output is keyed
by the table name and a `rename` re-keys all of them, so `Emp.did` becomes
`e.did` under alias `e`, and a subquery's `n` becomes `t.n` under alias `t`.

### 3.2 The rules, in order

`Scope::resolve` implements §3 exactly:

1. **Duplicate key** — reported when the entry is added, not when a column is
   resolved: `name: duplicate table name or alias 'Emp'`.
2. **Qualified `t.c`** — find the entry keyed `t` (else `name: unknown table or
   alias 't'`), then the column `t.c` in it (else `name: table 't' has no column
   'c'`). The two failures say different things, which is case 11.
3. **Unqualified `c`** — collect every entry column whose bare name is `c`.
   Zero is `name: unknown column 'c'; exactly one resolves; two or more is
   `ambiguity: column 'c' is ambiguous: Emp.c, Dept.c`, naming the candidates in
   `FROM` order, which is case 3.
4. **`SELECT *`** — never resolved one name at a time. `Scope::all_columns`
   returns every column of every entry, in `FROM` order then schema order, each
   already qualified, so two entries sharing a bare name is not an error — the
   expansion distinguishes them. This is case 2.

After resolution **no unqualified name survives**: every column in the plan is
either a fully qualified `key.column` or one of the plan's own output names (a
select alias or an aggregate output; §5's own examples use those bare, as in
`"n > 5"`).

### 3.3 `ORDER BY` resolution

`ORDER BY` follows §3's special rule: a key is matched first against the select
list's output names, then against the `FROM` scope. A key that names an alias
sorts on that alias (`ORDER BY n` → `["n","desc"]`, §5.4); a key that names a
scope column sorts on the fully qualified input column (`ORDER BY age` →
`["Emp.age","asc"]`, case 9).

A key outside the select list is **not** added to the projection. The canonical
order puts `sort` above `project`, and §5 measures the plan's shape, so the
translator keeps the sort key referring to the (fully qualified) input column and
leaves the projection exactly as written. Making a sort on a non-projected
column executable is a Module 4 rewrite (project the key, sort, project it away
again), which is where §5 says optim-isations belong.

## 4. Aggregation

A query is **aggregating** if it has a `GROUP BY`, a `HAVING`, or an aggregate
anywhere in its select list (the compiler's `select` path computes this). The
aggregating and non-aggregating paths then differ in three ways.

- **Non-aggregating**: `project` (then `distinct` if written), `sort`, `limit`.
- **Aggregating**: `group_by`, then a `select` for `HAVING`, then `project`,
  then `sort`, `limit`.

The grouping rules of §3.1 are enforced while walking the select list and the
conditions:

- In an aggregating query a bare select-list column must be a grouping column,
  or the query fails with an `aggregate` error. This is case 6.
- `HAVING` may reference only grouping columns and aggregates. A bare column
  there is checked against the group keys the same way.
- An aggregate in `WHERE` is always an `aggregate` error, because `WHERE` runs
  before grouping — checked up front, before the `WHERE` `select` is emitted.
  This is case 7.
- A `HAVING` aggregate the select list did not mention is still added to the
  `group_by`, so the `HAVING` condition can name its output. The `group_by`
  computes the union of the select list's aggregates and `HAVING`'s, each once.

Aggregates are de-duplicated by `(function, argument)`: `HAVING COUNT(*) > 5`
with `COUNT(*)` already in the select list reuses that one output instead of
computing it twice.

## 5. Plan construction in the canonical order

The compiler emits §5's operators bottom to top:

1. **FROM** — the `FROM` pass folds the list left to right. A comma is a
   `cross`; `JOIN ... ON` is a `join` carrying its condition, and a chain of
   joins is left-deep. An alias becomes a `rename` over the `scan` (or over the
   subquery plan), which is what makes the self join of §5.5 two renames of one
   table.
2. **WHERE** — one `select` carrying the whole condition, directly above the
   `FROM` plan. Nothing is pushed down.
3. **GROUP BY** — a `group_by` with the resolved keys and the aggregate specs.
4. **HAVING** — one `select` above the `group_by`.
5. **SELECT** — a `project`, then `distinct` if `DISTINCT` was written. A
   non-aggregate `AS` alias is an `[input, output]` project column; aggregate
   aliases already come from `group_by`.
6. **ORDER BY** — a `sort`.
7. **LIMIT** — a `limit`.
8. **Set operators** — applied between whole blocks, left to right, so
   `A UNION B EXCEPT C` is `except(union(A,B), C)`.

No predicate ever moves. The `WHERE` predicate stays above a join even when it
touches one side only (§5.2), and the comma form's predicate stays in the
`select` above the `cross` (§5.3). Recognising `cross` + `select` as a join is
Module 4's rewrite.

## 6. Types

Comparisons are checked by **kind**, not by declared length: a `VARCHAR(20)` and
a `VARCHAR(30)` compare, but an `INT` and any `VARCHAR` do not, which is the type
error of case 10 (`WHERE age > '30'`). The kind check covers `= <> < <= > >=`.

Aggregate result types (§3.2):

| Aggregate | Argument | Result |
| --------- | -------- | ------ |
| `COUNT` | anything, including `*` | `INT` |
| `SUM` | `INT` | `INT` |
| `AVG` | `INT` | `INT` (Module 2's scaled-by-10⁴ value) |
| `MIN` / `MAX` | either type | the argument's type |

`SUM`/`AVG` over a `VARCHAR` is a type error; `MIN`/`MAX` over `*` is a type
error (`*` is only legal in `COUNT(*)`).

Set operators require the same column **count** and the same `ColumnType` in
each position, compared exactly as Module 2's `Schema::same_shape` does, which is
the schema error of case 14. The error is positioned at the right-hand block's
`SELECT`.

## 7. Aggregate output names

An aggregate's output name is:

1. its `AS` alias, if it has one (`COUNT(*) AS n` → `n`); otherwise
2. `<function>_<argument>`, where the function is the lower-case tag and the
   argument is the resolved column with `.` replaced by `_`, and `*` spelled
   `star`:
   - `COUNT(*)` → `count_star`
   - `AVG(salary)` → `avg_Emp_salary`
   - `SUM(age)` → `sum_Emp_age`
   - `MIN(name)` → `min_Emp_name`

The rule is deterministic and collision-free enough for a plan; §5.4 itself
suggests `count_star` and `avg_Emp_salary`. `HAVING` and `ORDER BY` name an
aggregate by this output name, which is why the name is chosen at the moment the
aggregate is first added.

## 8. Errors

Every failure is the one §6 line, produced by `SqlError`:

```
error: <category>: <message> at line L, column C
```

The seven categories are produced by the stage that can know them: `lexical`
(scanner), `syntax` (parser), `name` and `ambiguity` (scope), `type` (type
checker), `aggregate` (aggregation checker), `schema` (shape checks). The column
is 0-based, matching the shared tokenizer. The translator reports the first error
and stops; there is no recovery.

## 9. Command-line interface

```
radb-sql parse   --query "SELECT ..." --explain   # indented AST, no catalog
radb-sql parse   --query "SELECT ..."             # token stream
radb-sql compile --db DIR --file q.sql --json     # logical-plan JSON
radb-sql compile --db DIR --file q.sql            # indented plan tree
radb-sql check   --db DIR --file q.sql            # errors only, exit 0/1
```

`compile` prints one JSON document per statement. `CREATE TABLE` and `INSERT
INTO` have no relational operator in §5, so they are carried through as
`{"op":"create_table",...}` and `{"op":"insert",...}` for the caller to apply;
`check` accepts them without error. The library performs no printing: the plan
JSON and the indented tree are built by `src/sql/report.rs`, and the binary only
prints them.
