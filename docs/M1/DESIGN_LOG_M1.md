# Design log — Module 1 (storage engine and buffer pool)

One short entry per working session, dated. Most sessions are pair-work with an
AI assistant. Each entry says what I was after, what I tried, and what broke.
The final section collects the occasions the AI gave me something wrong, and how
I found out.

This is a per-module log: Bonus Project 1 had a single `DESIGN_LOG.md`, which I
removed when Project 2 started, so that each module carries its own. The first
entry records the §1.3 week-one refactor, which is a prerequisite for Module 1
rather than part of it.

## 2026-10-01, session 1 — the §1.3 week-one refactor

Goal: split the Project 1 monolith into the layers the Project 2 contract names,
and do it before any Module 1 code so the storage engine sits on settled
foundations. `src/engine.rs` came out and `src/ast.rs`, `src/value.rs`,
`src/schema.rs`, `src/relation.rs` and `src/operators.rs` went in. The parser lost
its engine and kept only tokens plus a new public `parse_condition`, so a caller
can turn `Age>30` into a `Predicate` without a whole query. `Query` became
`serde` round-trippable — that JSON form is the contract between modules.

Two behaviour changes were deliberate, not refactor smoke. `Relation` stopped
being a set (`HashSet<Arc<[Value]>>`) and became a bag: a `Vec<Row>` in insertion
order, so the result of every query is deterministic, and removing duplicates is
the separate `distinct` operator. That is the §1.3 point and the reason
`tests/3_semantics.rs`'s case #23 changed from set semantics to bag semantics
plus `Distinct`. The other is the pull boundary: operators now consume a
`TupleSource` and ask it for one tuple at a time, so Module 2's heap scan can
drive the same code. Module 2 owns the real `open`/`next`/`close`; I built only
the boundary. Broke nothing quietly — the suite went 51 → 57 and I diffed a batch
of queries against the previous commit, normalising the old hash order, before
trusting it. GRAMMAR.md gained the bag-semantics note.

## 2026-10-02, session 1 — M1 groundwork: disk, record, page

Read `M1_INSTRUCTIONS.txt` twice before writing anything, because it says the
numbers are graded by comparison. Wrote `error.rs` first so every failure has a
home, then `types.rs` (INT/VARCHAR and the `--schema` parser, with `n <= 255`
enforced there), `record.rs` (§2, no header) and `page.rs` (§3). I made the
§3.2 worked example the page test from the start: empty, insert 13, insert 20,
insert 13, delete slot 1, and assert `slot_count = 3`, `free_end = 4050`,
`free = 4030`. Matching that page exactly before anything sits on top of it was
the single most useful thing I did this week. Wrote `disk.rs` next — a `Disk`
trait with `FileDisk` and `NullDisk` — so `trace` can run on a synthetic device
with no side-effect files.

## 2026-10-05, session 1 — the buffer pool and the two policies

`buffer.rs`: fixed frames, a `(file_id, page_id)` page table, pin counts, dirty
bits, and `fetch`/`new_page`/`unpin`/`flush_all`. Keying by `(file, page)` rather
than page id alone is the choice that makes the catalog and table files share one
pool without aliasing. Counting is exactly §5: a hit is a resident fetch, a miss
is a read, an eviction is a miss that replaced a resident page, and the first `B`
misses on an empty pool are not evictions. I wrote the §5.3 trace (`B=3`,
`1 2 3 1 4 2`) as the acceptance test and confirmed LRU = 1 hit / 5 miss / 2
evict and CLOCK = 2 / 4 / 1. CLOCK needed the careful part: the hand advances
past the frame it just loaded and the fresh page gets its reference bit set, and
I traced all six accesses by hand before trusting the code.

## 2026-10-06, session 1 — heap files and the catalog

`heap.rs`: page 0 is a directory page, directory pages chain, each entry is
`(data_page_id, free_bytes)`, capacity 681, insert is first-fit over the chain.
`scan` pins the directory page being walked plus one data page, materialises
nothing, and unpins on early stop via the scan's `Drop`. `catalog.rs` stores the
catalog in pages read through the same buffer pool, with an 18-byte page header,
a linked chain for growth, and a length-prefixed blob whose last field is a
reserved `stats` byte string — the seam Module 4's statistics will widen without
touching the page format. Format decided here and written down in `DESIGN_M1.md`,
because §6 leaves it to me and it must not leak into the graded scan counters.

## 2026-10-07, session 1 — CSV loader, CLI, first end-to-end run

