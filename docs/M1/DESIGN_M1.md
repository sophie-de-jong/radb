# DESIGN_M1.md — Storage Engine and Buffer Pool

This document describes the design of Module 1: the page and record formats, the
heap file, the buffer pool and its two replacement policies, the catalog, and the
bulk loader. It follows the structure of `M1_INSTRUCTIONS.txt`; §3.2 and §5.3 of
that document are reproduced by tests.

Everything lives in `src/storage/` as a library module (`radb::storage`), so the
later modules can call it. The column types (`ColumnType`) and the table shape
(`Column`, `Schema`) live in the shared `src/schema.rs`, used by both storage and
the execution engine. The command-line front end is the `radb-storage`
binary in `src/bin/radb_storage.rs`; it parses arguments with clap and owns all
CLI-specific logic, while the library exposes only general storage primitives
(pages, heap files, the buffer pool, the catalog, the record codec), so the
library carries no CLI dependency.

```
src/storage/
  error.rs    StorageError and its messages
  codec.rs    --schema parsing, the record codec (§2), and the catalog codec (§6)
  page.rs     the raw 4096-byte buffer, its integer accessors, PageId
  slotted.rs  the slotted data-page layout (§3)
  disk.rs     Disk trait, FileDisk, NullDisk (§1)
  buffer.rs   BufferPool, FileId, LRU and CLOCK (§5)
  heap.rs     directory pages, HeapFile, RecordId, streaming HeapScan (§4)
  catalog.rs  catalog.db page and blob format (§6)
  mod.rs      the module list and re-exports
```

The Module 1 documents live under `docs/M1/` together with the supplied
specification, the trace files are in `traces/`, and the library itself has no
CLI dependency.

## 1. Files and addressing

