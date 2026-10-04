# Memory baseline — disk-persisted-index (2026-10-02)

Corpus: Ruby 4.0.5 stdlib + bundled gems (27,674 files, 375,124 names, 626,579 definitions).
All release builds (`cargo run --release --features redb-store`). `/usr/bin/time` not installed;
bash built-in `time` for wall, `--stats` for RSS.

## Build path (fork child — heavy work)

`rubydex_cli <corpus> --build-store /tmp/rdx-bench/stdlib.redb --stats`

| Stage      | Time    | %     |
|------------|---------|-------|
| Listing    | 0.009s  | 0.1%  |
| Indexing   | 0.596s  | 9.5%  |
| Resolution | 0.995s  | 15.8% |
| Querying   | 0.204s  | 3.2%  |
| Cleanup    | 4.495s  | 71.4% |
| **Total**  | **6.30s** |     |

- Peak RSS: **1,539 MB** (in-memory graph + store build)
- Store on disk: **1.1 GB**
- "Cleanup" stage = store build + drop (dominant cost; 71% of build time)

## Open path (primary LSP process — must stay light)

`rubydex_cli <corpus> --open-store stdlib.redb [...]`

| Workload            | Peak RSS  | Wall (incl. spawn) |
|---------------------|-----------|--------------------|
| open + 1 FQN query  | **5.0 MB** | 1 ms               |
| open + prefix search ("enum", 222 hits) | **69.6 MB** | 62 ms              |
| open + query + search | 70.6 MB  | 64 ms              |

### Attribution