`csv.rs` splits a line with a two-state scanner (no regex, per the project
rules). The loader turns fields into `Value`s, checks INT range and VARCHAR
length, names the line and column on failure, and checks the 4088-byte limit
before inserting so the error can name the table and the row. `cli.rs` holds all
subcommand logic; `src/bin/storage.rs` is a two-line wrapper. I aliased
`radb-storage` too, for the crate's naming convention. Then the first real run
panicked in `page.rs` and started the debugging below. After that it produced the
required scan JSON, and `layout` reproduced the §3.2 numbers on a real file.

## 2026-10-08, session 1 — chaining, error paths, tests, clippy

Wrote the integration suite in `tests/storage.rs`. While writing the
directory-chaining test I re-read `HeapFile::insert` and found the entry-dropping
bug below, before the test ever ran. Added the chaining test (690 one-record
pages, which must produce a second directory page), the pinned-eviction error,
the delete-everything-then-scan test, a scan of a table > 20·B that asserts at
most two pins, catalog round-trip, and the CLI error paths. Cleaned two clippy
warnings and got to a warning-free `cargo build`. 92 tests.

## 2026-10-09, session 1 — measurements, report, design doc

Built the measurement tables for §8: capacity for three schemas (255 / 170 / 19
records per page, all matching `floor(4088/(L+4))`), the 5-trace × 4-`B` ×
2-policy hit-rate matrix, and the 500-page scan at `B ∈ {2, 8, 64}` (exactly 500
reads each). Wrote `REPORT_M1.md` and `DESIGN_M1.md`, including the byte-level
dump of a real page from a database this tool created and the decision not to
implement compaction.

## 2026-10-09, session 2 — CLI out of the library

Review caught a boundary mistake: the `radb-storage` argument parser had been
left in the library (`src/storage/cli.rs`, a `clap` struct plus dispatch), with
`src/bin/storage.rs` and `src/bin/radb_storage.rs` as two-line wrappers. That is
backwards from how `radb` and `radb-study` are built — the binaries own their
clap definitions and the library takes no CLI dependency. Split it properly: the
operations are now `src/storage/commands.rs`, plain `Result<_, StorageError>`
functions with no clap, and `src/bin/radb_storage.rs` holds the clap
`Args`/`Command`, the `AppError`, every `println!`, and the `--frames` check
(`check_frames` moved to the binary). Deleted `src/storage/cli.rs`,
`src/bin/storage.rs` and the `storage` bin target, leaving `radb-storage` as the
only storage binary; the integration tests now run `CARGO_BIN_EXE_radb-storage`.
Same output and counters — the 15 storage tests still pass — but the library no
longer mentions clap and `tests`/M2+ can call the operations directly.

## 2026-10-10, session 3 — the CLI leaves the library entirely

A second review made the sharper point: moving the clap struct out was not
enough, because `src/storage/commands.rs` still held CLI-shaped code. Its `scan`
built a JSON value, its `layout` returned a **formatted string**, and each
function opened a pool and printed-style results. A library function whose return
value is presentation text is the tell — the boundary is still in the wrong
place. So `commands.rs` and `layout.rs` came out. The library now exposes only
primitives (`BufferPool`, `HeapFile`, `Catalog`, `TableSchema`,
`encode_row`/`decode_row`, the page accessors), and every subcommand — argument
parsing, opening and resetting the pool, the catalog handling, text rendering and
the `--json` shapes — lives in `src/bin/radb_storage.rs`, the same way `radb` and
`radb-study` own their front ends. The `layout` dump is now a small struct the
binary formats. Output and counters are unchanged; 92 tests pass and clippy is
clean.

## 2026-10-10, session 4 — the CSV reader leaves the library, and borrows

Review pointed at one last leak: `src/storage/csv.rs`. Splitting a CSV line is
part of the `load` front end, not a storage primitive, so it moved into
`src/bin/radb_storage.rs` (with its `CsvError`, and `StorageError::BadCsv`
deleted — a CSV problem is the loader's to report, not the engine's). While
moving it I rewrote it to be zero-copy: `split_fields` now returns
`Vec<Cow<'_, str>>`, so a plain field or a simply-quoted field is a borrow of the
line and only a field with a doubled quote `""` is folded into an owned `String`.
`fields_to_values` takes the fields by value and moves an owned `String` straight
into its `Value`, so the common path allocates nothing per field. A byte scanner
replaces the old `chars()` loop; quotes and commas are ASCII, so slicing at them
is always on a character boundary. Three unit tests moved with the code into the
binary (borrowing plus the escape case), so `cargo test` is now 93.

## 2026-10-10, session 5 — `CsvReader`, and the documentation layout