One file per table, named `<table>.db`, plus `catalog.db`, all in the directory
given by `--db` (default `.`). Every file is a sequence of 4096-byte pages with
no header outside the pages. A **page id** is the zero-based index inside its
file; a **record id** (`RecordId`, the spec's `rid`) is a page id and a slot
number. All multi-byte integers are little-endian.

The three integers that address things — a file in the pool (`FileId`), a page
in a file (`PageId`), and a record (`RecordId { page, slot }`) — are distinct
newtypes, each defined next to the thing it names (`FileId` in `buffer.rs`,
`PageId` in `page.rs`, `RecordId` in `heap.rs`), so the compiler rejects passing
one where another is meant.

The disk manager is the `Disk` trait: `read_page`, `write_page`,
`allocate_page`, `num_pages`. `FileDisk` is the real file;
`NullDisk` is an always-zero synthetic device used by `trace` so that grading
the replacement policy has no side effects. Reads past the end of a file return a
zero page, which is how a freshly allocated page reads before it is written.

A page is addressed in the buffer pool by `(FileId, PageId)`, not by page id
alone. Several files share one pool, so a bare page id would alias across files.

## 2. Record format (§2)

A record is the concatenation of its field encodings, in schema order, with **no
header and no null bitmap** (there are no NULLs).

| Type | Encoding |
|------|----------|
| `INT` | 4 bytes, signed, two's complement, little-endian |
| `VARCHAR(n)` | 2-byte unsigned length `L`, then `L` bytes of UTF-8, `L <= n <= 255` |

Record length is fixed for an all-INT schema and variable as soon as a VARCHAR is
present; the record codec (`codec.rs`) never assumes a fixed length.

The spec's example — schema `(a INT, b VARCHAR(5), c INT)`, tuple `(42, 'bob', 7)`
— is a unit test and appears verbatim in a real file below:

```
2a 00 00 00   03 00 62 6f 62   07 00 00 00       = 13 bytes
```

## 3. Slotted page format (§3)

Header at the front, slot directory growing forward, record data growing backward
from the end of the page.

| offset | size | field |
|-------:|-----:|-------|
| 0 | 4 | `page_id` — uint32 |
| 4 | 2 | `slot_count` — uint16, includes dead slots |
| 6 | 2 | `free_end` — uint16, lowest byte used by record data |
| 8 | 4 | slot 0: `offset` uint16, `length` uint16 |
| 12 | 4 | slot 1 |
| … | | |
| `free_end` | | record data, newest at the lowest offset |
| 4095 | | end of page |

A slot whose `offset` is 0 is **dead** (its record was deleted). Slot numbers are
never reused and never renumbered, so a record id stays valid.

Free space is `free_end - (8 + 4 * slot_count)`. An insert of length `L` needs
`L + 4` free bytes (the record and one new slot).

Operations, exactly as in §3.1, are free functions over a raw page in
`slotted.rs`:

* `insert` returns the new slot number or `None` (`NO_SPACE`) — data moves down
  from `free_end`, the slot is appended;
* `get` errors if the slot is past `slot_count` or is dead;
* `delete` errors if the slot is already dead and otherwise writes `(0, 0)`.

One type, `Page` (`radb::storage::Page`), is passed around for every format: a
newtype over the 4096 bytes with `Deref`, `AsRef`/`AsMut`, `From<[u8; 4096]>` and
`Page::zeroed()`. It makes no assumption about the bytes it holds beyond what is
true of every page — little-endian integers at an explicit offset
(`read_u8`/`read_u16`/`read_u32` and the matching `write_*`). Each
layout then owns its own offsets: the slotted page in `slotted.rs` (`HEADER_SIZE`,
`SLOT_SIZE`, `MAX_RECORD`), the directory page in `heap.rs` (`dir_*`), and the
catalog page in `catalog.rs`. The buffer pool hands out `&Page`/`&mut Page`
keyed by `(file_id, page_id)` (§5) and the `Disk` trait moves `Page` values, so
the whole module — page, directory, disk, pool — names one type instead of
ad-hoc `[u8; 4096]` slices.

### 3.1 Worked example (§3.2)

Reproduced by `slotted::tests::worked_example_reproduces`:

| After | slot_count | free_end | free space | slot directory |
|-------|-----------:|---------:|-----------:|----------------|
| empty page | 0 | 4096 | 4088 | empty |
| insert 13 | 1 | 4083 | 4071 | 0:(4083,13) |
| insert 20 | 2 | 4063 | 4047 | 1:(4063,20) |
| insert 13 | 3 | 4050 | 4030 | 2:(4050,13) |
| delete slot 1 | 3 | 4050 | 4030 | 1:(0,0) |

Deleting the 20-byte record frees no usable space: the slot stays, the data stays.

### 3.2 Capacity (§3.3)

For fixed-length records of `L` bytes the page holds `floor(4088 / (L + 4))`.
`L = 13` gives 240. A record longer than 4088 bytes cannot be stored; the loader
reports it naming the table and the row. Overflow pages are out of scope.

## 4. Heap file (§4)

A heap file is a linked list of **directory pages**, each listing data pages and
their free space. **Page 0 of every table file is the first directory page.**

Directory page:

| offset | size | field |
|-------:|-----:|-------|
| 0 | 4 | `page_id` |
| 4 | 4 | `next_dir_page` — uint32, `0xFFFFFFFF` if none |
| 8 | 2 | `entry_count` |
| 10 | 6 | entry 0: `data_page_id` uint32, `free_bytes` uint16 |
| 16 | 6 | entry 1 |
| … | | |

Capacity `floor((4096 - 10) / 6) = 681` entries per directory page.

Operations (`heap.rs`):

* **`insert(record)`** walks the directory chain. The first entry with
  `free_bytes >= len + 4` is used: fetch the data page, insert, update the entry.
  If no page has room, allocate a data page, insert there, and append an entry —
  extending the directory chain when the last directory page is full. Returns a
  `RecordId`. Directory `free_bytes` values are always exact (written from the
  page's real free space on every insert and never raised), so a first-fit entry
  always accepts the record; `insert` relies on that invariant.
* **`get(rid)`** fetches the page, reads the slot, unpins: two page accesses at
  most.
* **`delete(rid)`** fetches the page, marks the slot dead, and unpins. It does
  not rewrite the directory entry: dead slots are not reclaimed, so marking a slot
  dead changes neither `free_end` nor `slot_count` and the page's free space — and
  the entry's stored `free_bytes` — is already current (§3.2's worked example
  shows free space unchanged across a delete). The spec's "update directory
  free_bytes" is therefore a no-op.
* **`scan(schema)`** returns a streaming `HeapScan`, which implements
  `Iterator<Item = Result<(RecordId, Vec<Value>), StorageError>>`. For
  each directory page, for each entry,
  it fetches the data page, and yields each live slot as a decoded row: `next`
  reads the record bytes and decodes them with the schema before returning, so a
  caller like the CLI can print a row the moment it leaves the page. The data page
  is unpinned before moving on. **At most one data page and one directory page
  are pinned at any instant**; the table is never materialised. `HeapScan::drop`
  releases the pins if the caller stops early (`--limit`).

The scan's state is only its position — the pinned directory page, the pinned
data page, and each one's cursor. Entry counts, the next directory page and the
slot count are read back from the pinned page when needed, so there is no second
copy to keep in sync.

### 4.1 Free-space strategy

The chosen strategy is **a per-data-page free-byte count in the directory, with
first-fit insertion**. An insert never reads a data page it will not write, and
the directory page it is already walking is enough to find a home.

The count is exact at the moment it is written: it is set from the page's
`free_space` after every insert and delete. As a safety net, if the count is ever
stale and the page's `insert` returns `None`, the entry is corrected in place and
the search continues rather than failing the insert.

Trade-off: the directory must be updated on every insert, so a hot table writes
one extra page per insert. Documented alternative — a bitmap or a sampled free
list — would make inserts cheaper but either lose precision or risk scanning many
nearly-full pages. Exact counts keep the cost of `insert` predictable.

### 4.2 Compaction decision

**Compaction is not implemented.** The specification makes it optional, and a
deleted slot keeps its `offset = 0` while `free_end` and `free_space` are
unchanged, so space inside a page is not reclaimed. The consequence is
well-defined: repeatedly deleting and re-inserting in one page eventually reports
`NO_SPACE` and starts a new data page even though dead bytes exist. If it were
implemented it would run when a page's dead bytes exceed a threshold (say half
the page) during `insert`, rewriting the live records contiguously and fixing the
slot offsets — a before/after would then be shown here. This is a deliberate
simplicity/space trade-off, not an oversight.

