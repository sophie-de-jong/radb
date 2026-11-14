# GRAMMAR_SQL.md — the SQL grammar of Module 3

This is the grammar `radb-sql` accepts, written as EBNF and read top to bottom by
a hand-written recursive-descent parser (`src/sql/parser.rs`) over the
hand-written scanner (`src/sql/tokenizer.rs`). There are no regular expressions
and no parser generator, as §1 requires.

## 1. Notation

- Terminals are quoted and case-insensitive where they are keywords:
  `"SELECT"` matches `SELECT`, `select` and `SeLeCt`.
- `{ x }` means zero or more, `[ x ]` zero or one, `( a | b )` a choice.
- `ident`, `int` and `string` are the scanner's identifier, integer and string
  literal tokens (§5 below).
- A `;` separates statements and is optional after the last one.

## 2. The grammar

```ebnf
program     ::= { ";" } [ statement { ";" [ statement ] } ]

statement   ::= create | insert | query

create      ::= "CREATE" "TABLE" ident "(" coldef { "," coldef } ")"
coldef      ::= ident ( "INT" | "VARCHAR" "(" int ")" )

insert      ::= "INSERT" "INTO" ident "VALUES" "(" literal { "," literal } ")"
literal     ::= int | string

query       ::= select_block { setop select_block }
setop       ::= "UNION" [ "ALL" ] | "INTERSECT" | "EXCEPT"

select_block ::= select | "(" query ")"

select      ::= "SELECT" [ "DISTINCT" ] select_list
                "FROM" from_list
                [ "WHERE" condition ]
                [ "GROUP" "BY" col_list [ "HAVING" condition ] ]
                [ "ORDER" "BY" sort_list ]
                [ "LIMIT" int ]

select_list ::= "*" | select_item { "," select_item }
select_item ::= ( column | aggregate ) [ "AS" ident ]
aggregate   ::= ( "COUNT" | "SUM" | "AVG" | "MIN" | "MAX" )
                "(" ( column | "*" ) ")"

from_list   ::= from_item { "," from_item }
from_item   ::= table_ref { "JOIN" table_ref "ON" condition }
table_ref   ::= ident [ [ "AS" ] ident ]
              | "(" query ")" [ "AS" ] ident

col_list    ::= column { "," column }
sort_list   ::= order_item { "," order_item }
order_item  ::= column [ "ASC" | "DESC" ]

condition   ::= disjunction
disjunction ::= conjunction { "OR" conjunction }
conjunction ::= negation { "AND" negation }
negation    ::= "NOT" negation | primary
primary     ::= "(" condition ")" | comparison
comparison  ::= operand cmp_op operand
operand     ::= column | aggregate | int | string
column      ::= ident [ "." ident ]
cmp_op      ::= "=" | "<>" | "!=" | "<" | "<=" | ">" | ">="
```

The grammar is purely syntactic: it builds the AST of `src/sql/ast.rs` and knows
nothing about a catalog, so every name here is kept exactly as written and every
meaning error is reported later (§6 of the spec, `src/sql/error.rs`).

## 3. Precedence and associativity

| Construct                     | Precedence (tightest first) | Associativity |
| ----------------------------- | --------------------------- | ------------- |
| `NOT`                         | 1                           | right (prefix) |
| `AND`                         | 2                           | left          |
| `OR`                          | 3                           | left          |
| `= <> != < <= > >=`           | tighter than all of the above | non-associative (one comparison) |
| `UNION [ALL] INTERSECT EXCEPT` | one level, loosest          | left          |

Consequences, all enforced by the shape of the descent and not by ad-hoc code:

- `a OR b AND c` is `a OR (b AND c)`; `NOT a AND b` is `(NOT a) AND b`;
  `NOT NOT a` is `NOT (NOT a)`.
- `A UNION B EXCEPT C` is `(A UNION B) EXCEPT C`.
- A comparison is not chainable: `a < b < c` is a syntax error.

The condition grammar is its own descent (`parse_or` → `parse_and` → `parse_not`
→ `parse_primary_cond` → `parse_comparison`), one method per level, in the style
of Bonus Project 1.

## 4. Where this grammar differs from the §2 sketch, and why

