# REPORT_M1.md — Storage Engine and Buffer Pool

Measurements for §8 of `M1_INSTRUCTIONS.txt`, produced with the release build:

```
cargo build --release
./target/release/radb-storage ...
```

## 1. Capacity

For a fixed-length record of `L` bytes the page holds `floor(4088 / (L + 4))`
records, where `4088 = 4096 - 8` is the free space of an empty page and the
extra 4 bytes are the slot that every inserted record needs.

Three tables were created with `radb-storage create-table` and loaded with 400 rows of
a constant record length, then measured with `radb-storage layout --page 1`.

| Schema | Record length `L` | Formula | Computed | Measured (`slot_count`) | Page `free_space` |
|--------|------------------:|---------|---------:|------------------------:|------------------:|
| `(a INT, b INT, c INT)`, all rows `i,i,i` | 12 | `4088 / 16` | 255 | **255** | 8 |
| `(a INT, b VARCHAR(20), c INT)`, `b` is 10 bytes | 20 | `4088 / 24` | 170 | **170** | 8 |
| `(a INT, b VARCHAR(200), c INT)`, `b` is 200 bytes | 210 | `4088 / 214` | 19 | **19** | 22 |

The computed and measured values agree exactly. The remainder shown in
`free_space` is the point of the `floor`: after `floor(4088/(L+4))` records there
are fewer than `L + 4` bytes left, so one more record plus its slot cannot fit.
For the all-INT table that remainder is `4088 - 255*16 = 8` bytes.

The spec's own example, `L = 13`, gives `floor(4088/17) = 240`; it is asserted in
`page::tests` and `tests/storage.rs`.

Two of the schemas contain a `VARCHAR`, so their capacity depends on the actual
strings. The table above fixes the string lengths (10 and 200 bytes) so that the
computation is meaningful; with shorter strings more records fit, with longer
strings fewer, exactly as the variable-length codec allows.

## 2. Replacement policies

The five traces were supplied as `traces/trace1.txt` … `traces/trace5.txt`.
The handout's published files were not present in this repository, so the five
here are deterministic stand-ins of the same character (uniform working set,
cycle, repeated sequential scan, large uniform, skewed hot set); see
`traces/generate.sh`. (Both the traces and the generator script live in
`traces/` at the repository root, so the command lines below are run from
there.) The `trace` command is unchanged, so dropping in the official files
reproduces the same tables.

Each trace was replayed with

```
radb-storage trace --db DIR --file traces/traceN.txt --frames B --policy lru|clock --json
```

for `B ∈ {4, 16, 64, 256}`. The tables report the hit rate `hits / (hits +
misses)`; every row has `misses = reads` and `writes = 0`, because the trace
fetches each page once and unpins it clean.

### 2.1 LRU (20 rows)

| Trace | B | Hits | Misses | Evictions | Hit rate |
|-------|--:|-----:|-------:|----------:|---------:|
| 1 | 4 | 13 | 987 | 983 | 0.013 |
| 1 | 16 | 64 | 936 | 920 | 0.064 |
| 1 | 64 | 276 | 724 | 660 | 0.276 |
| 1 | 256 | 748 | 252 | 0 | 0.748 |
| 2 | 4 | 0 | 1000 | 996 | 0.000 |
| 2 | 16 | 0 | 1000 | 984 | 0.000 |
| 2 | 64 | 950 | 50 | 0 | 0.950 |
| 2 | 256 | 950 | 50 | 0 | 0.950 |
| 3 | 4 | 0 | 800 | 796 | 0.000 |
| 3 | 16 | 0 | 800 | 784 | 0.000 |
| 3 | 64 | 0 | 800 | 736 | 0.000 |
| 3 | 256 | 720 | 80 | 0 | 0.900 |
| 4 | 4 | 18 | 1982 | 1978 | 0.009 |
| 4 | 16 | 63 | 1937 | 1921 | 0.032 |
| 4 | 64 | 233 | 1767 | 1703 | 0.117 |
| 4 | 256 | 890 | 1110 | 854 | 0.445 |
| 5 | 4 | 216 | 784 | 780 | 0.216 |
| 5 | 16 | 746 | 254 | 238 | 0.746 |
| 5 | 64 | 960 | 40 | 0 | 0.960 |
| 5 | 256 | 960 | 40 | 0 | 0.960 |

### 2.2 CLOCK (20 rows)

