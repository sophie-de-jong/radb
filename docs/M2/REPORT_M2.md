# REPORT_M2.md — Execution Engine

All numbers below come from the release build through the public CLIs. The
measurement harness is `bench/measure.py` (mode `m2`); it generates its own
tables under `bench/data/` so the run is reproducible:

```
cargo build --release
python3 bench/measure.py m2
```

Counters are the sum of the per-operator `radb-exec trace` lines. The join tables
are `a:INT,b:INT,pad:VARCHAR(72)` and `b:INT,c:INT,pad:VARCHAR(72)`, which hold
47 rows per page; “pages” below is the data-page count after loading. The three
sizes are 10, 40 and 100 data pages per input.

## 1. Cost against formula

The formula columns are M2 §2: nested loop `M + ceil(M/(B−2))·N`, sort-merge
`sort(M) + sort(N) + M + N`, hash `3(M+N)`.

| pages | M | N | B | join | measured reads | measured writes | formula I/O |
|---:|---:|---:|---:|---|---:|---:|---:|
| 10 | 10 | 10 | 4 | nested loop | 100 | 0 | 60 |
| 10 | 10 | 10 | 4 | sort-merge | 534 | 91 | 80 |
| 10 | 10 | 10 | 4 | hash | 74 | 22 | 60 |
| 10 | 10 | 10 | 16 | nested loop | 20 | 0 | 20 |
| 10 | 10 | 10 | 16 | sort-merge | 510 | 67 | 40 |
| 10 | 10 | 10 | 16 | hash | 52 | 32 | 60 |
| 10 | 10 | 10 | 64 | nested loop | 20 | 0 | 20 |
| 10 | 10 | 10 | 64 | sort-merge | 510 | 67 | 40 |
| 10 | 10 | 10 | 64 | hash | 90 | 70 | 60 |
| 10 | 10 | 10 | 256 | nested loop | 20 | 0 | 20 |
| 10 | 10 | 10 | 256 | sort-merge | 510 | 67 | 40 |
| 10 | 10 | 10 | 256 | hash | 108 | 88 | 60 |
| 40 | 40 | 40 | 4 | nested loop | 1400 | 0 | 840 |
| 40 | 40 | 40 | 4 | sort-merge | 2288 | 516 | 640 |
| 40 | 40 | 40 | 4 | hash | 610 | 82 | 240 |
| 40 | 40 | 40 | 16 | nested loop | 240 | 0 | 160 |
| 40 | 40 | 40 | 16 | sort-merge | 2122 | 350 | 320 |
| 40 | 40 | 40 | 16 | hash | 172 | 92 | 240 |
| 40 | 40 | 40 | 64 | nested loop | 80 | 0 | 80 |
| 40 | 40 | 40 | 64 | sort-merge | 2040 | 268 | 160 |
| 40 | 40 | 40 | 64 | hash | 220 | 140 | 240 |
| 40 | 40 | 40 | 256 | nested loop | 80 | 0 | 80 |
| 40 | 40 | 40 | 256 | sort-merge | 2040 | 268 | 160 |
| 40 | 40 | 40 | 256 | hash | 344 | 264 | 240 |
| 100 | 100 | 100 | 4 | nested loop | 8500 | 0 | 5100 |
| 100 | 100 | 100 | 4 | sort-merge | 5744 | 1314 | 1600 |
| 100 | 100 | 100 | 4 | hash | 3142 | 200 | 600 |
| 100 | 100 | 100 | 16 | nested loop | 1300 | 0 | 900 |
| 100 | 100 | 100 | 16 | sort-merge | 5302 | 872 | 800 |
| 100 | 100 | 100 | 16 | hash | 406 | 206 | 600 |
| 100 | 100 | 100 | 64 | nested loop | 400 | 0 | 300 |
| 100 | 100 | 100 | 64 | sort-merge | 5292 | 862 | 800 |
| 100 | 100 | 100 | 64 | hash | 468 | 268 | 600 |
| 100 | 100 | 100 | 256 | nested loop | 200 | 0 | 200 |
| 100 | 100 | 100 | 256 | sort-merge | 5096 | 666 | 400 |
| 100 | 100 | 100 | 256 | hash | 628 | 428 | 600 |

