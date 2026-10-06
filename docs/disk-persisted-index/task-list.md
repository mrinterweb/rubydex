# Ship task list — disk-persisted-index

Roadmap: `docs/disk-persisted-index/ship-plan.md`
Review: `docs/disk-persisted-index/review.md`
Detailed plans: `docs/disk-persisted-index/plan-differential-harness.md` (current)

## Current state

- Phase 1, Task 1.1 (differential harness) DONE.
- Baseline commits on branch: `28897e8` (all review items + nits done).
- Branch ahead of origin, unpushed, unsigned (1Password signer broken).
- Next: memory optimization workstream (user priority). First data point already in hand: stdlib differential = 374s release, dominated by full-store label scans + per-id deserialization (Task 1.7 territory).

## Task 1.1: Differential harness (plan: docs/disk-persisted-index/plan-differential-harness.md)

- [x] Task A: fixtures + skeleton + sensitivity test (RED first) — `c57aefd`, RED observed (empty probes)
- [x] Task B: declaration sweep + search probes (sensitivity GREEN) — `9b21ecc`. Side fix: CLI test count 1→7 (built-in doc + 6 diff_corpus fixtures; count is CWD-dependent). Clippy: `#[must_use]` on the two new pub methods, `Id::get`, `map_or_else`, `normalize` slice arg.
- [x] Task C: completion probes — `472f96d`
- [x] Task D: alias/member/require/document probes — `58257c8` (amended after clippy: `from_ref`, `map_or_else`, `map_or`)
- [x] Task E: cypher probes + stdlib ignored test + **Cypher provider store-aware fix**
  - Harness caught a real divergence (its purpose): Cypher `GraphProvider` (`query/cypher/schema.rs`) used in-memory-only map lookups/enumeration everywhere → store-backed graphs returned 0 rows for every label scan.
  - Fix: union enumerators (`document/definition/declaration_node_ids` = memory ∪ store) + all 19 point lookups switched to layered getters. New store accessor `RedbStore::definition_ids()` (key-only scan) + `Graph::store_definition_ids()` (tombstone-filtered, dual-cfg).
  - Stdlib tier is sampled (`sample=2000` cap on full-walk probes; fixture corpus stays full) — unsampled stdlib run is >15 min (O(N²) FQN resolution + per-id store decodes).
  - Stdlib differential GREEN in release: 374s, 0 divergence. Fixture differential + sensitivity GREEN in debug.

## Phase 1 remaining

- [ ] Task 1.2: Store round-trip fuzz
- [ ] Task 1.3: RedbStore::build error path (kill the .expect)
- [ ] Task 1.4: Fork-safety guard
- [ ] Task 1.5: Kill/corrupt/hostile-env matrix
- [ ] Task 1.6: Store format version (decoupled from gem version)
- [ ] Task 1.7: Performance baseline + LRU decision gate
- [ ] Task 1.8: CI matrix
- [ ] Task 1.9: Ops + docs

## Memory optimization (user priority — after 1.1)

- [x] Scope: measure current RAM profile — `docs/disk-persisted-index/memory-baseline.md` (build child 1,539 MB / 6.3s; open path 5 MB baseline; search spike 70 MB → streaming fix → 54 MB peak / 12.9 MB live)
- [x] Ruby-side session profile (`docs/disk-persisted-index/session_profile.rb`): disk 73 MB final vs memory 692 MB; hover 0.09 ms/op; search 100 ms/op (no name-keyed index yet)
- [x] Streaming search (commit `afc0465`) — measured win
- [x] P1 FFI abort root-caused + fixed (commit `7a27716`) — refcount tombstones vs store nodes + FFI null guards; 2 regression tests, RED-verified
- [ ] Next candidate: Stage-5 name-keyed search index (closes the 100 ms/op search gap) — ship-plan Task 3.4
- [ ] Later: require_paths measurement, build-child cleanup time (4.5s = 71% of build), overlay growth under live edits

## Phase 2 / 3 (roadmap)

- [ ] Phase 2: live-edit fuzzer, soak, overlay ceiling, write-back decision
- [ ] Phase 3: Windows build, incremental updates, LRU (if needed), Stage-5 search

## Notes / incidents

- 2026-10-01: transient SIGSEGV in rake ruby_test after manual double-extconf (build state); 4 consecutive green runs since.
- Session-profile crash (P1): `rdx_graph_resolve_constant` aborted the process on store-backed graphs. Root cause: hash-derived raw u64 id spaces overlap (StringId==DeclarationId for same name) + refcount cleanup tombstoning store-held ids. Fixed in `7a27716` (tombstone guard + FFI null guards).
- FFI/C/Ruby signatures stable; fix bugs in Rust core or FFI internals, not the C ABI.

## Post-rebase hardening plan — executed (all signed, pushed)

Plan: `docs/disk-persisted-index/plan-post-rebase-hardening.md`. Executed inline 2026-10.