Polish on the loader front end. `split_fields` became a `CsvReader`
constructed with `new` and implementing `Iterator<Item = Result<Cow<'a, str>,
CsvError>>`: each `next()` parses exactly one field lazily and the separate
`push_field` helper went away, with the field loop inlined into `next`. The
`Result` item is the error channel a lazy, mid-stream-failing iterator needs —
an unterminated quote surfaces as one `Err` item and the iteration stops, and
`collect::<Result<Vec<_>, _>>()` restores the `Result<Vec<Cow>, CsvError>`
shape the loader wants. The docs left the source tree for a top-level `docs/`
with one subfolder per module: `docs/M1/` holds `M1_INSTRUCTIONS.txt`,
`DESIGN_M1.md`, `REPORT_M1.md` and this log; the general documents
(`GRAMMAR.md`, `INSTRUCTIONS.txt`, `REPORT.md`) sit directly under `docs/`;
only `README.md` stays at the root; and the `traces/` directory returned to
the repository root.

## 2026-10-10, session 6 — one `main`, no command functions

Review of the CLI shape: the binary still split each subcommand into a
`*_cmd` function that *returned* a result and a second pass that printed it —
`scan_cmd` collected every row into a `Vec` before `print_scan` could render
anything, and `layout_cmd` built a `PageLayout` struct that `print_layout`
then formatted. That is library thinking; a CLI should print as it goes. So
`dispatch`, `create_table_cmd`, `load_cmd`, `scan_cmd`, `layout_cmd`,
`trace_cmd`, `ScanOutputCli` and `PageLayout` are all gone: `main` now matches
on the clap `Command` and each arm implements its command directly, printing
in the same pass (`scan` prints the header, then each row the moment
`HeapScan::next_row` hands it over, then the stats; `layout` dumps the page
straight from the raw page bytes). The `scan --json` output is streamed too
(the `"rows"` array is emitted one element at a time) and still byte-for-byte
deterministic. Same outputs and counters; 94 tests pass and clippy is clean.

## 2026-10-10, session 7 — back from the iterator, decode inside the scan

Two reversals after real use. First, the `CsvReader` iterator was restructured
back into a plain `split_fields(line) -> Result<Vec<Cow<'_, str>>, CsvError>`
function. The iterator's laziness never mattered to the loader, which consumes
the whole line anyway, and it forced the awkward `Result<Cow, CsvError>` item
type plus a `done` flag and a `pos` cursor. The eager function keeps everything
that mattered — zero-copy borrowing, owned folding for `""` escapes, the
two-state byte scanner, and the unterminated-quote error — in a straight loop;
the loader's call is one line again. (On reflection the "laziness via `pos`"
unit test was testing machinery, not behaviour, and is gone.) Second,
`HeapScan::next_row` now decodes as it goes: `scan` takes the schema and the
rows come back as `(rid, Vec<Value>)` instead of raw bytes. Every scan caller —
three integration tests and the binary's `scan` arm — decoded the row on the
spot anyway, and decoding inside `next_row` lets streamed readers like the CLI
print each row the moment it leaves the page. The task ordering inside
`next_row` is unchanged (read the record bytes, decode them, return), so the
pin discipline and the read counters are identical. Also fixed along the way:
`create-table` had regressed to flushing the pool *before* saving the catalog,
so a freshly created table vanished ("unknown table 'R'" on load), and the
`trace` command had lost its documented `--db` flag (§7 mandates the
invocation, even though the trace runs on a synthetic disk). Both are back;
95 tests pass and clippy is clean.

## 2026-10-10, session 8 — `json!` output, and a real `Page` type

Two readability passes on the storage front end, both requested by the user.
First, `scan --json` no longer hand-builds the JSON with `print!` calls (the
`"rows"` array framed element by element, the rest quoted in place). It now
collects the decoded rows while scanning and prints the whole §7 object from one
`serde_json::json!` value via `to_string_pretty`, with a small `row_to_json`
helper for the per-row array. Text output still streams each row the moment
`HeapScan::next_row` hands it over — the two modes are two branches of the same
arm. Same counters, same schema and stats fields; `cli_scan_is_deterministic`
still passes byte-for-byte, and the integration test parses the same keys.

