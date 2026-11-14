# Report M3: SQL front end

## Summary

`radb-sql` turns SQL text into a logical plan. A hand-written scanner produces
tokens; a hand-written recursive-descent parser produces the AST of `CREATE
TABLE`, `INSERT INTO` and `SELECT` (with set operators, joins, grouping and
`ORDER BY`/`LIMIT`); a translator walks the `FROM` clause bottom-up, resolves
every name against the catalog, checks types and aggregation rules, and emits
§5's operators in the canonical order. Nothing physical ever appears in a plan,
and no predicate is moved.

All fifteen required cases of §7 pass, every worked example of §5 reproduces,
and the whole suite passes under `cargo test` and `cargo clippy --all-targets`.

## Verification against the §9 checklist

| §9 requirement | Where it holds |
| -------------- | -------------- |
| Every §5 example reproduces with the same shape | `tests/sql.rs`: `spec_example_5_1` … `spec_example_5_7`, and cases 1, 5, 8 |
| No plan contains a physical operator name | `src/sql/plan.rs` has only the fourteen logical operators; there is no other `op` string |
| No plan contains an unqualified column reference | every column comes from `Scope::resolve`, which returns a `key.column` name; `resolved_columns_are_fully_qualified` checks it |
| No predicate moved from where the SQL put it | `WHERE` is one `select` above `FROM`; `HAVING` one `select` above `group_by`; `JOIN ... ON` keeps its condition; comma `FROM` keeps its predicate above the `cross` |
| Plans load and run once Module 4 makes them physical | plan conditions use the shared typed `Condition` language, and the plan nodes are the logical operators Module 2 and Module 4 understand |

## The fifteen required cases

| # | Case | Test |
| - | ---- | ---- |
| 1 | self join, two renames over one table | `case_01_self_join_scans_the_table_once_under_two_names` |
| 2 | `SELECT *` over three tables sharing a column | `case_02_star_qualifies_every_shared_column` |
| 3 | unqualified column in two tables | `case_03_ambiguous_unqualified_column_names_both_candidates` |
| 4 | `UNION` vs `UNION ALL`, different plans | `case_04_union_and_union_all_differ_in_one_node` |
| 5 | `WHERE` and `HAVING` on opposite sides of the grouping | `case_05_where_and_having_straddle_the_grouping` |
| 6 | non-grouped column in a `GROUP BY` query | `case_06_non_grouped_column_is_an_aggregate_error` |
| 7 | aggregate in `WHERE` | `case_07_aggregate_in_where_is_an_aggregate_error` |
| 8 | `FROM` subquery with an alias, joined to a base table | `case_08_subquery_in_from_joined_to_a_base_table` |
| 9 | `ORDER BY` a column outside the select list | `case_09_order_by_a_column_not_in_the_select_list` |
| 10 | `WHERE age > '30'`, `age` is `INT` | `case_10_int_against_string_is_a_type_error` |
| 11 | unknown table and unknown column, different messages | `case_11_unknown_table_and_unknown_column_are_different_errors` |
| 12 | `FROM Emp, Emp` with no aliases | `case_12_duplicate_table_name_is_an_error` |
| 13 | `COUNT(*)` with no `GROUP BY` | `case_13_count_without_group_by_groups_over_nothing` |
| 14 | `UNION` of mismatched column types | `case_14_union_of_mismatched_types_is_a_schema_error` |
| 15 | four-table join chain, commas and `JOIN ON` | `case_15_four_table_join_chain_two_ways` |

## Design decisions worth repeating

- **Bag semantics.** Duplicate elimination happens in exactly four places:
  `DISTINCT` → `distinct`, `UNION` → `union`, `INTERSECT` → `intersect`,
  `EXCEPT` → `except`. A `project` never deduplicates, and a `GROUP BY` never
  gets a `distinct` above it. See `DESIGN_M3.md` §2.
- **Aggregate naming.** `<function>_<argument>` with `.`→`_` and `*`→`star`, or
  the `AS` alias: `count_star`, `avg_Emp_salary`.
- **`ORDER BY` off the select list.** Resolved to the fully qualified input
  column and left in the `sort` above the `project`; making it executable is a
  Module 4 rewrite. See `DESIGN_M3.md` §3.3.
- **`!=` is accepted, `<>` is canonical.** The scanner accepts both spellings;
  a plan always renders not-equal as `<>`, the spelling in the M2 and M4
  specifications.

## Tests

- `src/sql/*.rs` — unit tests next to the code: the scanner's lexical rules,
  the parser's precedence and set-operator folding, scope resolution and
  ambiguity, the plan's JSON shapes, aggregate naming, and condition rendering
  round-trips.
- `src/exec/operators/filter.rs` — the compiled condition evaluation shared by
  Module 2's filters and joins.
- `tests/sql.rs` — 35 integration tests driving the `radb-sql` binary: the
  fifteen cases, the §5 worked examples, bag semantics, name qualification, the
  error format, `parse --explain`, multiple statements and `CREATE`/`INSERT`.

## What is deliberately not here

- **No optimisations.** No predicate pushdown, no join reordering, no turning a
  `cross` plus a `select` into a join. §5 assigns all of that to Module 4.
- **No `CREATE`/`INSERT` execution.** They parse, check and translate to a JSON
  description but are not applied to the catalog; there is no relational
  operator for them in §5.
- **No multi-error recovery.** §6 says panic mode is enough: the first error is
  reported and the front end stops.
