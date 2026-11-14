# Deliberate deviations and known limitations

Only the `*INSTRUCTIONS*.txt` files under `docs/` are the specification. This
file records the places where the implementation knowingly differs from (or
takes a liberty with) a specification, so the choice is visible rather than
implicit. It also records the small number of specification items that remain
unimplemented.

## Confirmed acceptable (explicit sign-off)

1. **Physical-plan rejection messages do not name the offending node.**
   M2 §4 asks that a non-conforming plan be "rejected with a message naming the
   offending node and the problem". `Plan`'s `Deserialize` reports serde's
   message (for example `invalid plan: missing field 'table'`) without the path
   to the node. This was confirmed to be acceptable as-is.

2. **`AVG` is an `INT` scaled by 10 000, not a real printed to four decimals.**
   M2 §2.6 says "AVG returns a real number printed to four decimal places". The
   value model (`Value`) has only `Int(i32)` and `Varchar(String)`, and the
   overview fixes the types as `INT` and `VARCHAR`; `AVG` is therefore computed
   as the mean times 10 000 and stored as an `INT` (see `src/value.rs` and
   `src/exec/operators/group_by.rs`). This was confirmed to be acceptable as-is.

## Extensions to a module contract

3. **The physical `seqscan` carries an optional `alias`.**
   The logical plan keeps `rename` for `FROM Emp e` (M3 §5.5), but the physical
   plan (M2 §2) names columns by the table. To execute a *self-join*, where both
   scans read the same table and must stay distinguishable, the physical plan
   gained one optional field:
   `{ "op": "seqscan", "table": "Emp", "alias": "e" }`.
   It defaults to absent (`#[serde(default)]`), so every plan the M2 spec shows
   is unchanged and still parses.

The alias field is the only extension to a JSON contract. It is required by the
combination of M3’s self-join case and M2’s physical plan: M2 has no `rename`
operator, so without a way to distinguish two scans of the same table a
self-join cannot be executed. Plans written to the M2 specification are
unaffected because the field is optional and absent by default.