Second, the page module centres on a real type. `PageBytes` was only
`type PageBytes = [u8; 4096]`, so every signature advertised a raw byte slab.
It is now `pub struct Page([u8; 4096])` (the user's suggestion: call it `Page`)
with `Deref`/`DerefMut`, `AsRef`/`AsMut`, `From<[u8; 4096]>`, `Page::zeroed()`
and `Default`. The `Disk` trait returns a `Page`, buffer frames hold `Box<Page>`,
and the `heap::dir_*` helpers take `&Page` like the `page::*` functions do — one
type through the whole stack, re-exported as `radb::storage::Page`. Two things
surfaced: the disk layer needs explicit `&mut data[..]` slices for
`read_exact`/`write_all`, because deref-to-array does not compose with the
unsizing coercion to `&[u8]`; and the newtype is transparent — a plain struct of
4096 bytes is already `Copy`, so evicting and flushing stay value copies for
free.

Also fixed a test that the working tree had drifted into red before this session:
`record::rejects_out_of_range_and_overlong_values` asserted `i32::MAX` fails to
encode, but a `Value::Int(i32)` is always in range by construction. The test now
asserts the extremes encode, and the real rejection cases (overlong VARCHAR,
wrong-kind value, truncated or trailing record bytes) still err. 95 tests,
clippy clean and rustfmt clean.

## 2026-10-10, session 9 — Rust 2024, and the Project-1 docs leave

Toolchain moved to Rust 1.99 and the crate to `edition = "2024"`. The
proc-macro errors the bump appeared to cause did not reproduce on a clean build:
proc-macro crates are compiled against their own editions and the only edition
2024 requirement is rustc ≥ 1.85, so clap's and serde's derives build unchanged
(all 95 tests and clippy are green). A stale `target/` from the previous
compiler is the usual culprit if it recurs. Two real consequences of the bump:
rustfmt's style edition follows the crate edition, so the whole tree was
reformatted (import order, and a few trailing-semicolon and `write!` cases), and
clippy's `collapsible_if` started firing where the older toolchain was quiet —
both spots (the semijoin budget in `operators.rs` and `flush_all` in
`buffer.rs`) collapsed into `let` chains, which edition 2024 stabilises. The same
pass deleted the Project-1 documents that no longer describe this project:
`docs/GRAMMAR.md`, `docs/REPORT.md`, and their `report_loglog.png` /
`plot_loglog.py` generators. README lost its three citations to the grammar doc
and now points at the §7 test suites instead; `traces/` and the Module 1 docs
stay.

## 2026-10-10, session 10 — the page API becomes methods

The user asked for every function that acts on a `Page` to be a method on it, and
to consider pulling the layout constants onto the type too. Both landed. The
slotted-page operations (`init`, `page_id`, `slot_count`, `free_end`,
`free_space`, `slot`, `is_live`, `insert`, `get`, `delete`) are now methods in
`page.rs`, the two byte helpers became `pub(crate)` methods so the directory
block could reuse them, and the constants are associated: `Page::SIZE`,
`Page::HEADER_SIZE`, `Page::SLOT_SIZE`, `Page::MAX_FREE`, `Page::MAX_RECORD`
(only `SIZE` is written down; the rest are expressions over it). The directory
format is still the heap file's, so its operations are a second `impl Page` block
in `heap.rs` rather than a `page.rs` that would have to know about data-page
chains. Call sites lost their module paths — `page::insert(pool.page_mut(f), r)`
is `pool.page_mut(f).insert(r)`, `page::free_space(p)` is `p.free_space()` — and
`storage::mod` no longer re-exports a bare `PAGE_SIZE`. Behaviour is untouched:
95 tests, clippy and rustfmt all clean, and the `storage::page::tests` worked
example still reproduces byte for byte.

## 2026-10-10, session 11 — a simplification pass, and parsing moves to `FromStr`

The user asked whether anything in the storage module was over-engineered, then
chose the full cleanup: dead code, a single-page catalog, no path aliasing, fewer
files. The audit found that the module is mostly *spec-shaped* — the page, heap,
pool, catalog and loader are all mandated — so the win was not in merging files
but in deleting what had accreted around them.

Deleted as unused: `StorageError::NoSpace` (never constructed; §9 has no such
error), `record::field_len` (a length-only helper nothing called),
`BufferPool::{frames, policy, pinned_count, is_resident, frame_key}`, an empty
`if policy == Clock {}` left in `install`, and the whole `Disk::path` /
`FileDisk::file_path` path accessor pair (plus the now-dead `path` field on both
back-ends). `TableSchema::column(i)` went the same way; its one caller was its own
test, which now reads `columns()[i]`.

Two things were built past the spec and were pulled back. The catalog paged its
blob across a `next_page` chain with an 18-byte header, but nothing in Module 1
can fill half a page of metadata, so it is now a single page with a 12-byte
header that errors if a blob would not fit; the version and a reserved `u16`
remain as the seam Module 4 widens. And `BufferPool::open_file` canonicalised
every path so two spellings shared one disk — machinery for a case the CLI never
has — so it simply opens and registers now.

`types.rs` folded into `record.rs`: one module for the type system and the codec
that walks it, `src/storage/` down from nine files to eight. As part of that, the
ad-hoc parsers became trait impls, on the user's instruction: `TableSchema::parse`
is now `impl FromStr for TableSchema` and the private `parse_type` is
`impl FromStr for ColumnType` (joining the existing `Policy`), so callers write
`schema.parse::<TableSchema>()`. The catalog's on-page type tags moved verbatim,
so no bytes changed. 96 tests, clippy and rustfmt clean, and the CLI smoke run —
create, load, `scan --json`, `layout`, and the LRU trace — still reproduces the
§3.2 and §5.3 numbers.

## 2026-10-11, session 12 — `Page` goes back to being just a buffer

The user reopened the design smell flagged at the end of session 10 — that one
type was serving as both a generic frame and a slotted page — and settled it in
the best way: `Page` should make *no* assumption about its bytes beyond what is
true of every page. It is now only a 4096-byte buffer plus little-endian integer
accessors at an explicit offset (`read_u8`/`read_u16`/`read_u32`,
`write_u8`/`write_u16`/`write_u32`) and the old `Deref`/`AsRef`/
`From`/`zeroed` surface. Session 10's associated constants (`HEADER_SIZE`,
`SLOT_SIZE`, `MAX_FREE`, `MAX_RECORD`) and the `init`/`insert`/`get`/`delete`
methods are gone from it.

