# Disk-Persisted Index: POC → Ship Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement tasks. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the opt-in disk-persisted index shippable in rubydex: correct on every query path, safe under every failure mode, performant on hot paths, and operationally sane for a near-universal gem.

**Architecture:** Existing: `redb` store keyed by interned `u64` node IDs, layered accessors (memory overlay over store), materialize-on-write for live edits, fork-based store build with signature-based freshness. This plan adds: a differential test harness (prove correctness, don't review it), real error paths, a fork-safety guard, store format versioning, a performance gate, and CI/docs. Live-edit hardening and scaling are later phases.

**Spec:** `docs/disk-persisted-index/review.md` (review), `MEMORY_POC.md` (memory model + reproduce steps), and the ship-gap analysis (Phase 1–3 structure below).

## Global Constraints

- Opt-in stays double-gated: `redb-store` Cargo feature (build) + `RUBYDEX_DISK_INDEX=1` (runtime). The in-memory path must remain behavior-identical; a CI job builds and tests the gem **without** the feature to prove it.
- Every failure mode ends in clean fallback to in-memory. Never a process crash, never a silently-wrong answer.
- TDD: no production code without a failing test, watched red first.
- This is a roadmap: each task expands into a detailed TDD plan (failing-test → fix → green → commit steps) when the phase starts. Tasks are right-sized to be independently reviewable.
- Commits per task. (1Password SSH signer currently broken: use `--no-gpg-sign`.)
- Current baseline (post-review-fixes, commit `28897e8`): 1104 Rust tests (store feature) / 1089 (default) / 6 (sys store) / 265 Ruby, clippy + rubocop clean.

---

## Phase 1 — Harden the read path (shippable v1)

**Exit criteria:** `RUBYDEX_DISK_INDEX=1` on a large workspace boots from a fresh store, every query API returns results identical to in-memory, and every kill/corrupt/env-hostile scenario ends in clean fallback.

### Task 1.1: Differential test harness

**Detailed plan: `docs/disk-persisted-index/plan-differential-harness.md`** (tasks A–E, ready to execute)

**Files:**
- Create: `rust/rubydex/tests/differential_store.rs` (integration test; redb-store feature)
- Modify: `rust/rubydex/src/test_utils.rs` or existing test helpers as needed

**What:** Index a corpus fully in-memory; build the store; reopen store-backed. Run every public query API against both graphs; assert identical results (order-insensitive where APIs are unordered).

- [ ] Enumerate the query surface from FFI entry points (`rust/rubydex-sys/src/lib.rs` + `*api.rs`): definition location, expression completion, follow method alias, declaration search, require paths / resolve require, references, ancestors/linearization, signatures, document getters. Each becomes a harness case with ≥1 real symbol and ≥1 symbol that only exists in the store-backed state if the corpus is big enough.
- [ ] Corpus: Ruby stdlib via `RbConfig::CONFIG["rubylibdir"]` (as in `MEMORY_POC.md`) plus one real gem checkout (vendored path via env var).
- [ ] Harness runs green. Any mismatch is a real bug → fix via TDD (test from the harness, then fix), like review items 1–6.
- [ ] Wire into CI (runs under `--features redb-store`).

**Acceptance:** `cargo test -p rubydex --features redb-store --test differential_store` green on stdlib corpus; deleting any one query case from the harness is a reviewable regression.

### Task 1.2: Store round-trip fuzz

**Files:**
- Create: test module in `rust/rubydex/src/model/store.rs` (or `tests/` if it needs generated fixtures)

**What:** Seeded random graph generator (names, namespaces, methods, refs, documents) → `RedbStore::build` → reopen → assert every node of every type deserializes byte-equivalent (compare via Debug/PartialEq on the model, not raw bytes).

- [ ] Generator produces all node types, including multi-definition names and cross-document references.
- [ ] N=100 seeds in CI, N=1000 locally.

**Acceptance:** round-trip is a proven invariant, not an assumption. Any future schema change that breaks round-trip fails CI.

### Task 1.3: `RedbStore::build` error path (kill the `.expect`)

**Files:**
- Modify: `rust/rubydex/src/model/store.rs` (`RedbStore::build`)
- Modify: FFI boundary (`rust/rubydex-sys`) + `lib/rubydex/graph.rb` fallback

**What:** Decide by inspection: if postcard serialization is infallible for these types, replace the `.expect("node should serialize")` with the infallible API and document why it can't fail. If any step can fail (allocation, IO), return a `StoreError` (the Stage-1 error type the review references) through FFI as a clean error; `build_store_via_fork` already surfaces child failure as `RuntimeError` → in-memory fallback in `index_workspace`.

- [ ] Failing test first: force a build failure mode that is testable (read-only cache dir for `open`; for `build`, a path whose parent is a file, or a mock at the FFI boundary). Assert the Ruby side falls back to in-memory and the error is inspectable, not a crash.
- [ ] Remove/replace the `.expect`; update the "spike-only" comment in `MEMORY_POC.md` if it stays.

**Acceptance:** no `.expect`/`unwrap` on the store write path; a failed build degrades to in-memory with a logged reason.

### Task 1.4: Fork-safety guard

**Files:**
- Modify: `lib/rubydex/graph.rb` (`build_store_via_fork`, `index_workspace`)
- Test: `test/disk_index_live_edit_test.rb` or new `test/disk_index_fork_test.rb`

**Problem:** fork is safe only while the parent's Rust state is thread-free. A rebuild triggered after the parent has run rayon (indexing/resolving in-process) inherits locks held by threads that don't exist in the child — corruption class. (A transient SIGSEGV was observed during this branch's development; attributed to build state, but the hazard is structural.)

**Decision (surface to maintainer):** recommended rule — track whether the parent graph has done any Rust work (`index_all`/`resolve`/materialize). First build from a fresh graph: fork (fast). Any later build: spawn a fresh Ruby child (`ruby -e` loading the gem and calling `build_store`) — universally safe; Ruby startup is negligible next to a multi-minute index build.

- [ ] Failing test first: a test that forces the second-build path after in-process Rust work and asserts the spawn path is taken (deterministic via the guard flag), not fork.
- [ ] Implement the guard flag + spawn path. Spawn child must get the same signature/marker protocol as the fork path (reuse `build_store_via_fork` publish logic; extract if needed).

**Acceptance:** no fork ever happens with warm Rust threads; both build paths publish with the same atomic rename protocol.

### Task 1.5: Kill / corruption / hostile-environment matrix

**Files:**
- Test: `test/disk_index_live_edit_test.rb` (Ruby-level: fork kill, corrupt file, read-only dir) + Rust-level store open tests in `rust/rubydex/src/model/store.rs`

- [ ] SIGKILL the build child mid-build → parent raises → in-memory fallback; no `*.building` residue; no partial store published (existing ensure cleanup covers residue; add the assertion).
- [ ] Corrupt store bytes (flip bytes in the middle of a built store) → `attach_store`/`open_store` returns error → clean fallback. (redb may reject at open; if it accepts and fails per-read, the per-read path must also degrade — test both a header-corrupt and a mid-file-corrupt store.)
- [ ] Read-only cache dir → clean fallback at build; read-only store file → open works (multi-reader) — assert.
- [ ] ENOSPC: manual script only (flaky in CI), documented in the task.

**Acceptance:** every scenario above ends in in-memory fallback with a logged reason; zero crashes.

### Task 1.6: Store format version, decoupled from gem version

**Files:**
- Modify: `rust/rubydex/src/model/store.rs` (metadata table or key), `lib/rubydex/graph.rb` (`store_signature`)

**Problem:** the freshness signature embeds `Rubydex::VERSION`, so every gem release invalidates every user's store → full rebuild (peak RAM = in-memory build). For a near-universal gem with frequent releases that is a real tax.

**What:**
- [ ] Add `FORMAT_VERSION: u32` written to the store at build; `open` checks it; mismatch → clean `Err` (unsupported version) → rebuild. Test with a doctored version.
- [ ] Change the rebuild signature from `Rubydex::VERSION` to `FORMAT_VERSION` (keep lockfile + workspace source signature). Convention: any node schema change MUST bump `FORMAT_VERSION` — enforced by the round-trip fuzz (1.2) plus a pinned fixture store checked into the repo that must still open (cross-version fixture test).
- [ ] Failing tests first: (a) store built with an old pinned fixture opens or rebuilds cleanly; (b) doctored FORMAT_VERSION → clean error.

**Acceptance:** a gem patch/minor release does not invalidate stores; a schema change provably does.

### Task 1.7: Performance baseline + LRU decision gate

**Files:**
- Create: `BENCHMARKS.md` (or extend `MEMORY_POC.md`)
- Use: `utils/bench` (per AGENTS.md; requires `DEFAULT_BENCH_WORKSPACE` — prompt maintainer if unset) and/or the `rubydex_cli --stats` path from `MEMORY_POC.md`

**What:** Measure hot paths on a large workspace, in-memory vs store-backed: definition location (workspace + gem symbol), expression completion, declaration search, references, hover-shaped multi-location query (exercises `REBUILT_LINE_INDEXES`). Record table in the doc.

- [ ] Decision gate, recorded in the doc with the numbers: if store-backed hot paths exceed the agreed multiple of in-memory (propose: 3× on p50), implement a bounded per-process LRU over deserialized hot nodes in front of the layered getters (start: names + declarations, the hottest). If within gate, no LRU (YAGNI).
- [ ] If LRU: TDD with a hit/miss counter test; bound memory explicitly (document the ceiling).

**Acceptance:** numbers exist; either an LRU ships (tested) or the decision is documented with the data.

### Task 1.8: CI matrix

**Files:**
- Modify: CI config (find existing: `.github/workflows/` or equivalent)

- [ ] Jobs: Rust tests + clippy × {default, redb-store}; Ruby suite × {feature on, feature **off** (proves default gem untouched)}; differential harness (1.1) on the default platform; gem packaging smoke: `gem build` → install into a clean bundle → require → index a small fixture workspace with and without `RUBYDEX_DISK_INDEX=1`.
- [ ] Windows: assert the no-fork fallback path (read-only store support); do not require store *build* on Windows in v1 (documented).

**Acceptance:** a red feature cannot ship silently; a broken default build is caught.

### Task 1.9: Ops + docs

**Files:**
- Modify: `README.md` (or docs/), `MEMORY_POC.md` → rename/evolve to `docs/disk-index.md` if it graduates

- [ ] Document: `RUBYDEX_DISK_INDEX`, `RUBYDEX_CACHE_DIR`, where the store lives, size expectations (515 MB for stdlib alone — set expectations), rebuild triggers (format version, lockfile, workspace sources), fallback behavior, Windows read-only status.
- [ ] Diagnostic: CLI/`--stats` reports store freshness, size, and format version for a workspace (small addition to `rubydex_cli`).
- [ ] Treat `RUBYDEX_DISK_INDEX` as semi-public API: no more renames without a deprecation note.

**Phase 1 exit:** all tasks green; v1 story = "boot from disk, correct read-path answers, safe failure, honest docs." The live-edit overlay is already implemented and unit-tested; v1 ships with it and documents that session edits don't persist (fresh boot rebuilds). Phase 2 is what earns the "solid" claim for edits.

---

## Phase 2 — Live edits: prove it, then persist

**Entry:** Phase 1 green.

### Task 2.1: Edit-sequence fuzzer
Random document edit sequences (insert/delete lines, add/remove methods and constants, add/remove files, change requires) applied to a store-backed graph via `consume_document_changes`; after each batch, assert query results == fresh in-memory reindex of the same corpus (reuse the 1.1 harness) and tombstone invariants (no resurrection, no stale overlay shadow). Failing seed = bug → TDD fix.

### Task 2.2: Soak test
Scripted long session (or real ruby-lsp run) on a large workspace: continuous edits + queries over N minutes. Assert: no crash, RSS ceiling recorded, spot-checked correctness. Output appended to `BENCHMARKS.md`.

### Task 2.3: Overlay memory ceiling
Measure overlay growth under bulk edits (large file rewrites, mass rename). Define the *modified* set precisely (nodes changed vs store — these cannot be evicted; unchanged materialized nodes can). Decide: eviction policy for the unmodified portion or documented ceiling. TDD the eviction with a modified-set preservation test.

### Task 2.4: Write-back strategy (decision)
Options: (a) full store rewrite on shutdown; (b) incremental merge — reindex changed documents, delete their nodes from the store (feasible: node IDs embed `uri_id`, so a `URI → node IDs` index table makes per-document deletion possible), and upsert new nodes in a redb write transaction; (c) keep rebuild-on-boot (status quo).
**Default: (c)** — it is correct and already understood. Choose (b) only if 2.2/1.7 data shows rebuild cost is unacceptable (minutes on large workspaces per boot).

### Task 2.5: `consume_document_changes` edge audit
Targeted tests: multi-file single transaction, file delete, file rename, require-graph change (new/removed dependency), edit that changes a name's last referrer (refcount → 0 → tombstone). Each is a small TDD cycle.

**Phase 2 exit:** edits on the disk path are fuzz- and soak-proven; persistence policy documented (rebuild-on-boot is a legitimate ship state).

---

## Phase 3 — Scale

### Task 3.1: Windows store build
Spawn-based build (reuses 1.4's spawn child) so Windows users can build stores, not just read them.

### Task 3.2: Incremental store updates
Only if 2.4 chose (b). Requires the `URI → node IDs` index table and careful refcount handling for interned names/strings across the store boundary (the in-memory tombstone work in `3242d9f` is the reference model).

### Task 3.3: Hot-node LRU
Only if 1.7's gate demanded it and it wasn't done there.

### Task 3.4: Stage-5 search indexes
`SEARCH_NAMES`/`DOCUMENT_URIS` are full scans today (correct, but O(n) per search). For very large workspaces: prefix/fuzzy projection tables built at store build time, refreshed with 3.2's incremental path.

---

## Decisions to surface (before/at phase starts)

1. **v1 overlay:** ship with live edits enabled (implemented + unit-tested) vs read-only v1. *Recommendation: ship enabled; Phase 2 earns the deep claim.*
2. **Fork guard vs always-spawn.** *Recommendation: fresh-graph fork + warm-Rust spawn (1.4).*
3. **Rebuild policy:** format-version decoupling (1.6) vs keep full-VERSION signature. *Recommendation: decouple.*
4. **Windows v1:** read-only (recommended) vs unsupported.
5. **Write-back:** (c) rebuild-on-boot is the default ship state (2.4).

## Per-phase verification (always)

```bash
cargo test -p rubydex && cargo test -p rubydex --features redb-store
cargo test -p rubydex-sys --features redb-store
bundle exec rake ruby_test
bundle exec rake lint   # clippy both configs + rubocop
```
Plus phase-specific: differential harness (1.1+), round-trip fuzz (1.2+), kill matrix (1.5+), benchmarks (1.7+), fuzzer/soak (2.1–2.2).
