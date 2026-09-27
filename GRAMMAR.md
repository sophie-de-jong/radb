# Grammar, precedence, and associativity

The document has the five parts the project brief asks for:

1. The grammar itself — §1
2. Precedence and associativity — §2
3. An ambiguity demonstration — §3
4. A parsing strategy justification — §4
5. Sources — §5

---

## 1  The grammar (EBNF)

### 1.1  Lexical rules

A scanner walks the character stream one character at a time, following
maximal munch: at every non-separator character it consumes the longest
token that can start there. On seeing `>` it looks one character ahead
before choosing `>` or `>=` (rows #3). A `-` immediately followed by a digit
starts a (negative) integer literal; a `-` anywhere else is a lexical error,
because this language has no binary `-` symbol — set difference is the
keyword `minus` (row #4).

```
letter ::= "A".."Z" | "a".."z"
digit  ::= "0".."9"

IDENT      ::= letter ( letter | digit )*    (* bare word                   *)
QUAL_IDENT ::= IDENT ( "." IDENT )?          (* dotted name, one token      *)
INT        ::= "-"? digit+                   (* "-30" is one Int token      *)
STRING     ::= "'" ( any_char | "''" )* "'"  (* '' is one literal quote     *)
COMMENT    ::= "//" any_char_except_newline  (* discarded by the scanner    *)
WS         ::= ( " " | tab | newline | cr )+ (* ignored between tokens      *)

KEYWORD  ::= "select" | "project" | "rename" | "join" | "union"
           | "intersect" | "minus" | "times" | "and" | "or" | "not"

PUNC     ::= "(" | ")" | "[" | "]" | "{" | "}" | ","
CMPOP    ::= "=" | "!=" | "<" | "<=" | ">" | ">="
```

Rules carried by the scanner, not the parser:

* A string literal is atomic: `)`, `,` and spaces inside it are ordinary
  characters, never tokens (rows #5, #6). A doubled quote `''` is an escaped
  quote and contributes one `'` to the value, and does not close the string
  (row #7). Reaching end-of-input inside an open string is
  `LexError::UnterminatedString` pointing at the opening quote (row #9).
  Bare (unquoted) strings are allowed only in relation-definition tuples,
  where they lex as `IDENT`, `QUAL_IDENT` or `KEYWORD`. §4.1 also requires
  a bare value to be quoted if it contains a comma, a space, a parenthesis
  or a quote character, and the scanner enforces that without a separate
  check: a bare value is exactly one `IDENT` / `QUAL_IDENT` / `KEYWORD`
  token, and each of those four characters ends one. The consequence is
  therefore grammatical rather than an error — in a tuple body
  `Hello World` is two bare values, and since a tuple ends where its last
  value is not followed by a comma, it is two tuples rather than one value
  containing a space. Quoting is the only way to get the space into the
  value.
* Keywords are produced as keyword tokens everywhere, unconditionally
  (see §Keywords as attribute names). A qualified name such as `Emp.DID`
  lexes as a single `QUAL_IDENT` whose text contains the dot (see §Qualified names).

### 1.2  The concrete grammar

```
(* ─────────────────────────── program ─────────────────────────── *)

Start        ::= ( RelationDef )* ( Query )?

(* ─────────────────────── relation definitions ────────────────── *)

RelationDef  ::= IDENT "(" AttrList ")" "=" "{" TupleList "}"

AttrList     ::= BareAttrName ( "," BareAttrName )+
                                          (* at least one name             *)
                                          (* and never a dotted one        *)
BareAttrName ::= IDENT | KEYWORD          (* §Keywords as attribute names  *)
TupleList    ::= Tuple ( Tuple )*         (* whitespace is insignificant;  *)
                                          (*  // comments are ignored      *)
Tuple        ::= Value ( "," Value )*     (* one value per attribute; a    *)
                                          (*  mismatch is a load-time error *)
Value        ::= INT                      (* numeric value                *)
               | STRING                   (* quoted string value          *)
               | IDENT | QUAL_IDENT       (* bare string value, e.g. John *)
               | KEYWORD

(* ───────────────────────────── queries ────────────────────────── *)

Query        ::= Expr
Expr         ::= SetExpr

SetExpr      ::= JoinExpr ( "union"     JoinExpr )*
               | JoinExpr ( "intersect" JoinExpr )*
               | JoinExpr ( "minus"     JoinExpr )*

JoinExpr     ::= Unary ( ( "times" Unary )
               | ( "join" "[" Cond "]" Unary ) )*

Unary        ::= "select"  "[" Cond     "]" "(" Expr ")"
               | "project" "[" ProjList "]" "(" Expr ")"
               | "rename"  "[" NewName  "]" "(" Expr ")"
               | Atom

ProjList     ::= AttrName ( "," AttrName )+   (* project[] is a parse error *)
NewName      ::= AttrName                     (* new relation name          *)
Atom         ::= IDENT | "(" Expr ")"

(* ─────────────────────────── conditions ───────────────────────── *)

Cond         ::= OrExpr
OrExpr       ::= AndExpr ( "or" AndExpr )*
AndExpr      ::= NotExpr ( "and" NotExpr )*
NotExpr      ::= "not" NotExpr
               | PrimaryCond
PrimaryCond  ::= "(" Cond ")"
               | Comparison
Comparison   ::= Operand CmpOp Operand
Operand      ::= INT | STRING | AttrName
AttrName     ::= IDENT | QUAL_IDENT | KEYWORD  (* §Keywords as attribute names *)
CmpOp        ::= "=" | "!=" | "<" | "<=" | ">" | ">="
```

The language generated by this grammar is: the set of programs consisting
of zero or more relation definitions followed by zero or one query, where a
relation definition has a non-empty attribute list and a non-empty brace
block of comma-separated value lists (whitespace is insignificant: a tuple
ends where its last value is not followed by a comma), and a query is a mix
of
the six core operators (select, project, rename, union, intersect, minus,
times, join) and conditions formed from `not`, `and`, `or` and comparisons
of a number, a string, or a (possibly relation-qualified) attribute name.

A dotted `QUAL_IDENT` in `Atom` position is accepted syntactically but rejected at
execution as an unknown relation — a dotted name only makes sense where an
attribute is expected. A dotted name in a relation *header* is rejected at
parse time, because it could never be referred to afterwards (§Qualified
names).

---

## 2  Precedence and associativity

### 2.1  Query operators

| Precedence | Operators                          | Associativity | Enforced by      |
|------------|------------------------------------|---------------|------------------|
| 1 (loosest)| `union`, `intersect`, `minus`     | left          | `SetExpr`        |
| 2          | `times`, `join[c]`                | left          | `JoinExpr`       |
| 3          | `select[c]`, `project[..]`, `rename[..]` | prefix, applies to its own parenthesised argument | `Unary` |
| 4 (tightest)| relation name, `( expr )`        | -             | `Atom`           |

Decisions:

* `A union B minus C` == `(A union B) minus C`  (row #10). The three set
  operators share one precedence level and are left-associative.
* `A minus B minus C` == `(A minus B) minus C` (row #11). `minus` is not
  associative as a set operation, so this matters: with
  `A = {1,2,3}`, `B = {2,3}`, `C = {3}`,
  `(A minus B) minus C = {1}` but `A minus (B minus C) = {1,3}`. The
  executable version of this example is `tests/2_grammar.rs::test_11b_*`.
* `times` and `join[c]` are tighter than the set operators:
  `A union B times C` == `A union (B times C)`, and `R times S times T`
  == `(R times S) times T`. `A join[c] B join[d] C`
  == `((A join[c] B) join[d] C)`.
* The unary operators bind tightest of all but are applied to a *full
  expression* in parentheses, so `select[c](A union B)` is legal.

### 2.2  Condition operators

| Precedence | Operators            | Associativity | Enforced by |
|------------|----------------------|---------------|-------------|
| 1 (loosest)| `or`                | left          | `OrExpr`    |
| 2          | `and`               | left          | `AndExpr`   |
| 3          | `not`               | prefix        | `NotExpr`   |
| 4 (tightest)| comparison, `( cond )` | comparison is exact: two operands and one operator | `PrimaryCond` |

Decisions:

* `not` binds tighter than `and`, which binds tighter than `or`
  (rows #12, #13), so
  `select[not (a=1 and b=2) or c>3](R)` parses as
  `select[(not (a=1 and b=2)) or (c>3)](R)` and
  `select[a=1 and b=2 or c=3](R)` as `select[(a=1 and b=2) or c=3](R)`.
* `and`/`or` are left-associative; `not` is prefix
  (`not not a=1` == `not (not (a=1))`).
* A comparison is non-associative: the grammar demands exactly `Operand
  CmpOp Operand`, so `a<b<c` is a syntax error, not `(a<b)<c`.

Every decision above is enforced inside the grammar rules, never inside
special cases in the parser code: the precedence levels map one-to-one onto
nonterminals (`OrExpr` → `AndExpr` → `NotExpr` → `PrimaryCond`; `SetExpr` →
`JoinExpr` → `Unary` → `Atom`), and left associativity is encoded by the
right-recursive `( op Operand )*` loops, which fold each new operand into
the already-parsed left expression.

---

## 3  An ambiguity demonstration

Take the deliberately naive grammar

```
Expr ::= Expr "union" Expr
       | Expr "minus" Expr
       | "(" Expr ")"
       | IDENT
```

It is ambiguous: the input `A union B minus C` has two parse trees.

Tree 1 - `(A union B) minus C`:

```
              minus
             /     \
         union       C
        /     \
       A       B
```

Tree 2 - `A union (B minus C)`:

```
          union
        /       \
       A       minus
              /     \
             B       C
```

The two trees genuinely mean different things. With three single-column
relations  `A = {1, 2}`, `B = {2, 3}`, `C = {2, 4}`:

```
Tree 1:  (A ∪ B) − C  =  {1,2,3} − {2,4}  =  {1, 3}
Tree 2:   A ∪ (B − C) =  {1,2}   ∪ {3}    =  {1, 2, 3}
```

Different inputs, different answers. The ambiguity must be removed by
stratifying the grammar into precedence levels and pinning associativity.
The stratified grammar replacement (§1.2) is:

```
Expr     ::= SetExpr
SetExpr  ::= JoinExpr ( SetOp JoinExpr )*
SetOp    ::= "union" | "intersect" | "minus"  (* one level, left-associative *)
```

`SetExpr` is a left fold: after parsing `A union B`, the parser sees `minus`
next and makes the accumulator `(A union B)` the left operand of the next
operator, which forces **Tree 1**, `(A union B) minus C`, and makes
`A minus B minus C` left-associative. Because `union` and `minus` share one
precedence level (rather than `union` being looser than `minus`), the tree
is fixed by associativity alone.

---

## 4  Parsing strategy

The parser is a hand-written recursive-descent parser, one function per
nonterminal in §1.2, with a single-token lookahead. There is no backtracking:
every nonterminal begins with a distinct first token, so the parser commits
to the production that token starts and never rewinds.

Why this strategy:

* The project brief forbids parser generators (ANTLR, Lark, yacc, ...), so a
  hand-built parser is required anyway; recursive descent is the simplest
  strategy that maps a stratified grammar straight onto function calls and
  onto the AST (`tests/2_grammar.rs` checks the parsed trees directly).
* It gives the best error messages with positions: the parser knows exactly
  which nonterminal expected which token when it hits an unexpected token or
  end-of-input (rows #16, #17).

**What left recursion does to recursive descent.** The naive rule
`Expr ::= Expr "union" Expr | ...` cannot be transcribed directly:
`parse_expr`'s first action would be a recursive call to `parse_expr` before
any token is consumed, so the parser recurses forever and overflows the
stack on any input whatsoever.

**Where it is avoided in this grammar.** The grammar in §1.2 never uses
left recursion. Every previously left-recursive rule was rewritten as a
right-recursive repetition using the EBNF star:

* `SetExpr ::= JoinExpr ( SetOp JoinExpr )*` starts with a `JoinExpr`,
  then loops on operators; the left-associative tree is built by folding
  each operator into the accumulated left operand.
* `JoinExpr ::= Unary ( ( "times" Unary ) | ( "join" "[" Cond "]" Unary ) )*`
  the same fold for `times` and `join`.

There is no `Expr` appearing as the first symbol of its own right-hand side
anywhere in §1.2, which is exactly the property that makes an LL-style
recursive-descent parser terminate.

---

## 5  Sources

* Crafting Interpreters — Robert Nystrom (free online), chapters on scanning
  and parsing: https://craftinginterpreters.com
* Wikipedia: *Extended Backus–Naur form*, *Recursive descent parser*,
  *Maximal munch*, *Operator-precedence parser*
* Relax (dbis-uibk.github.io/relax) — the behavioural target for what the
  operators and schemas are supposed to do, per the project brief, and the
  place I confirmed semantics like the self-join and projection dedup before
  writing the grammar

**Where AI assistance was wrong.** The AI was used heavily for this project and
most of the parser, the engine and the test suite came out of it, so the
failures worth recording are the ones that survived into working code. Both of
the parsing failures below are expanded in DESIGN_LOG.md's closing section,
which also lists the failures that aren't about parsing — the two performance
ones appear only there.

* It never matched `TokenKind::QualIdent` anywhere, only `TokenKind::Ident`, so
  the single-token decision in §1.1 was not actually implemented: a qualified
  name in a condition or projection failed to parse. This stayed hidden until
  the relation-definition parser was moved onto the same `Tokenizer`, and the
  refactor's own test run failed. The fix is that every rule that expects an
  `AttrName` accepts both token kinds, which is why `AttrName` and `Value`
  list `QUAL_IDENT` explicitly in §1.2.
* Its query parser handled a nested operand by calling the whole-input entry
  point, which enforces end of input, so any parenthesised operand failed:
  `project[b](R)` reported "unexpected `)` ... expected end of input" on input
  that §1.2 accepts. Found only because five grammar tests failed during an
  unrelated refactor and the transcript showed the offending code predated that
  refactor by two days.

---

##  Design decisions the spec forces you to make

These are the "decide and document" rows. The grammar above already encodes
them; this section states them so they can't drift.

### Keywords as attribute names  (row #8)

**Rule:** keywords are tokenized as keywords everywhere, unconditionally —
`select`, `project`, `rename`, `join`, `union`, `intersect`, `minus`,
`times`, `and`, `or`, `not` never become `TokenKind::Ident`, even when they
appear where an attribute name would make sense (row #8, `select[union=3](R)`).

Responsibility for accepting `union=3` as "the attribute named union equals
3" sits in the parser: the grammar rule `AttrName ::= IDENT | QUAL_IDENT |
KEYWORD` (implemented as `Parser::parse_attr_name`) accepts any keyword token
and reinterprets its spelling as the attribute name. This keeps the lexer
context-free (it never has to know "am I inside `[...]`?") at the cost of a
small amount of extra leniency in the `AttrName` rule. `AttrName` is reused
in condition operands, `project[]`/`rename[]` names and bare tuple values;
relation-definition attribute lists use the narrower `BareAttrName`, which
drops `QUAL_IDENT` (see §Qualified names) but keeps keywords.

The header's attribute list is where the brief's §4.1 wording ("attribute
names are identifiers") is deliberately widened, in exactly one direction:
accepting a keyword-spelled column makes row #8 executable end-to-end — you
can define `R(union) = {1}` and then run `select[union=3](R)` against a real
column instead of a parse that could never succeed. Nothing else is added. In
particular `QUAL_IDENT` is *removed* from the header, which is a narrowing
back towards §4.1 rather than away from it.

**Relation names are not loosened.** `RelationDef ::= IDENT "(" ...` (§1.2):
a relation cannot be named after a keyword. The §4.2 operators are bare words, so
allowing `union` as a relation name would collide with the operator's own
spelling — and even if the header accepted it, query atoms already accept
only `IDENT`, so a keyword-named relation could never be referenced. A
keyword in relation-name position is a parse error (expected a relation
name); a qualified name (`E.D`) is rejected the same way.

### Qualified names  (rows #18–#20)

`Emp.DID` lexes as a single `QUAL_IDENT` token whose text contains the dot,
never as `Ident("Emp")`, `Dot`, `Ident("DID")` (§1.1). Anywhere the grammar
accepts an `AttrName` — which is `IDENT | QUAL_IDENT | KEYWORD` (§1.2) — a
qualified name is therefore automatically legal too. A dotted name in `Atom`
position (a relation reference) is a semantic error ("unknown relation"),
never special-cased in the parser.

**A qualified name has exactly one period, and a name with two can never be
written.** `QUAL_IDENT ::= IDENT ( "." IDENT )?` allows one dot, so maximal
munch cannot lex `Q.D.Name`: it produces `QUAL_IDENT("Q.D")`, then `.`, then
`IDENT("Name")`, which no rule accepts. This is why a relation *header* takes
`BareAttrName ::= IDENT | KEYWORD` and not `AttrName` (§4.1 of the brief:
"Attribute names are identifiers").

Rejecting `Q(D.Name, Age)` is not just deference to the brief — the column
would be unusable. §4.3 has `times` and `join` prefix every attribute with
its relation name, so `D.Name` in relation `Q` becomes `Q.D.Name`, and that
is a name no query can mention. The column could be printed in a result
schema and then never selected on, projected, compared or joined. The parser
reports `ParseError::QualifiedAttributeName` at the name
(`tests/2_grammar.rs::test_18_*`).

One consequence is worth stating because it looks like a special case and is
not: a column whose name equals its relation's, `K(K)`, comes out of a `times`
or `join` as `K.K` like any other attribute. Leaving it bare would be the
only way to keep it one period, and then the only spelling available for it
in a condition is `K` — indistinguishable from the relation name. Qualifying
it is both what §4.3 says and what makes it addressable
(`tests/3_semantics.rs::test_26_*`).

### Duplicate projected attributes  (row #24)

**Rule:** `project[Name, Name](R)` is syntactically valid (the parser doesn't
check for duplicates) but is rejected at execution time with
`SemanticError::DuplicateProjectedAttribute`. Rationale: whether a name is a
duplicate can depend on how you resolve qualified vs. unqualified names
against the input schema, which is schema information the parser doesn't
have. This is what `tests/3_semantics.rs::test_24_*` checks.

### Empty projection  (row #17)

`project[](R)` fails to parse: `ProjList ::= AttrName ( "," AttrName )+`
requires at least one name, and the parser reports
`ParseError::EmptyProjectionList` at the `]`.

### Relation definitions and set semantics  (rows #1–#9, #5, #6)

Relation blocks (`IDENT ( attrs ) = { ... }`) hold comma-separated value
lists with no line rule: whitespace is insignificant, and a tuple ends
exactly where the grammar says it ends — when its last value is not
followed by a comma (`Tuple ::= Value ( "," Value )*`) — so the usual
one-tuple-per-line layout is just a convention, and a value list may span
lines freely. A value is a number, a quoted
string, or a bare word (which must be quoted if it contains a comma, a
space, a parenthesis or a quote character — §4.1 of the brief). Attribute
names in the header follow `BareAttrName ::= IDENT | KEYWORD`
(§Keywords as attribute names), so a column may be spelled like a keyword —
that is what
lets row #8's `select[union=3](R)` run against a real column; the relation
name itself is an identifier. A header name is never qualified — see
§Qualified names for why a dotted one would be unreachable. A relation
is a set: duplicate tuples in the input collapse to one, which is done by
the engine's own definition of tuple equality, and `project` removes
duplicates the same way (row #23).