## 5. Buffer pool (§5)

A fixed array of `B` frames. Each frame holds one page, a pin count, a dirty
flag, a CLOCK reference bit and an LRU timestamp. A page table maps
`(file_id, page_id)` to the frame index.

Frames are the pool's internal machinery, not part of its interface. §5's
surface is page-oriented — `fetch_page(page_id) -> page`, `unpin_page(page_id,
is_dirty)`, `new_page() -> page` — so every pool call names a `(file_id, page_id)`
and no caller ever names a frame. `fetch` pins the page and returns the pinned
`PageId`; `new_page` allocates one and returns its id; `page`/`page_mut` resolve
that id to the resident bytes for reading or writing; `unpin` releases it. The
interface is symmetric with §5: the same identity is used to fetch, to touch the
bytes, and to unpin.

The returned `PageId` is a plain `Copy` value, not a borrow, so a caller can hold
two pages pinned at once (the directory page and a data page in `insert`/the
scan) and can re-borrow either page whenever it needs. That is why the handle is
the page id rather than a frame index or a live `&mut Page`.

* `fetch`: on a hit, increment the pin count and run `policy.on_access`
  (**HIT**). On a miss, choose an unpinned victim; if the victim is dirty write
  its page (**WRITE**); read the requested page (**READ**); install it pinned and
  clean; run `policy.on_access` (**MISS**, and an **EVICTION** only if a resident
  page was replaced). Returns the pinned page's id.
* `unpin`: error if the page is not resident or its pin count is already zero;
  otherwise OR in the dirty bit and decrement the pin count.
* `new_page`: allocate a fresh id, install a zero page pinned and dirty, return
  the id. It is not a fetch, so evacuating a dirty victim here counts a write but
  not an eviction or a miss.
* `page`/`page_mut`: borrow a resident page's bytes by `(file_id, page_id)`,
  resolving the id from `fetch`/`new_page`. Panics if the page is not resident —
  an internal invariant, since it is only reachable after a pin.
* `flush_all`: write every dirty frame once, clear the flags.
* `stats`: `{hits, misses, evictions, reads, writes}`.

Counting rules that must be exact: a first `B` misses on an empty pool are not
evictions; reads and writes count every disk call, including eviction writes and
flushes.

### 5.1 LRU

Every access, hit or miss, sets the frame's `last_used` to a monotonically
increasing tick. The victim is the unpinned frame with the smallest `last_used`.
An unused frame has `last_used = 0` and is chosen first, which is why the first
`B` misses do not evict.

### 5.2 CLOCK

Frames form a circle with a reference bit each and a hand. Every access sets the
frame's reference bit. `choose_victim` sweeps from the hand: a pinned frame is
skipped; a frame with `ref == 1` is cleared and skipped; the first frame with
`ref == 0` is evicted, and the hand advances past it. A newly loaded page
therefore gets `ref = 1` with the hand already past it, exactly as §5.2 requires.