- [x] A1 stdlib differential after rebase: 327.9s release, 0 divergence
- [x] A2 memory re-measured: disk 60 MB final, search 105 ms/op, hover 0.22 ms; no regression
- [x] A3 Cleanup attributed: redb write 4.49s; drop 10 ms
- [x] B1 StoreError + error counting (`375d1ec`)
- [x] B2 corrupt-store fallback + catch_unwind on open/read/scan (`6f856c8`)
- [x] B3 STORE_FORMAT_VERSION freshness key (`c991d60`)
- [x] B4 full round-trip harness, field-fidelity (`5c9267a`)
- [x] C1 parallel search scan: floor 34 -> 3.5-11 ms/op measured (`541d76a`)
- [x] C2 build write attributed, accepted (`711980f`)
- [x] C3 overlay bounded after 500 edit rounds — no LRU needed (`d0d3493`)
- [x] D1 debug-jemalloc CI job + D2 no-feature CI job (`3e7c917`)
- [x] D3 Windows disk-index tests in CI + D4 docs tracked (`22fe86d`)
- [x] E lint wave green; all commits SSH-signed and pushed (`efaf107`)

Remaining (deferred): live-edit fuzzer/soak (Phase 2), write-back design,
Windows disk-index CI result once the job runs on a runner, n-gram index only
if measured need.

## Incremental refresh (Phase 3) — executed 2026-10, unsigned (sign + push pending)

Plan: `tmp/plan-incremental-refresh.md`.

- [x] 3.1 FS-events crate: `notify` 9.0.0-rc.5 behind an adapter trait (`FsEventSource`); decision record `docs/disk-persisted-index/fs-event-crate.md`; user approved.
- [x] 3.2 Session registry + lock liveness (`edc4a07`): Ruby `File#flock` holds, Rust `File::try_lock` prunes; 2 Rust + 3 Ruby tests.
- [x] 3.3 Adapter + debounce + single-flight indexer (`846dcee`): adapter moved into the manager crate (notify out of the gem build); burst cap = 25% of manifest → `--full`; 5 builder + 4 adapter tests.
- [x] 3.4 Config opt-in + session wiring (`9e228d6`, `aa9dc62`): `[disk_index] manager`, `RUBYDEX_INDEX_MANAGER`, `index_session.rb` builder, manager binary shipped by the compile task.

Measurements (release build, 3 sessions on `/tmp/rdx-ws`):
- Idle manager: **3.4 MB RSS max, 0.2 % CPU max** over 180 s (target < 15 MB, ~0 %).
- Branch-switch storm (200 rewrites, 201 docs): **9.0 s** to a fresh store marker; the burst crossed the 25% cap, so the manager appended `--full` and rebuilt.
- inotify watch cost unchanged: ~9 ms / ~2 MB for ~2,005 dirs.

### Task 1.4 soak gate + Task 1.5 surgical refresh — executed 2026-10, unsigned (sign + push pending)

- [x] Edit soak (`edit_soak_on_corpus`, deterministic LCG script) drove out five store-path invalidation bugs: duplicate references, recording onto a dead declaration, poisoned name materialization, tombstoned name dependents, and the store snapshot resurrecting deleted name-dependency edges (`20e4186`, `d318e74`, `71c441f`).
- [x] Final soak root cause: `invalidate_declaration` chose Remove-vs-Update with an **overlay-only** owner check, so a store-backed live owner read as gone and every visit cascaded the Remove path (`2237b78`). Cascade visit sets then matched exactly (round 0: 4253 vs 1633 → 1633 vs 1633).
- [x] Soak GREEN: seeds 11/12/13 at 5 edits and seed 11 at 20 edits on the stdlib corpus.
- [x] Task 1.5 surgical refresh (`lib/rubydex/graph.rb`): `build_store_via_fork` writes a `<store>.files` manifest of the per-file stamps it stored; `refresh_if_stale` diffs the live tree and re-indexes only those documents, falling back to a full rebuild past `REBUILD_DIFF_RATIO` (25%) or on a lockfile change. Document URIs come from the Rust conversion over FFI (`Graph#path_to_uri`); Ruby's `URI::File.build` disagrees on paths with spaces.

Measurements (release build, `/tmp/rdx-ws` copy of reserv-api, 6,423 files):
- Surgical refresh of 50 touched files: **341 ms** (stat scan 56 ms, `resolve` 64 ms, ~3 ms/file) vs **24.6 s** full rebuild — 72x faster. RSS growth 17 MB (target ≤ 50 MB). 1-file refresh 186 ms, so ~120 ms is the fixed floor.
- Missed the plan's 250 ms target by ~90 ms; the floor is scan + resolve, not per-file work.

Deferred: base-store split; `rdx cache` command; persisting overlay writes back to the store; Stage-5 name-keyed search index; Windows build.