Each layout now owns its offsets, as free functions over a raw `&Page`/
`&mut Page`, with no wrapper objects per format — which was the user's explicit
constraint, and the reason the earlier draft of this session stalled: it kept
trying to design `DataPage`/`DirPage`/`CatalogPage` view types and negotiate
their shared/unique-borrow story instead of the far simpler answer the user
gave. The slotted layout moved to a new `slotted.rs` (`HEADER_SIZE`, `SLOT_SIZE`,
`MAX_FREE`, `MAX_RECORD`; `init`, `page_id`, `slot_count`, `free_end`,
`free_space`, `slot`, `is_live`, `insert`, `get`, `delete`, and its three
worked-example tests, moved out of `page.rs`). The heap directory helpers became
`heap::dir_*` free functions, and the catalog page gained `write_catalog_page` /
`read_catalog_page` — both pure functions of a page, so the raw `[u8]` indexing
in `Catalog::save`/`read_blob` is gone. `storage::mod` adds `slotted` and
re-exports a slimmer `Page`; call sites read `slotted::insert(pool.page_mut(f), r)`
and `dir_count(pool.page(f))`. `page.rs` gains one test that the integer
accessors are little-endian on the wire. 97 tests, clippy and rustfmt clean, and
the CLI smoke run still reproduces the §3.2 layout numbers and the §5.3 LRU
(1 hit, 5 misses, 2 evictions) and CLOCK (2/4/1) counts.

## 2026-10-11, session 13 — clearer identity types, and `HeapScan` is a real iterator

The user read the heap code and asked for three things: call the record-id type
`RecordId` rather than `Rid`; make `HeapScan` implement `Iterator` as the spec's
`scan` describes; simplify the scan's state; and, since `FileId` and `RecordId`
are types, make `PageId` a type too.

`Rid = (u32, u16)`, `FileId = u32` and the bare `page: u32` parameters became
three newtypes, each defined next to the thing it names: `PageId(u32)` in
`page.rs`, `FileId(u32)` in `buffer.rs`, and `RecordId { page: PageId, slot: u16 }`
in `heap.rs` — each `Debug`/`Copy`/`Eq`/`Hash` and `Display`, with
`PageId: FromStr` for `trace`. (I first put them in a new `id.rs`; a review point
was right that a whole module for three tiny types is not worth it, and they moved
home.) The types thread through `Disk`
(`read_page(PageId)`), `BufferPool` (`fetch`/`unpin`/`new_page`),
`slotted::init`/`page_id`, the `dir_*` helpers, `HeapFile::{insert,get,delete}`
and the `StorageError` variants, so `unpin(file, page, ...)` can no longer be
called with the arguments the wrong way round. `NO_PAGE` became
`PageId(0xFFFF_FFFF)`.

`HeapScan` now implements `Iterator<Item = Result<(RecordId, Vec<Value>), StorageError>>`
and the CLI's two scan loops call `next()`; the integration tests iterate with
`by_ref()`. The state machine shrank from twelve fields to seven: it keeps only
its position — an `Option<Pinned>` for the current directory page and one for the
current data page, plus a `u16` cursor for each — and reads `dir_count`,
`dir_next` and `slotted::slot_count` back from the pinned page on demand instead
of caching them, which removes the `done` field and the second copy of every
count. Decoding moved into a private `row` helper.

While testing I found a genuine codec bug: `decode_row` read the VARCHAR body as
`VARCHAR_LEN_SIZE` (2) bytes instead of the length `L` it had just read, so any
string longer than two bytes failed ("column 'b' is not valid UTF-8" on the
`héllo` round-trip). One-line fix (`at + len`), and the round trip is green again.

Docs: `DESIGN_M1.md` notes the newtypes beside their host modules and the
iterator-shaped scan; the test count went to 99 (42 for Module 1).

## 2026-10-11, session 14 — the buffer pool stops handing out frames

