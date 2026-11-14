# Design Log M2

## Wrong Turns / Lessons Learned

### 1. Real/REAL type representation
- **Issue:** The instructions initially suggested REAL for AVG output; later the professor clarified REAL is just INT scaled by 10^4 (the value times 10,000).
- **Wrong approach:** Initially modeled REAL as a distinct `Value`/column-type variant across the codebase, which forced a second column-type enum in Module 2 (`ColumnTy`) that duplicated Module 1's `ColumnType`, plus a second key-part variant in the sort/join comparisons.
- **Fix:** Removed the REAL variants and collapsed Module 2's `ColumnTy` back into Module 1's `ColumnType`, so storage and execution now share one column type. AVG is computed as `round(sum / count × 10⁴)`, one more scaled `INT`.
- **Lesson:** Follow the exact problem specification precisely; representation details matter, and a type that exists only to carry one derived value is usually not worth a new variant or a duplicated enum.

### 2. Streaming final merge in external sort
- **Issue:** Naive external merge writes the final merged run, then the sort operator reads it back to stream to parent. This adds extra I/O and inflates counters.
- **Fix:** Implemented `RunSource` trait and `MergeStream` that can stream from a set of runs without writing a final run. The sort returns a source/iterator over merged tuples directly in the final pass.
- **Lesson:** Reading the example counts carefully (pass 2 streams) is important - match the expected behavior.

### 3. Plan parsing and node paths

- **Issue:** The §4 spec asked a malformed plan to name the offending node (`root.child.left`). A derived `Deserialize` cannot report a path, and hand-writing a streaming deserializer with `serde_path_to_error` was a lot of machinery for a message.
- **Fix:** The professor removed the node-naming requirement, so `Plan` and its leaf types use `serde`'s plain `Deserialize` derive with `serde_json::from_str`; a failure is reported once with serde's own message. The two array shapes need no hand-written visitor either: a sort key is a tuple struct holding the direction enum, and an aggregate converts from its `(function, column)` pair.
- **Lesson:** Do not hand-write a deserializer for a requirement that is not actually needed; confirm the requirement before paying its cost.

### 4. Modularizing operators
- **Issue:** Original monolithic structure made it harder to manage. Split operators into `src/exec/ops/` (group, joins, scan, setops, unary) while keeping `sort.rs` at top level.
- **Fix:** Added `mod ops;` with re-exports and updated build/imports. Kept public API stable.
- **Lesson:** Good modularization improves maintainability without breaking the interface.

### 5. Budget and pinning
- **Issue:** Need to ensure no pages pinned and no temp files left after close, and never exceed B.
- **Fix:** Track budget allocation; in close(), unpin everything and delete temp files. The buffer pool/context must be cleaned up properly even on error paths (use open/close wrappers).
- **Lesson:** Resource cleanup must be robust - always clean up in close, and use RAII-like patterns where possible.

### 6. Duplicate key handling in SMJ
- **Issue:** Simple "advance both when equal" gives wrong results for duplicate keys (Cartesian product of matching runs is required).
- **Fix:** When keys are equal, mark position of inner, then for each outer in run iterate outer, rewind inner to mark, iterate inner in run and emit pairs. Advance both past their runs after processing.
- **Lesson:** The spec's worked example is critical - test against it.