| Trace | B | Hits | Misses | Evictions | Hit rate |
|-------|--:|-----:|-------:|----------:|---------:|
| 1 | 4 | 13 | 987 | 983 | 0.013 |
| 1 | 16 | 67 | 933 | 917 | 0.067 |
| 1 | 64 | 274 | 726 | 662 | 0.274 |
| 1 | 256 | 748 | 252 | 0 | 0.748 |
| 2 | 4 | 0 | 1000 | 996 | 0.000 |
| 2 | 16 | 0 | 1000 | 984 | 0.000 |
| 2 | 64 | 950 | 50 | 0 | 0.950 |
| 2 | 256 | 950 | 50 | 0 | 0.950 |
| 3 | 4 | 0 | 800 | 796 | 0.000 |
| 3 | 16 | 0 | 800 | 784 | 0.000 |
| 3 | 64 | 0 | 800 | 736 | 0.000 |
| 3 | 256 | 720 | 80 | 0 | 0.900 |
| 4 | 4 | 18 | 1982 | 1978 | 0.009 |
| 4 | 16 | 62 | 1938 | 1922 | 0.031 |
| 4 | 64 | 239 | 1761 | 1697 | 0.119 |
| 4 | 256 | 886 | 1114 | 858 | 0.443 |
| 5 | 4 | 211 | 789 | 785 | 0.211 |
| 5 | 16 | 728 | 272 | 256 | 0.728 |
| 5 | 64 | 960 | 40 | 0 | 0.960 |
| 5 | 256 | 960 | 40 | 0 | 0.960 |

### 2.3 LRU on trace 3 at B = 64

Trace 3 is a sequential scan of an 80-page file repeated ten times. At `B = 64`
LRU achieves a **0.000** hit rate.

The mechanism, in two sentences: the working set is 80 pages but only 64 fit, and
a single LRU frame is evicted as each new page is read; a page is therefore always
the least-recently-used one exactly when it is about to be needed again, because
by the time the scan wraps around the next pass, all 64 frames hold pages that
were read later in the previous pass. In other words the stream is longer than
the pool and perfectly cyclic, so LRU evicts page `p` just before the next pass
reaches `p`, and no access ever hits.

The same thing happens to CLOCK here, because neither policy can beat capacity:
only when `B >= 80` does the whole file stay resident and the hit rate jump to
0.900 (the first pass is 80 compulsory misses).

### 2.4 Which policy to ship

The two policies are within about two percentage points of each other on these
traces (usually much less), so the choice is about cost, not hit rate. I would
**ship CLOCK**: it reaches essentially LRU's hit rate (sometimes a fraction
better on the uniform traces, a fraction worse on the skewed one), and its
victim search is amortised O(1) per replacement with a single reference bit per
frame, whereas the LRU implementation here scans all `B` frames on every miss to
find the smallest timestamp. Both degenerate identically on the sequential-scan
trace, so CLOCK loses nothing that matters. LRU's only advantage would be a
strict recency guarantee on a workload with a clear temporal locality that
CLOCK's bit approximation blurs; that does not show up in the measured table.

## 3. Scan cost

A 500-page table was built with the 15-column `VARCHAR(255)` schema (each row
fills one page: `499` data pages + the directory page = 500 pages, exactly
2,048,000 bytes) and scanned with

```
radb-storage scan --db DIR --table Big --frames B --json
```

| B | rows | reads | misses | evictions | hits |
|--:|-----:|------:|-------:|----------:|-----:|
| 2 | 499 | **500** | 500 | 498 | 0 |
| 8 | 499 | **500** | 500 | 492 | 0 |
| 64 | 499 | **500** | 500 | 436 | 0 |

Page reads are exactly 500 at every `B`, as required. Each page is read once:
the directory page is pinned for the whole scan and every data page is visited in
order a single time, so the pool never needs to re-read a page it already
evicted. The read count does not fall as `B` grows because nothing is being
re-read at small `B` in the first place; the only quantity that changes is the
eviction count (`evictions = 500 - B`, since the first `B` misses populate the
pool). If reads had fallen with `B`, that would mean the scan was revisiting
pages and small pools were paying to re-read them; if reads had risen, a scan
would be touching pages more than once.

The maximum number of simultaneously pinned pages during the scan is 2 (the
directory page and one data page), asserted directly in
`tests/storage.rs::scan_of_500_pages_reads_about_500_pages` and
`::scan_pins_at_most_two_pages`.

## 4. Determinism

Running any command twice yields identical counters; the buffer pool has no
dependence on wall-clock time, hash iteration order or the file system, and
eviction ties are broken by frame index rather than by an unordered structure.
Two back-to-back `scan --frames 8 --json` runs of the same table are byte-for-byte
identical, and a test asserts it.