The user read `buffer.rs` and asked what a *frame* was doing in the API: §5 says
`fetch_page(page_id) -> page`, so why did `fetch` return a frame, and why was the
concept of a frame in the signatures at all? We worked through it before editing.

The frame is real and required — §5 mandates a fixed array of `B` frames, each
holding one page's bytes plus a pin count and a dirty flag, with the page table
mapping `(file, page_id)` to a frame index, and LRU/CLOCK order and replace
frames. But it is *internal*. The spec's own pseudo-code is loose here: the
signature says `-> page` while the body says `return frame`, and we had copied the
body. The result was a public `FrameId` that leaked into `HeapFile`,
`HeapScan`, the catalog, the CLI and the tests, and that forced every caller to
carry two identities at once — a `(frame, page)` pair, which is why `HeapScan`
needed a `Pinned { frame, page }` struct at all. There was a correctness edge too:
a `FrameId` is a slot index, so `page(frame)` after an unpin could silently read
whatever page later occupied that slot.

The fix is the shape the spec already had: the pool is page-keyed end to end.
`fetch(file, page)` and `new_page(file)` return nothing new about identity
(`new_page` returns the new `PageId`), `page(file, page)` / `page_mut(file, page)`
resolve through the page table, and `unpin(file, page, dirty)` was already
page-keyed. `FrameId` is gone; frames are private. `HeapScan` lost `Pinned` and
now stores two `Option<PageId>`, and every call site in `heap.rs`, `catalog.rs`,
the binary and `tests/storage.rs` names `(file, page)`. Resolving through the
table also means a stale reference becomes a loud "page is not resident" panic
instead of silent corruption — the borrow-checker cost we paid for (you cannot
return a `&Page` borrowed from `&mut self` and then fetch a second page) is now
paid with a page id rather than a frame index. Behaviour and counters are
unchanged: 99 tests, clippy and rustfmt clean.

## 2026-10-11, session 15 — `fetch` returns the page it pinned

Session 14 made the pool page-keyed but left `fetch` returning `()`. That still
did not match §5's `fetch_page(page_id) -> page`: the operation that pins a page
said nothing about *which* page you now hold, and a reader of the signature could
not tell that a successful `fetch` gives you a page to use. The user proposed the
smallest shape that fixes it and keeps the borrow checks quiet: have both
`fetch` and `new_page` return a `PageId`, and let the caller *resolve* that id to
a borrow with `page`/`page_mut`.

That is the whole change. `fetch(file, page)` returns the pinned `PageId` (the
same one it was given — the value is a handle, not new information);
`new_page(file)` already returned the new `PageId`. `page`/`page_mut` are
unchanged: they take `(file, page)` and borrow the resident bytes. Because the
handle is a `Copy` value, holding two pages pinned — the directory page and a
data page, in `insert` and in the scan — is now trivial: the caller keeps two
`PageId`s and re-borrows either on demand, with no lifetime tying the borrows
together.

Call sites now read in the intended shape. `insert` binds the directory page and
the data page from `fetch` and resolves each with `page`/`page_mut` as it goes;
`get`, `delete`, the scan's two fetches and the catalog's `save`/`read_blob` do
the same; the scan stores the returned ids in its `Option<PageId>` fields. The
two `match … { Ok(()) => … }` arms in `HeapScan::next` became `Ok(pinned)`,
which is where the return value finally earns its keep: the arm no longer repeats
the page id it just passed in. Counters are untouched — the same pages are
fetched and unpinned, in the same order — so the LRU/CLOCK worked examples still
hold. 99 tests, clippy and rustfmt clean.

## 2026-10-11, session 16 — the insert walk, and what the spec's delete line means

The user's read of `heap.rs` was that `insert` and `HeapScan::next` looked far
heavier than the module's pseudocode. We separated what the spec forces from what
we had added. `next` is genuinely resumable-state complexity — an `Iterator`
returns once per row, so §4's nested `for` loops become a machine that resumes
from `(dir, dir_entry, data, data_slot)` — and was left alone. `insert` was
carrying four things the spec never asked for: an `inserted: Option<RecordId>`
that existed only so the function could return after the loop; a
`last: Option<(PageId, bool)>` with an `expect` to carry the tail of the chain
out of the loop; a re-fetch of the last directory page in the grow path; and a
defensive branch for a "stale" directory free count.

The rewrite deletes all four. A first-fit hit returns directly, unpinning the data
page and the directory page first. The grow path is handled where the walk reaches
the end of the chain, with that directory page still pinned, so there is no cursor
to carry out and no second fetch. The stale-free branch becomes an `expect`:
directory counts are written from the page's real free space on every insert and
never raised, so `free_bytes >= need` always implies the page fits — the branch
was unreachable. `insert` fell from ~92 to ~62 lines and now reads top-to-bottom
in the spec's order: walk, first fit, else allocate.