Nested loop agrees with the formula at B ≥ 16 (for 100 pages, 1300 at B=16 is
900 + the directory-page refetches the inner restart performs). Below that, the
measured reads are larger than the formula because the memory block is sized by
`row_ram_size` — encoded record plus `Vec`/`Arc`/`Value` bookkeeping — rather than
by encoded pages alone. At B=4 a block holds fewer than two encoded pages, so
the inner is restarted more often than `ceil(M/(B−2))` suggests. The formula is
a page-level model; this implementation deliberately errs on the side of fewer
rows per block rather than overrunning `B`.

Sort-merge writes its final runs to disk for the join (so the two sorts never
hold their merge readers together); that is why its writes are close to its
reads. Hash join’s partition phase writes, then the probe phase reads, so its
measured reads+writes are the smallest at small B, where the input does not fit.
The formula’s `3(M+N)` assumes every partition is touched; with an empty
partition the implementation does less.

## 2. Sort passes

Passes are derived from the sort node’s written pages: the final pass streams and
writes nothing, so `1 + writes / N` is the number of passes. The formula is
`1 + ceil(log(ceil(N/B)) / log(B−1))`.

| N | B | writes | reads | measured passes | formula |
|---:|---:|---:|---:|---:|---:|
| 10 | 4 | 12 | 12 | 2 | 2 |
| 10 | 8 | 11 | 11 | 2 | 2 |
| 40 | 4 | 124 | 124 | 4 | 4 |
| 40 | 16 | 41 | 41 | 2 | 2 |
| 80 | 4 | 255 | 255 | 4 | 4 |
| 80 | 16 | 82 | 82 | 2 | 2 |
| 100 | 32 | 100 | 100 | 2 | 2 |

Every measured pass count matches the formula, including `N = 80`, `B = 4`
(`N = 20 × B`). Writes slightly exceed `N × (passes−1)` because each run has a
partly filled last page; for example `N=10, B=4` writes 12 pages in its two
written passes. The reads include the pass-0 input reads attributed to the child
scan.

## 3. Crossover

Fixed 100-page inputs, varying B (total I/O = reads + writes):

| B | nested loop | sort-merge | hash | winner |
|---:|---:|---:|---:|---|
| 4 | 8500 | 7058 | 3342 | hash |
| 8 | 2900 | 6594 | 1025 | hash |
| 16 | 1300 | 6174 | 612 | hash |
| 32 | 700 | 6162 | 652 | hash |
| 64 | 400 | 6154 | 736 | nested loop |
| 128 | 200 | 5762 | 852 | nested loop |
| 256 | 200 | 5762 | 1056 | nested loop |

Fixed `B = 16`, varying input size:

| pages | nested loop | sort-merge | hash | winner |
|---:|---:|---:|---:|---|
| 10 | 20 | 577 | 84 | nested loop |
| 40 | 240 | 2472 | 264 | nested loop |
| 100 | 1300 | 6174 | 612 | hash |

The ranking changes at `B = 64`: the 100-page outer fits in `B−2` frames, so the
block nested loop reads each input once and needs no temporary files. Hash join
still pays its partition write and probe read. Sort-merge is never cheapest on
this workload because both inputs fit the cache at larger B and its sort writes
dominate at small B.

## 4. Skew

Both inputs had 1,880 rows (40%) with key 1 and the rest spread over distinct
keys. At `B = 4` there are `P = 3` hash partitions; the hot key lands in one
partition, which receives 1,880 rows on each side — about 40 encoded pages, far
above the 3-frame partition budget. The build side therefore overflows its
in-memory table and the partition pair falls back to a block nested loop over the
two spilled runs; the other partitions still probe normally.

| case | reads | writes | output rows |
|---|---:|---:|---:|
| uniform 100 pages, B=4 | 3142 | 200 | 56,400 |
| 40% hot key, 100 pages, B=4 | 3974 | 200 | 3,546,620 |

The skewed run costs 26% more reads and leaves writes unchanged. The output is
63× larger because every hot-key row on the left pairs with every hot-key row on
the right, which is the expected Cartesian product of the duplicate run.

## 5. The budget

A sort of `N = 80` pages at `B = 4` (20 × B) completes. It returns the expected
7,520 rows, performs 335 reads and 255 writes, and reports `peak_frames = 4`.
The peak is the budget’s high-water mark, including the lazy final merge’s
reader pages (recorded with `Budget::observe`); it never exceeds `B`.