The sketch in §2 is close to this grammar. The deliberate differences are:

1. **A qualified name is two tokens.** §1's scanner and the §2 rule
   `column ::= ident | ident "." ident` are both satisfied, but the scanner
   keeps `e.name` as `ident Dot ident` and the parser assembles the qualified
   reference. This keeps the scanner free of the two-token lookahead that
   `e.name` would otherwise need, and matches how §2 writes the rule.

2. **An aggregate may be a condition operand.** §2 writes
   `operand ::= column | int | string`; this grammar adds `aggregate`, so
   `HAVING COUNT(*) > 5` parses. The AST can represent it, and
   `src/sql/compiler.rs` rejects an aggregate in `WHERE` with an `aggregate`
   error, exactly as §3.1 demands. Without this the parser could not report the
   error §3.1 asks for, because the input would fail earlier as a syntax error.

3. **`!=` is accepted as a spelling of `<>`.** §2 lists only `<>`, but `!=` is
   unambiguous and common, so the scanner produces the same token for both. The
   logical plan always renders the operator canonically as `<>`, the spelling
   the M2 and M4 specifications use.

4. **A subquery's alias does not need `AS`.** §2 writes `"(" query ")" AS ident`;
   this grammar accepts `"(" query ")" [ "AS" ] ident`, so `(...) AS t` and
   `(...) t` both parse. The alias is still mandatory (`src/sql/compiler.rs`'s
   `TableRef::Subquery` always carries one), which is the part §3 relies on.

5. **A base table's alias does not need `AS` either.** §2 writes
   `ident [ AS ident ]`; this grammar accepts `ident [ [ "AS" ] ident ]`, so
   `FROM Emp e` and `FROM Emp AS e` are both accepted. This is the usual SQL
   convention and costs nothing: an identifier directly after a table name can
   only be an alias.

6. **`HAVING` requires `GROUP`.** §2 nests it inside the `GROUP BY` clause, so
   `SELECT COUNT(*) FROM Emp HAVING COUNT(*) > 1` is a syntax error here. This
   is exactly §2's shape; it is called out because it is the one place a query
   with an aggregate but no grouping columns cannot express a `HAVING`.

7. **A `VARCHAR` length outside `0..=255` is a `schema` error, not a syntax
   error.** `coldef` accepts any integer there; the range check needs the
   catalog's type (`ColumnType::Varchar(u8)`) and so is reported as `schema`.

## 5. Lexical rules (§1)

The scanner is `src/sql/tokenizer.rs`.

| Rule        | Behaviour |
| ----------- | --------- |
| Keywords    | Case insensitive: lowercased before matching. `SELECT` = `select` = `SeLeCt`. |
| Identifiers | Case sensitive: a letter or `_`, then letters, digits, `_`. `Emp` and `emp` differ. |
| Strings     | Single quoted; `''` inside means one literal `'`. A string runs to the next single quote or the end of input (an unterminated one is a `lexical` error). |
| Numbers     | Integers only, optionally signed (`-30` is one token). A value outside `i32` is a `lexical` error. |
| Operators   | Maximal munch: `<=`, `>=`, `<>` and `!=` are one token each; `=` `<` `>` are one token. |
| Punctuation | `( ) , . * ;` |
| Comments    | `--` to the end of the line, discarded. |
| Whitespace  | Any mix of spaces, tabs, `\n`, `\r`; insignificant. |
| Statements  | Separated by `;`. A file may hold several. |

The scanner tracks a 1-based line and a **0-based** column directly, and every
token carries that `Position`, which is what the §6 error format prints and what
the Bonus Project 1 tokenizer printed.

The reserved words are the 33 that appear in §2's grammar: `CREATE TABLE INSERT
INTO VALUES SELECT DISTINCT FROM JOIN ON WHERE GROUP BY HAVING ORDER AS ASC DESC
LIMIT UNION ALL INTERSECT EXCEPT AND OR NOT COUNT SUM AVG MIN MAX INT VARCHAR`.
A word that matches no keyword is an identifier, so a table may be called `city`
or `value` — the comparison is case-insensitive, so a table may *not* be called
`select` or `Select`.