- Rest / hover-scale lookups: ~5 MB (matches POC's ~4 MB claim; the POC measured pre-scan).
- **Search is the spike**: `RedbStore::search_names()` materializes ALL 626k
  `(DeclarationId, String)` FQN pairs per search, then filters in Rust.
  ~65 MB transient allocation + 626k postcard decodes per keystroke-driven search.

## Comparison vs in-memory (main branch behavior)

|                        | In-memory | Store-backed |
|------------------------|-----------|--------------|
| Boot (index+resolve)   | 1.6s + 1,539 MB resident | 6.3s child (forked), 5 MB primary |
| Hover/FQN lookup       | ~1539 MB resident | 5 MB |
| Prefix search          | ~1539 MB resident | 70 MB transient spike |

## Optimization results

### 1. Streaming search — DONE (measured win)

`RedbStore::declaration_ids_matching(predicate)` filters during the redb range scan;
`Graph::store_declaration_ids_matching` wraps it with tombstone filtering (dual cfg);
`declaration_search` streams store matches instead of materializing 626k `(id, String)`
pairs + a HashMap. Memory-takes-priority semantics preserved (store stream skips ids
present in the overlay; tombstones excluded). Differential harness `probe_search` green.

| | Before | After |
|---|--------|-------|
| Search peak RSS (default jemalloc) | 69.6 MB | 54.3 MB |
| Search peak RSS (`dirty_decay_ms:0`, i.e. live data) | — | **12.9 MB** (5 MB base + 8 MiB redb cache) |
| Search wall (incl. spawn) | 62 ms | 32 ms |
| Per-search retained allocation | ~25 MB (pairs Vec + HashMap) | none (churn only) |

The 54.3 → 12.9 MB gap is jemalloc dirty-page retention of transient scan buffers
(626k `from_utf8_lossy` strings); pages return to the OS under decay/pressure.
Not a leak. Allocator config left untouched.

## Remaining candidates (measure before/after; keep only measured wins)

2. `require_paths` materializes all 27,674 URIs per call (~2 MB) — likely fine; measure, don't assume.
3. redb read-cache tuning — not implicated at this scale (query-only = 5 MB); revisit if a larger corpus changes it.
4. Cypher label scans deserialize every declaration (harness stdlib run = 374s) — test-only today; LSP cypher usage is rare. Defer.
5. ~~Ruby-side LSP session profile~~ — DONE, below.
6. Stage-5 search index (name-keyed table) would cut the full scan itself — ship-plan Task 3.4, only if measured latency on larger corpora demands it. Session profile below shows store-backed search at ~100 ms/op vs 0.05 ms/op in-memory on the stdlib corpus — this is the candidate that would close it.

## Ruby-side session profile — DONE (`docs/disk-persisted-index/session-profile.rb`, release .so, stdlib corpus)

Long-lived `Graph` with `index_workspace`, then 200× search / 200× resolve_constant
(hover) / 200× resolve_require_path / 20× full require_paths. Disk mode attaches the
prebuilt store; memory mode indexes in-process.

| | Disk (store-backed) | Memory |
|---|---|---|
| Boot | 0.34 s (attach) | 0.51 s (in-process index) |
| RSS after boot | 33 MB | 809 MB |
| RSS final (after all phases) | **73 MB (HWM 75)** | 692 MB (HWM 856) |
| search | 100 ms/op | 0.05 ms/op |
| resolve_constant (hover) | 0.09 ms/op | 0.00 ms/op |
| resolve_require_path | 0.00 ms/op | 0.00 ms/op |
| require_paths (full enumeration) | 22.8 ms/op | 18.5 ms/op |

Disk mode ≈ 1/10 the RSS. Search is the only slow phase (full 626k-row scan per call —
no name-keyed index yet; candidate 6 above). require_paths is identical in both modes.

### Incident (fixed, commit `7a27716`): FFI abort in `rdx_graph_resolve_constant`

The first disk-mode profile run aborted the process at
`rust/rubydex-sys/src/graph_api.rs:242` (`graph.declaration(id).unwrap()` on None →
panic in `extern "C"`). Root cause: StringId/NameId/DeclarationId raw u64 spaces
overlap (all hash-derived); refcount cleanup of transient FFI names
(`untrack_name` → `untrack_string`) tombstoned raw ids the store still held, killing
the same-named declaration on the next call. Two-part fix: (1) with a store attached,
refcount cleanup never tombstones (only explicit live-edit deletions do); (2) FFI
boundary returns null instead of unwrapping a missing layered-getter result.
Regression tests: `untracking_overlay_names_does_not_tombstone_store_nodes`
(store.rs) and `resolve_constant_returns_null_for_deleted_declaration`
(graph_api.rs, RED-verified via SIGABRT with the guard reverted).

## Post-rebase measurements (post-8f9359b, release .so)

_These supersede every number above; the pre-rebase figures are kept for comparison._

Corpus: Ruby 4.0.5 `rubylibdir/..` — **29,360 files, 386,192 names, 686,691 definitions**
(pre-rebase corpus was 27,674 files / 626,579 definitions, so it grew ~6%).

### Build path (`rubydex_cli --build-store ... --stats`, release)

| | Pre-rebase | Post-rebase |
|---|---|---|
| Total build | 6.3 s | 6.67 s |
| Cleanup (store write + drop) | 4.495 s (71.4%) | **4.873 s (73.1%)** |
| Indexing / Resolution / Querying | 0.596 / 0.995 / 0.204 s | 0.594 / 0.960 / 0.227 s |
| Peak RSS | 1,539 MB | 1,573 MB |
| Store size | 1.1 GB | 1.00 GB |

### Session profile (long-lived Ruby process, 200 searches / 200 hovers / 200 require-path resolves / 20 full require-path enumerations)

| | Disk (store) | Memory |
|---|---|---|
| Boot | 0.37 s | 0.54 s |
| RSS after boot | 34 MB | 876 MB |
| RSS final (HWM) | **60 MB (63)** | 726 MB (885) |
| search | 104.8 ms/op | 0.05 ms/op |
| hover (resolve_constant) | 0.22 ms/op | 0.00 ms/op |
| resolve_require_path | 0.00 ms/op | 0.00 ms/op |
| require_paths (full) | 21.0 ms/op | 21.2 ms/op |

**Verdict: no memory regression** (60 MB vs 73 MB disk, and the corpus grew 6%). Search is
unchanged at ~100 ms/op — the full 686k-row scan, unchanged by the rebase. Hover went 0.09 →
0.22 ms/op (sub-millisecond either way).

Store-builder spawn overhead (replacing `fork`, which corrupted jemalloc): cold build+attach
0.571 s for a one-file workspace, warm attach 0.059 s. That is Ruby startup + `require`, paid once
per cold store build.

### Measurement trap (cost me a false "28x regression")

Every latency above requires the **release** `.so` (`bundle exec rake compile_release`). Measured
against the **debug** `.so`, the same search reads 997 ms/op and hover 1.38 ms/op — a 10x
difference from build profile alone. Always confirm which profile is installed before quoting a
perf number. A related trap: `Graph.open_store` twice on the same path in one process fails
(redb holds an exclusive file lock), which looks like a corrupt-store error but is not one.

### Where the store-build "Cleanup" stage goes (post-rebase attribution)

`bench_build_write_vs_drop` (release, stdlib corpus, kept as an `#[ignore]`d test for C2):

```
index=494ms  resolve=828ms  write=4486ms  drop=10ms  size=1,077,940,224
```

**Cleanup is 99.8% the redb write transaction**, not the drop: serializing 686,691 definitions
plus committing a 1.08 GB database. Dropping the store costs 10 ms, so "close the store faster" is
not a lever. Any C2 optimization has to shrink the bytes written or make the commit cheaper.

### Store-build write, table by table (C2, release)

`bench_build_write_vs_drop` (kept as an `#[ignore]`d test) times each table's serialize+insert
inside the single write transaction:

```
method_references  1.53 s   1,818,270 nodes   51 MB
definitions        0.93 s     686,691 nodes  109 MB
constant_references 0.49 s    764,834 nodes   18 MB
search_names       0.38 s     386,192 rows
declarations       0.45 s     386,192 nodes   65 MB
names              0.17 s     252,856 rows
name_dependents    0.19 s     252,831 rows
strings            0.12 s     211,408 rows
documents          0.08 s      29,360 nodes   36 MB
document_uris      0.03 s      29,360 rows
commit             0.21 s
```

**Verdict: accepted.** The write is ~4.2M serialize+insert operations (~1µs each) and is
single-threaded — redb permits one write transaction, so the shards-parallel trick from C1 does not
apply. The commit itself is 0.21s; compression would trade CPU for space while CPU is already the
cost. Cold builds stay a one-time cost per workspace (freshness marker); no change made.