That change raised the delete question. §4 says `delete` should "update directory
free_bytes", and we did not. But under §3 free space is
`free_end − (8 + 4 × slot_count)`, and marking a slot dead changes neither
`free_end` nor `slot_count` — §3.2's worked example leaves free space at 4030
across the delete. The entry already holds the right value, so the update would
write it back unchanged: the line is a no-op, and making `delete` walk the
directory to rewrite the same number would add a search for no observable effect.
`delete` stays as it is, and the doc now says why instead of claiming an update we
never did. 99 tests, clippy and rustfmt clean.

## Where the AI was wrong

**It forgot to initialise a freshly allocated page (10-07).** `buffer.new_page`
returns a zeroed page, and a zeroed slotted page has `free_end = 0`, so
`free_space` underflowed and the very first `load` panicked with "attempt to
subtract with overflow" at `page.rs:71`. The AI had written `insert` assuming
`new_page` produced a usable page; it does not — a *fetch* past the end is
zeroed, and so is an allocation. Found by running the end-to-end CLI instead of
trusting the unit tests, which had only ever inserted into pages `page::init` had
already touched. Fixed by calling `page::init` on the new data page. This is the
one bug that would have zeroed the module if I had shipped the unit tests alone.

**`reset` did not reset the counters, so scan stats counted the catalog
(10-07).** The AI added `BufferPool::reset` for `scan` "to drop the catalog
pages", and the doc comment even claimed it left the counters alone — but the
intent was the opposite: the reported scan numbers must cover only the table.
The first scan printed `misses = 3, reads = 3` for a table with exactly two pages
(the directory page and one data page), and the extra count was the catalog read
that survived the reset. Found by hand-computing what the two-page scan should
cost and comparing. Fixed by adding `reset_stats` and calling it after the
catalog is read, which is also what makes the counters portable at all: the
catalog's page format is implementation-defined, so counting it would make two
correct implementations disagree.

**Directory chaining dropped the very entry it was creating (10-08).** In
`heap.insert`, when the last directory page is full the code allocates a new
directory page and links it, but the AI's version never wrote the new data page's
`(page_id, free_bytes)` entry into that new page. Up to 681 data pages
everything looked fine; the 682nd page's records became invisible to `scan` (and
its free space unusable). Found by re-reading `insert` while writing the chaining
test — the test then confirmed the fix (690 rows → 692 pages, all 690 scanned).
The lesson: first-fit directories have exactly one non-obvious branch, and it is
the one that only fires past 681 pages, so it needs a test that crosses that
line.

**Two of the AI's own tests asserted the wrong expected value (10-07/10-08).**
The CSV test claimed `""",plain` should split to `[",plain"]`; that input is
simply an unterminated quote and the splitter correctly errors, so the test was
wrong and the code was right. The capacity test asserted a page full of 255
twelve-byte records has `free_space == 0`, but `255 * 16 = 4080` leaves 8 of the
4088 bytes unused — that remainder is the whole point of the `floor`, and the
test was asserting the formula was exact. Found by reading the failure messages
and re-deriving the arithmetic by hand, not by changing the code to match the
test.

**The library grew a CLI (10-09/10-10).** The AI put the subcommand logic in the
library and had `layout` return a ready-to-print string and `scan` assemble the
JSON. Both compile and both are wrong: a general module cannot know how a caller
wants to present a page, and a `Result<_, StorageError>` API that hands back
formatted text can only ever serve one front end. Review caught it and the AI had
to be told twice — first to drop the clap struct, then to drop the command
functions too. Found by asking what a *library* caller (Module 2) would do with a
layout string; the answer is nothing, which is the definition of a leaky
boundary. It took a third reminder for `csv.rs`: a line splitter is not a storage
primitive either, so it moved to the binary (and was rewritten to borrow). The
pattern is the same each time — the AI put code where it was convenient to write
it, not where the caller needs it.

**The AI dressed a two-line loop up as an iterator (10-10).** Session 5 turned a
plain `split_fields` into a `CsvReader` implementing `Iterator` with
`Item = Result<Cow<str>, CsvError>`, on the theory that lazy field parsing was
cleaner. It was machinery looking for a problem: the loader consumes the whole
line on every row, so "lazy" bought nothing, while the stop-mid-stream error
channel forced every caller through a `collect` and the iterator itself through
a `done` flag and a `pos` cursor. The user asked to go back to the function in
session 7. The tell it should have been caught at: the only test of the
"laziness" was inspecting `pos` after a `next()` call — a test of that iterator's
internals, which means the laziness was an implementation detail nobody could
observe. A good abstraction earns its interface from what a caller does with it;
here the caller wanted a whole-line `Vec<Cow>` all along.

