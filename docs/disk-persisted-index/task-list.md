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