### 5.3 Worked example (§5.3)

Reproduced by `buffer::tests::worked_policy_example_reproduces`. `B = 3`, trace
`1 2 3 1 4 2`:

| Policy | hits | misses | evictions | reads | writes |
|--------|-----:|-------:|----------:|------:|-------:|
| LRU | 1 | 5 | 2 | 5 | 0 |
| CLOCK | 2 | 4 | 1 | 4 | 0 |

## 6. Catalog (§6)

`catalog.db` is read and written through the buffer pool like any other data, so
its pages count as reads and writes while it is touched. The format is ours to
choose and is versioned so Module 4 can extend it.

### 6.1 Catalog pages

The catalog is a chain of 4096-byte pages. Page 0 starts with a 12-byte header;
the serialized blob starts at offset 12 and continues on pages 1..`page_count`.

| offset | size | field |
|-------:|-----:|-------|
| 0 | 4 | magic `0x52414442` (`"RADB"`) |
| 4 | 2 | version = 3 |
| 6 | 2 | `page_count` — total catalog pages, including page 0 |
| 8 | 4 | `total_len` — byte length of the blob |
| 12 | … | blob bytes, continuing on the following pages |

`page_count` is at most `u16::MAX`. If a later save needs fewer pages, the old
trailing pages are ignored; the header says how many pages belong to the current
blob. Page 0 is written after the continuation pages, so a failed save leaves the
previous header (and catalog) intact.

### 6.2 Blob

The blob is a bincode encoding of `Vec<TableMeta>` with **fixed-width
little-endian** integers and trailing-byte rejection. Bincode writes strings and
vectors with a `u64` length, integers little-endian, and enum variants as a
`u32` tag. The serialized schema is:

```
TableMeta {
    name: String,
    file_name: String,
    num_pages: u32,
    num_tuples: u64,
    schema: Vec<Column>,          // Column { name: String, ty: ColumnType }
    stats: Option<Stats>,         // Module 4
}
ColumnType = Int | Varchar(u8)
```

The record format of the heap pages is prescribed separately (§2) and is not
affected by the catalog encoding. The statistics layout is described in
`DESIGN_M4.md`.

The contract in `docs/INSTRUCTIONS.txt` also lists `first_page`. Every heap file's
first directory page is page 0 by construction, so the field is implicit; the
catalog stores `num_pages` and the reader derives the rest.

## 7. Bulk loader (§6)

`load` reads a CSV file with no header line, one record per line, comma
separated. A field containing a comma may be wrapped in double quotes; a doubled
quote inside a quoted field is one literal quote. The reader lives in
`src/csv.rs`: `CsvReader::new(schema, input)` borrows the whole input string and
implements `Iterator<Item = Result<Vec<Value>, CsvError>>`, so the loader does not
copy the file or build an intermediate `Vec<String>`. The splitter is still a
two-state byte scanner with **no regular expression and no parser generator**;
each field is a `Cow<str>` borrowed from its line, and only a field with a
doubled quote is folded into an owned `String`.

Each field is turned into a `Value`: an `INT` must parse as a signed 32-bit
integer, a `VARCHAR` must fit its declared length. A failure is reported with the
line number and the column name and the load stops. The encoded record must fit
4088 bytes; otherwise the load reports the table and the row. Rows are inserted
one at a time through `HeapFile::insert`, which takes the typed `&[Value]` and
encodes it with the heap file's schema; the catalog's `num_tuples` and `num_pages`
are updated and everything is flushed before the command exits.

## 8. Command-line interface (§7)

```
radb-storage create-table --db DIR --name R --schema "a:INT,b:VARCHAR(20),c:INT"
radb-storage load         --db DIR --table R --file r.csv
radb-storage scan         --db DIR --table R --frames 8 [--limit N] --json
radb-storage layout       --db DIR --table R --page 3
radb-storage trace        --db DIR --file trace.txt --frames 16 --policy lru|clock --json
```

`--db` defaults to `.`, so the shorter invocations in `docs/INSTRUCTIONS.txt` also
work. `create-table` and `load` accept an optional `--frames` (default 16)
although the spec's invocation omits it. Argument parsing with clap lives in
`src/bin/radb_storage.rs`; `main`'s match runs each subcommand directly in its
arm, on top of the library primitives. Text `scan` prints each row the moment it
reads it (streaming, no intermediate command object); `scan --json` builds the
whole §7 object with a single `serde_json::json!` and prints it once, because a
JSON value must exist before it can be serialised. The library itself knows
nothing about the CLI.