**"Always stream, never gather" was an over-rule (10-10).** Session 6's review
killed the collect-then-print CLI, and DESIGN_M1 still claims a scan "prints
each row the moment it reads it — there is no compute-then-render pass". That is
the right shape for text but the wrong frame for structured output: a JSON
object has to exist as a value before it can be serialised, and the hand-rolled
`print!("{{\n  \"table\": ...")` framing that streaming forced was visibly worse
to read than one `json!`. The user asked for the macro; the scan now collects
its rows and emits a single object, and the streaming rig shrank to a byte-driven
loop and a `json!`. Same lesson as the iterator: pick the shape the caller of
the output actually needs, not the shape that fits one streaming pass.

**A type alias is not a type (10-10).** `PageBytes = [u8; 4096]` let the page,
heap and disk modules share "a byte slab", which meant `write_page` could not
say it wanted a page rather than any 4096-byte array, and the alias could not
grow a constructor or an invariant. The user's request for a wrapper (maybe
called `Page`?) turned it into a real type the module now centres on: one
struct, one name, `zeroed()`, deref for the byte dances. The AI had called the
alias good enough; the disk `read_exact` call taught it in the first build
failure that a slab and a page are different claims.

**The AI misread "one type" as "one type that knows every layout" (10-10,
reversed 10-11).** Session 10 moved the slotted operations onto `Page`, and the
AI presented that as the clean end state: one type through the whole stack. It
is not — the slotted page, the directory page and the catalog page are three
different formats, and hanging the first one's offsets on the shared buffer made
`Page` mean "page, unless it is a directory page, unless it is the catalog page".
The tell was already visible in that same session: the directory operations had
to go into a second `impl Page` block in `heap.rs` to keep them away from the
slotted ones, which is a format living on the wrong type. When the user asked for
format-specific handling, the AI then spent the whole session trying to design
borrowing wrapper objects (`DataPage<&Page>`/`DataPage<&mut Page>` and friends)
and negotiating const visibility on generic types, instead of the answer the user
had already given: the buffer should be generic and each format should own its
offsets. The lesson is the one from the type-alias entry read backwards — a page
should be a real type, but a *dumb* one; naming one type does not mean teaching it
every layout. Reversed in session 12: `Page` is a buffer with integer accessors,
and each layout is free functions over it.

**The AI let an internal concept leak into the public API (10-05, caught 10-11).**
`BufferPool::fetch` returned a `FrameId` and the accessors were
`page(FrameId)`/`page_mut(FrameId)`, so every caller — the heap file, the scan,
the catalog, the CLI, the tests — spoke in frames even though the spec's contract
is `fetch_page(page_id) -> page` and frames are the pool's private replacement
machinery. The tell was that `HeapScan` had to bundle a `(frame, page)` pair in a
`Pinned` struct purely to carry two names for one page. The AI copied the spec's
pseudo-code (`return frame`) over its own signature (`-> page`) without noticing
they disagreed, and then propagated the choice outward instead of keeping it
inside the pool. Because a `FrameId` is just an array index, the leak also hid a
real hazard: using a frame after its page was evicted reads the wrong page
silently. Found when the user asked what a frame was doing in `fetch` at all;
fixed in session 14 by making the whole pool page-keyed (`fetch`/`page`/
`page_mut`/`unpin` all take `(file, page)`) and deleting `FrameId` from the
public surface.

**The AI stopped one step short of the spec's `fetch_page -> page` (10-11).**
Session 14 removed the frame leak but left `fetch` returning `()`, so the pool's
most important operation still did not hand back a page — the spec's signature
was satisfied only in spirit, by a separate `page(file, page)` call that repeated
the id the caller already had. The AI treated "no frames in the API" as the
finish line and did not re-check the operation's own contract. The user spotted
it with a one-line question — could `fetch` and `new_page` both just return a
`PageId`, resolved later with `page`/`page_mut`? — and the answer deleted the
last gap: one shape (`… -> PageId`) for both allocating and fetching, one way to
borrow. Fixed in session 15.

**The AI turned a straight walk into a state machine (10-11).** `insert`'s first
version tracked an `inserted` slot it only ever set so it could return *after*
the loop, carried the tail of the directory chain out of the loop in a
`last: Option<(PageId, bool)>` (with an `expect`), re-fetched a page it had just
unpinned, and guarded a "stale free count" branch that could not fire. The tell
was the `expect("the walk visits page 0 at least once")`: when correctness rests
on a sentence about what the loop must have done, the loop is shaped wrong. The
user asked whether it had to be that complex; it did not. The spec's shape — walk,
first fit, return on a hit, allocate at the end — is writable directly, and the
rewrite is shorter by a third. Fixed in session 16.