`scan --json` emits exactly the fields

```json
{ "table": "R",
  "schema": [["a","INT"],["b","VARCHAR",20],["c","INT"]],
  "rows": [[42,"bob",7],[43,"amy",9]],
  "stats": {"hits":12,"misses":4,"evictions":1,"reads":4,"writes":0} }
```

built with one `serde_json::json!` and printed via `to_string_pretty` (so the
keys come out alphabetised and the rows one array element per line).

and `trace --json` emits `{policy, frames, hits, misses, evictions, reads,
writes}`. Extra fields would be allowed, but none are emitted.

### 8.1 What the scan counters cover

`scan` reads the catalog first, then calls `BufferPool::reset` and
`reset_stats` so that the reported counters and frames cover **only the table
scan**. This is deliberate: the catalog's on-page format is implementation
defined (§6), so if catalog reads or catalog-resident frames leaked into the
scan, two correct implementations would report different numbers for the same
table and `B`. Excluding them makes the counters reproducible.

## 9. A real page, byte by byte

From a real database created by this tool: `A.db` with schema
`(a INT, b INT, c INT)` after loading 400 rows. Page 1 is the first data page,
filled to its 255-record capacity. First 64 bytes of the file at offset 4096:

```
00001000: 01 00 00 00  ff 00  0c 04  f4 0f 0c 00  e8 0f 0c 00
00001010: dc 0f 0c 00  d0 0f 0c 00  c4 0f 0c 00  b8 0f 0c 00
00001020: ac 0f 0c 00  a0 0f 0c 00  94 0f 0c 00  88 0f 0c 00
00001030: 7c 0f 0c 00  70 0f 0c 00  64 0f 0c 00  58 0f 0c 00
```

Reading it:

| bytes | field | value |
|-------|-------|-------|
| `01 00 00 00` | `page_id` | 1 |
| `ff 00` | `slot_count` | 255 |
| `0c 04` | `free_end` | 1036 |
| `f4 0f 0c 00` | slot 0 | offset 4084, length 12 |
| `e8 0f 0c 00` | slot 1 | offset 4072, length 12 |
| `dc 0f 0c 00` | slot 2 | offset 4060, length 12 |
| … | | each slot 12 bytes lower |

`free_space = 1036 - (8 + 4*255) = 8`; the eight bytes left over are the
remainder of `4088 / 16`, exactly as §3.3 predicts. `layout --table A --page 1`
prints these same numbers.

The record bytes are the spec's own example. From `R.db`, page 1, slot 0
(`42, 'bob', 7`):

```
00001ff3: 2a 00 00 00  03 00 62 6f 62  07 00 00 00
```

and the directory page (`R.db` page 0) shows one entry pointing at page 1 with
4013 free bytes:

```
00000000: 00 00 00 00  ff ff ff ff  01 00  01 00 00 00  ad 0f
```

## 10. Errors (§9)

Each failure mode has its own `StorageError` variant and a message naming the
offending entity:

* evicting a pinned page: the fetch returns `NoUnpinnedFrame`, never a silent
  choice;
* unpinning a page at pin count 0: `UnpinZeroPin`;
* reading a dead slot: `DeadSlot { page, slot }`;
* a record longer than 4088 bytes: `RecordTooLong { table, row, len, max }`.

A bad CSV field (`INT` out of range, `VARCHAR` too long, wrong field count) is a
loader concern, so it is reported from the binary as
`line L, column C: <detail>`, naming both.

## 11. Tests

`cargo test` is green (99 tests over the whole project). Module 1 contributes 42
of them: 22 unit tests across `src/storage/` (page accessors, the page-id
newtype, the slotted-page worked example, the record codec, the disk round-trip,
the catalog blob, the buffer policies, the record-id display, and the
pinned/zero-pin errors), 5 for
`CsvReader` in `src/csv.rs` (borrowing, doubled-quote folding, typed rows,
line/column errors, and the unterminated-quote error), and 15
integration tests in `tests/storage.rs`. The integration tests drive the public
API and the real binary and cover the §10 checklist: the §3.2 page example, the
§5.3 policy example, a scan of a table more than twenty times `B` that pins at
most two pages, deterministic counters, the pinned-eviction error, a directory
chain past 681 pages, and an empty scan after deleting every record.
