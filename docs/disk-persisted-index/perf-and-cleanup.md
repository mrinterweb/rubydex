# Disk-Persisted Index Performance & Simplification Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox syntax (`- [ ]` unchecked, `- [x]` done) for tracking.

**Goal:** Eliminate the branch's remaining N+1 redb read transactions, random-key B-tree build overhead, duplicate warm-boot work, sequential multi-watch daemon stalls, and ~60 lines of localized duplication across the disk-persisted index implementation.

**Architecture:** Keep the existing redb table layout (`STORE_FORMAT_VERSION = 2` unchanged — no migration needed), layered `Graph` accessors, and single-daemon `rubydex-index-manager` design. Push bulk enumeration into single-transaction streaming scans on `RedbStore`, sort keys before B-tree insertion during build, deduplicate warm-boot manifest/hash checks in `lib/rubydex/graph.rb`, switch the manager loop to non-blocking per-watch drain with one shared idle sleep, and remove small duplicated helpers.

**Tech Stack:** Rust (`rubydex`, `rubydex-sys`, `rubydex-index-manager`, `redb 4.1`, `postcard 1.1`, `notify 9`), C FFI (`ext/rubydex`), Ruby 4 (`lib/rubydex`, Minitest). No new dependencies.

**Spec:** Branch review of `disk-persisted-index` against `main` (`fd58d6a`), targeting 1–5 concurrent editor/MCP sessions on a ~6,441-file Rails repo + gems (~300k declarations, ~500k constant references, ~1 GB store).

**Outcome (all tasks done, measured on this machine):**

| Task | Result |
|---|---|
| 1 | Declaration enumeration 3.02x, constant references 4.08x faster (891 files, 11,357 declarations, 8,225 references) |
| 2 | Kept: cold store build 5.26s -> 3.62s (31%), identical 1.08 GB store (27,726 files, 370,458 declarations) |
| 3 | One manifest parse and one digest per warm boot instead of two; 19.3ms off a 30,734-file boot (~4ms on a 6,441-file one) |
| 4 | Per-pass cost over 2 idle workspaces: 403.7ms -> 1.3us |
| 5 | Finding 5 measured **wrong** and reverted (see below); the other three applied |

**Task 2 note:** the baseline was taken with a throwaway Rust integration test timing `RedbStore::build` three times and taking the median, not the Ruby snippet below — the change is entirely in Rust, so measuring the Ruby wrapper would have added noise. Run it against `MEASURE_ROOT`, index with `index_files`, then build.

**Task 5 note:** finding 5 (empty-query `declaration_search` via `store_declaration_ids()`) was measured **10x slower** — 42.0ms keys-only against 4.1ms for the existing filtered parallel name scan, because walking the declarations table streams every large value page while the name table is small and sharded. The existing call is unchanged and carries a comment recording the measurement.

## Global Constraints

- Keep `STORE_FORMAT_VERSION = 2` and all existing redb table schemas unchanged; stores built before these changes must remain readable without invalidation.
- Never allow a redb error or panic to unwind across `extern "C"`: every new store scan must run through `catch_unwind` + `record_store_error` / `record_store_panic` (incrementing `store_errors` and returning cleanly on failure).
- Both feature configurations (`--features redb-store` and default without `redb-store`) must compile cleanly with `cargo clippy --workspace --all-targets -- -D warnings` and pass all tests. Put `#[allow(clippy::unused_self)]` and leading-underscore parameter names on every `#[cfg(not(feature = "redb-store"))]` stub method on `Graph`.
- Overlay entries must shadow store entries and tombstones must never be resurrected by any streaming scan.
- Preserve Linux, macOS, and Windows behavior; run all Ruby tests with `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null`.
- Run `bundle exec rake compile` after any Rust change before running Ruby tests — it rebuilds both `lib/rubydex/librubydex_sys.so` and the release `lib/rubydex/rubydex-index-manager` binary.
- Commit messages: write them to a temp file and pass `git commit -F /tmp/msg.txt` (do not use shell heredocs with backticks or quotes).
- Ruby 4.0 does not ship `benchmark` as a default gem; use `Process.clock_gettime(Process::CLOCK_MONOTONIC)` for timing. Follow RuboCop rules (blank line after `# frozen_string_literal: true`, double quotes, trailing commas in multiline calls) and run `bundle exec rubocop` on edited Ruby files.
- Execute tasks in order: Task 1 edits `rust/rubydex/src/query/cypher/schema.rs` before Task 5 simplifies the ID-merge helpers in the same file.
- Measure Task 2 (`RedbStore::build` sorted-key insertion) on a real multi-thousand-file store build before committing; keep only if build time or `.redb` file size measurably improves with no regression.

## Review Focus

- **Overlay shadowing & tombstones during single-transaction scans:** A live-edited declaration or deleted constant reference must appear once with its overlay state (or zero times if tombstoned) when enumerated via Cypher `scan_label`, `dead_code_candidates`, `Graph#declarations`, and `Graph#constant_references` (Task 1 tests).
- **Corrupt node mid-scan:** A damaged postcard payload inside `DECLARATIONS` or `CONSTANT_REFERENCES` during a streaming scan must increment `graph.store_error_count()` and never panic (Task 1 test).
- **Warm boot still checks freshness when `.hash` mismatches or is missing:** Removing duplicate `adopt_manifest` / `store_signature` calls in `index_workspace` must still trigger `refresh_if_stale` whenever the marker is stale or absent (Task 3 test).
- **Multi-workspace manager responsiveness:** Two or more registered watches must not serialize `N * debounce` sleeps when idle, and a burst on watch 2 must fire within one debounce window while watch 1 is quiet (Task 4 test).
- **No-`redb-store` build compatibility:** All new `Graph` streaming wrappers must provide `#[cfg(not(feature = "redb-store"))]` stubs so default builds stay green (Task 1 & Task 5 verification).

---

## Scope and File Map

| File | Responsibility | Tasks |
|---|---|---|
| `rust/rubydex/src/model/store.rs` | Single-txn streaming scans (`for_each_declaration`, `for_each_constant_reference_with_name`), sorted-key `build`, `#[cfg(test)]` `declaration_ids_matching`, drop redundant `sort_unstable` | 1, 2, 5 |
| `rust/rubydex/src/model/graph.rs` | `Graph` streaming wrappers (`for_each_store_declaration`, `for_each_store_constant_reference_with_name`) with overlay/tombstone filtering & no-feature stubs; `declaration_mut` delegates to `materialize_declaration` | 1, 5 |
| `rust/rubydex/src/query/cypher/schema.rs` | Stream declarations in `scan_label`; collapse 3 ID-merge helpers into `merge_ids` | 1, 5 |
| `rust/rubydex/src/query.rs` | Stream declarations in `dead_code_candidates`; use `store_declaration_ids()` for empty-query `declaration_search` | 1, 5 |
| `rust/rubydex-sys/src/graph_api.rs` | Stream in `rdx_graph_declarations_iter_new` and `rdx_graph_constant_references_iter_new` | 1 |
| `lib/rubydex/graph.rb` | Single manifest/signature pass in `index_workspace`, delete `session_state`, avoid `Array#-` in `surgical_changes` & `overlay_divergence` | 3 |
| `rust/rubydex-index-manager/src/fs_events.rs` | Single `Mutex<(Vec<PathBuf>, Instant)>`, merged `impl NotifySource`, `std::mem::take`, non-blocking `take_ready_batch` | 4 |
| `rust/rubydex-index-manager/src/main.rs` | Drain all watches non-blockingly per pass, sleep once when idle | 4 |

---

## Task 1: Single-transaction streaming scans for bulk store enumeration

**Covers:** Finding 1 (N+1 redb `ReadTransaction`s in Cypher `scan_label`, `dead_code_candidates`, `Graph#declarations`, and `Graph#constant_references`).

**Files:**
- Modify: `rust/rubydex/src/model/store.rs`
- Modify: `rust/rubydex/src/model/graph.rs`
- Modify: `rust/rubydex/src/query/cypher/schema.rs:488-506`
- Modify: `rust/rubydex/src/query.rs:918-956`
- Modify: `rust/rubydex-sys/src/graph_api.rs:765-785`, `:841-885`
- Test: `rust/rubydex/src/model/store.rs`, `rust/rubydex/tests/differential_store.rs`, `test/disk_index_enumerators_test.rb`, `test/disk_index_cypher_test.rb`

**Interfaces:**
- Produces on `RedbStore` (`#[cfg(feature = "redb-store")]`):
  - `pub fn for_each_declaration(&self, visit: &mut dyn FnMut(DeclarationId, Declaration)) -> Result<(), StoreError>`
  - `pub fn for_each_constant_reference_with_name(&self, visit: &mut dyn FnMut(ConstantReferenceId, ConstantReference, Option<NameRef>)) -> Result<(), StoreError>`
  - Both open a single `ReadTransaction` (the second opens `CONSTANT_REFERENCES` and `NAMES` in that same transaction) and decode each row with `postcard::from_bytes`, returning `StoreError::Corrupt` on decode failure.
- Produces on `Graph` (with `#[allow(clippy::unused_self)]` empty no-op stubs under `#[cfg(not(feature = "redb-store"))]`):
  - `pub fn for_each_store_declaration(&self, mut visit: impl FnMut(DeclarationId, Declaration))` — wraps `RedbStore::for_each_declaration` in `catch_unwind` + `record_store_error` / `record_store_panic`, skipping IDs present in `self.declarations` or tombstoned as `TombstoneKind::Declaration`.
  - `pub fn for_each_store_constant_reference_with_name(&self, mut visit: impl FnMut(ConstantReferenceId, ConstantReference, Option<&NameRef>))` — passes `Option<&NameRef>` (borrowed, zero clone for overlay names), skipping IDs present in `self.constant_references` or tombstoned as `TombstoneKind::ConstantReference`. Resolve the reference's `name_id = cref.name_id()` in this exact precedence order:
    1. `if let Some(overlay_name) = self.names.get(name_id)` -> `Some(overlay_name)`
    2. `else if self.is_tombstoned(TombstoneKind::Name, name_id.get())` -> `None`
    3. `else` -> `store_name.as_ref()`

- [x] **Step 1: Write failing Rust unit tests in `rust/rubydex/src/model/store.rs`.**
  - `for_each_store_declaration_skips_overlay_and_tombstones`: build a store with `Foo` and `Bar` (`index_files` + `Resolver::new(&mut graph).resolve()` + `RedbStore::build`), open via `Graph::with_store`, edit `Foo` in memory and delete `Bar`'s document + `Resolver::new(&mut graph).resolve()`, call `graph.for_each_store_declaration`, assert neither `Foo` nor `Bar` is yielded from the store layer.
  - `for_each_store_declaration_records_corrupt_node_without_panicking`: build a store, drop the `RedbStore`, open the file read-write via `Database::builder().open(&path)` (exact pattern at `rust/rubydex/src/model/store.rs:647`) and overwrite one `DECLARATIONS` entry with invalid postcard bytes (`&[0xff, 0xff, 0xff]`), reopen via `Graph::with_store(RedbStore::open(&path)?)`, call `graph.for_each_store_declaration`, assert `graph.store_error_count() > 0` and no panic occurs.
  - `for_each_store_constant_reference_with_name_prefers_overlay_name`: verify a store-backed constant reference whose `NameId` was updated or tombstoned in the overlay yields the overlay `NameRef` (or `None` when tombstoned) rather than the stale store `NameRef`.
- [x] **Step 2: Run the new tests to confirm they fail.**
  - Run: `cd rust && cargo test -p rubydex --features redb-store model::store::tests::for_each_store_`
  - Expected: compile failure (methods not yet defined).
- [x] **Step 3: Implement `RedbStore::for_each_declaration` and `RedbStore::for_each_constant_reference_with_name` in `rust/rubydex/src/model/store.rs`.**
  - Open one `ReadTransaction` per call; iterate `table.range(..=u64::MAX)?`; decode values with `postcard::from_bytes` mapping errors to `StoreError::Corrupt`.
  - In `for_each_constant_reference_with_name`, open both `CONSTANT_REFERENCES` and `NAMES` in the same `ReadTransaction` and decode `names_table.get(cref.name_id().get())?` inside the loop (mapping decode failure to `StoreError::Corrupt { table: "names", .. }`).
- [x] **Step 4: Implement `Graph::for_each_store_declaration` and `Graph::for_each_store_constant_reference_with_name` in `rust/rubydex/src/model/graph.rs`.**
  - Route through a small `visit_store` helper (same `catch_unwind` + `record_store_error` / `record_store_panic` pattern as `scan_store`, returning `()`).
  - Filter out overlay keys and tombstones before calling `visit`; apply the exact 3-step `NameRef` precedence from the Interfaces block above.
  - Add matching `#[cfg(not(feature = "redb-store"))]` `#[allow(clippy::unused_self)]` no-op stubs with `_visit`.
- [x] **Step 5: Switch the four N+1 callers onto the streaming helpers.**
  - `rust/rubydex/src/query/cypher/schema.rs:496-504` (`scan_label`): collect matching overlay declarations from `graph.declarations().iter()`, then append matching store declarations via `graph.for_each_store_declaration(|id, decl| { if declaration_matches_label(&decl, other) { results.push(NodeRef::Declaration(id)); } })`.
  - `rust/rubydex/src/query.rs:918-956` (`dead_code_candidates`): extract a small helper `fn is_dead_code_candidate(graph: &Graph, decl: &Declaration) -> bool`. Evaluate overlay `memory_ids: Vec<DeclarationId>` in `thread::scope` **only when `!memory_ids.is_empty()`** (do **not** `return Vec::new()` when `memory_ids.is_empty()` — on a store-backed graph with an empty overlay, `memory_ids` is empty and all results come from the store!). Then append matching store declarations via `graph.for_each_store_declaration(|id, decl| { if is_dead_code_candidate(graph, &decl) { results.push(id); } })`.
  - `rust/rubydex-sys/src/graph_api.rs:765-785` (`rdx_graph_declarations_iter_new`): append `entries.push(CDeclaration::from_declaration(id, &decl))` inside `graph.for_each_store_declaration`.
  - `rust/rubydex-sys/src/graph_api.rs:841-885` (`rdx_graph_constant_references_iter_new`): append `CConstantReference { id: *id, declaration_id }` inside `graph.for_each_store_constant_reference_with_name(|id, _cref, name_ref| ...)`.
- [x] **Step 6: Verify Rust & Ruby tests pass in both feature modes.**
  - Run: `cd rust && cargo test -p rubydex --features redb-store model::store::tests && cargo test -p rubydex --features redb-store --test differential_store && cargo test -p rubydex --lib query:: && cargo clippy --workspace --all-targets -- -D warnings`
  - Run: `bundle exec rake compile && GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest -e 'ARGV.each { |f| require File.expand_path(f) }' test/disk_index_enumerators_test.rb test/disk_index_cypher_test.rb`
  - Expected: all pass, 0 warnings.
- [x] **Step 7: Commit.**
  - Stage only the files modified in Task 1; commit with `git commit -F` (`Stream store-backed full-table scans in a single redb transaction`).

---

## Task 2: Sorted-key B-tree insertion in `RedbStore::build` (benchmark-gated)

**Covers:** Finding 2 (random-key B-tree inserts from `IdentityHashMap` in `RedbStore::build`).

**Files:**
- Modify: `rust/rubydex/src/model/store.rs:221-251`
- Test: `rust/rubydex/src/model/store.rs`, `rust/rubydex/tests/store_roundtrip.rs`

**Interfaces:**
- Preserves `pub fn RedbStore::build(path: &Path, graph: &Graph) -> Result<Self, StoreError>` signature and byte-for-byte table contents identically.

- [x] **Step 1: Record baseline build time and `.redb` file size.**
  - Compile first (`bundle exec rake compile`), then time `build_store` alone (separate from indexing/resolution) on this repo (or `$DEFAULT_BENCH_WORKSPACE` if set) using `Process.clock_gettime(Process::CLOCK_MONOTONIC)`:
    ```bash
    bundle exec ruby -Ilib -rrubydex -e '
      ws = ENV.fetch("DEFAULT_BENCH_WORKSPACE", Dir.pwd)
      out = "/tmp/rubydex-bench-before.redb"
      g = Rubydex::Graph.configure_for_workspace(ws)
      g.index_all(g.workspace_paths); g.resolve
      t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      g.build_store(out)
      dt = Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0
      puts format("build_store: %.3fs  size: %d bytes", dt, File.size(out))
    '
    ```
- [x] **Step 2: Sort keys before inserting in `RedbStore::build` (`rust/rubydex/src/model/store.rs:221-251`).**
  - Inside `write_map!`: collect `let mut entries: Vec<(u64, _)> = $map.iter().map(|(id, v)| (id.get(), v)).collect(); entries.sort_unstable_by_key(|(k, _)| *k);` then iterate `entries` and call `table.insert(id, bytes.as_slice())?`.
  - Apply the same ascending-key sort to `SEARCH_NAMES` and `DOCUMENT_URIS`.
- [x] **Step 3: Recompile and re-measure on the same corpus.**
  - Run `bundle exec rake compile` and re-run the exact Step 1 command writing to `/tmp/rubydex-bench-after.redb` (remove both `/tmp/rubydex-bench-*.redb` files afterwards).
  - Gate: keep the change only if `build_store` wall time or `.redb` file size measurably improves; otherwise revert `rust/rubydex/src/model/store.rs` and mark this task done as "measured, no win".
- [x] **Step 4: Run round-trip tests.**
  - Run: `cd rust && cargo test -p rubydex --features redb-store build_persists_full_graph && cargo test -p rubydex --features redb-store --test store_roundtrip`
  - Expected: all pass.
- [x] **Step 5: Commit (if kept).**
  - Stage `rust/rubydex/src/model/store.rs`; commit with `git commit -F` (`Insert redb store entries in ascending key order during build`).

---

## Task 3: Deduplicate warm-boot work and `Array#-` allocations in `lib/rubydex/graph.rb`

**Covers:** Finding 3 (duplicate manifest JSON parse + source-tree hashing on warm boot, unused `session_state` helper) and Finding 6 (four 6,441-element array + two temporary hash allocations in `surgical_changes` and `overlay_divergence`).

**Files:**
- Modify: `lib/rubydex/graph.rb:105-110`, `:291-306`, `:608-610`
- Test: `test/disk_index_session_refresh_test.rb`, `test/disk_index_surgical_refresh_test.rb`

**Interfaces:**
- Removes private single-caller helper `Graph#session_state`.
- Preserves `Graph#index_workspace`, `Graph#store_fresh?`, `Graph#surgical_changes`, `Graph#overlay_divergence` behavior and return types.

- [x] **Step 1: Write a failing test in `test/disk_index_session_refresh_test.rb`.**
  - Add `test_warm_boot_reads_manifest_and_hashes_sources_once` using the file's existing `with_workspace` and `session(dir)` helpers, spying with `define_singleton_method` + `super` so real behavior runs:
    ```ruby
    def test_warm_boot_reads_manifest_and_hashes_sources_once
      with_workspace do |dir, _rb|
        session(dir) # builds a fresh store + manifest + marker

        warm = Rubydex::Graph.configure_for_workspace(dir)
        manifest_reads = 0
        signature_calls = 0
        warm.define_singleton_method(:read_manifest) do |path|
          manifest_reads += 1
          super(path)
        end
        warm.define_singleton_method(:store_signature) do |*args|
          signature_calls += 1
          super(*args)
        end

        warm.index_workspace

        assert_equal(1, manifest_reads, "warm boot should parse the manifest once")
        assert_equal(1, signature_calls, "warm boot should hash the source scan once")

        # Removing the marker must still trigger refresh_if_stale on boot.
        FileUtils.rm_f("#{warm.send(:store_cache_path)}.hash")
        stale = Rubydex::Graph.configure_for_workspace(dir)
        refreshed = false
        stale.define_singleton_method(:refresh_if_stale) do
          refreshed = true
          super()
        end
        stale.index_workspace
        assert(refreshed, "a missing marker on warm boot must trigger refresh_if_stale")
      end
    end
    ```
- [x] **Step 2: Run the test to confirm it fails.**
  - Run: `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest test/disk_index_session_refresh_test.rb -n test_warm_boot_reads_manifest_and_hashes_sources_once`
  - Expected: FAIL (`manifest_reads` is 2 and `signature_calls` is 2).
- [x] **Step 3: Simplify `index_workspace`, delete `session_state`, and remove `Array#-` in `lib/rubydex/graph.rb`.**
  - In `index_workspace` inside `if adopt_manifest(cache)`:
    - Compute `scan = source_scan` and `expected_marker = store_signature(scan)` once.
    - Check `marker = "#{cache}.hash"; if File.exist?(marker) && File.read(marker) == expected_marker` -> set `@session_signature = "#{@head_sha}#{expected_marker}"`, `else refresh_if_stale`.
  - Delete `def session_state` (`lib/rubydex/graph.rb:608-610`) and its comment.
  - In `surgical_changes`: replace `@applied_files.keys - scan.keys` with `@applied_files.each_key.reject { |rel| scan.key?(rel) }`.
  - In `overlay_divergence`: replace `(@manifest_files.keys - scan.keys).size` with `@manifest_files.each_key.count { |rel| !scan.key?(rel) }`.
- [x] **Step 4: Run Ruby tests and RuboCop.**
  - Run: `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest -e 'ARGV.each { |f| require File.expand_path(f) }' test/disk_index_session_refresh_test.rb test/disk_index_surgical_refresh_test.rb test/disk_index_corrupt_store_test.rb && bundle exec rubocop lib/rubydex/graph.rb test/disk_index_session_refresh_test.rb`
  - Expected: all pass, 0 offenses.
- [x] **Step 5: Commit.**
  - Stage `lib/rubydex/graph.rb` and `test/disk_index_session_refresh_test.rb`; commit with `git commit -F` (`Avoid duplicate manifest parsing, source hashing, and array diffs in Graph`).

---

## Task 4: Non-blocking multi-watch drain and single-mutex state in `rubydex-index-manager`

**Covers:** Finding 4 (sequential `N * debounce` sleep per idle watch + `guard.clone(); guard.clear()`) and Finding 10 (two `Arc<Mutex<...>>` fields locked back-to-back and split `impl NotifySource` blocks).

**Files:**
- Modify: `rust/rubydex-index-manager/src/fs_events.rs`
- Modify: `rust/rubydex-index-manager/src/main.rs:70-96`
- Test: `rust/rubydex-index-manager/tests/fs_events.rs`, `test/disk_index_manager_session_test.rb`

**Interfaces:**
- Produces on `NotifySource`:
  - `pub fn take_ready_batch(&mut self) -> Vec<PathBuf>` — non-blocking; returns sorted, deduplicated changed paths if pending is non-empty and `last_event.elapsed() >= self.debounce`, otherwise `Vec::new()`.
  - Preserves `pub fn try_next_batch(&mut self, timeout: Duration) -> Vec<PathBuf>` as a polling loop over `take_ready_batch()` for existing callers/tests.
- Internal state of `EventCollector` / `NotifySource`: single `Arc<Mutex<(Vec<PathBuf>, Instant)>>` (merged into one `impl NotifySource` block); drains via `std::mem::take`.

- [x] **Step 1: Write failing tests in `rust/rubydex-index-manager/tests/fs_events.rs`.**
  - `take_ready_batch_is_non_blocking_when_idle_or_within_debounce`: assert `take_ready_batch()` returns empty in `< 10 ms` on an idle source, returns empty immediately after a file write while inside a `200 ms` debounce window, and returns the created file once the debounce elapses.
- [x] **Step 2: Run the new test to confirm it fails.**
  - Run: `cd rust && cargo test -p rubydex-index-manager --test fs_events take_ready_batch`
  - Expected: compile failure (`take_ready_batch` not defined).
- [x] **Step 3: Refactor `rust/rubydex-index-manager/src/fs_events.rs`.**
  - Replace `pending: Arc<Mutex<Vec<PathBuf>>>` and `last_event: Arc<Mutex<Instant>>` with `state: Arc<Mutex<(Vec<PathBuf>, Instant)>>`.
  - Merge the two consecutive `impl NotifySource` blocks into one.
  - Implement `pub fn take_ready_batch(&mut self) -> Vec<PathBuf>`: lock `self.state`, return `Vec::new()` if `state.0.is_empty() || state.1.elapsed() < self.debounce`, otherwise `let mut batch = std::mem::take(&mut state.0); drop(state);` **before** `batch.sort(); batch.dedup();` so the watcher thread is never blocked while sorting.
  - Implement `try_next_batch(&mut self, timeout: Duration)` by sleeping `20 ms` and calling `self.take_ready_batch()` until non-empty or `started.elapsed() >= timeout`.
- [x] **Step 4: Update the event loop in `rust/rubydex-index-manager/src/main.rs:70-96`.**
  - Replace `watch.source.try_next_batch(debounce)` with `watch.source.take_ready_batch()`.
  - Track `let mut any_work = false;` across the `for watch in &mut watches` loop (true if `!batch.is_empty()` or `started.is_some()`), and call `std::thread::sleep(Duration::from_millis(20))` once at the end of the loop when `!any_work`.
- [x] **Step 5: Run Rust manager tests, rebuild the release manager binary, and run the Ruby session test.**
  - Run: `cd rust && cargo test -p rubydex-index-manager && cargo clippy -p rubydex-index-manager --all-targets -- -D warnings`
  - Run: `bundle exec rake compile && GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest test/disk_index_manager_session_test.rb` (do **not** skip `bundle exec rake compile` — `test/disk_index_manager_session_test.rb` spawns `lib/rubydex/rubydex-index-manager`, which `rake compile` copies from `rust/target/release/`).
  - Expected: all pass, 0 warnings.
- [x] **Step 6: Commit.**
  - Stage `rust/rubydex-index-manager/src/fs_events.rs`, `rust/rubydex-index-manager/src/main.rs`, and `rust/rubydex-index-manager/tests/fs_events.rs`; commit with `git commit -F` (`Poll manager watches non-blockingly and unify NotifySource mutex`).

---

## Task 5: Rust query & store lean cleanups

**Covers:** Finding 5 (empty-query `declaration_search`), Finding 7 (3 copy-pasted ID-merge helpers in `schema.rs`), Finding 8 (`Graph::declaration_mut` duplicating `materialize_declaration`), and Finding 9 (`#[cfg(test)]` `declaration_ids_matching` + removing redundant `sort_unstable`).

**Files:**
- Modify: `rust/rubydex/src/query.rs:53-56`
- Modify: `rust/rubydex/src/query/cypher/schema.rs:426-462`
- Modify: `rust/rubydex/src/model/graph.rs:797-807`
- Modify: `rust/rubydex/src/model/store.rs:463-516`

**Interfaces:**
- Replaces private `document_node_ids`, `definition_node_ids`, `declaration_node_ids` in `schema.rs` with one private generic `fn merge_ids<K: Copy + Eq + std::hash::Hash, V>(overlay: &IdentityHashMap<K, V>, store_ids: Vec<K>) -> Vec<K>`.
- Gates `RedbStore::declaration_ids_matching` with `#[cfg(test)]` (production uses `declaration_ids_matching_parallel`).

- [x] **Step 1: Add a sorted-order assertion to `declaration_ids_matching_parallel` test in `rust/rubydex/src/model/store.rs`.**
  - In the existing parallel-vs-serial comparison test (`store.rs:850-865`), assert `parallel.windows(2).all(|pair| pair[0] < pair[1])` so removing `ids.sort_unstable()` is explicitly pinned.
- [x] **Step 2: Apply the four localized cleanups.**
  - ~~`rust/rubydex/src/query.rs:53-56`: replace the filtered name scan with `store_declaration_ids()`.~~ **Reverted: measured 10x slower.** See the Task 5 note above.
  - `rust/rubydex/src/query/cypher/schema.rs:426-462`: replace `document_node_ids`, `definition_node_ids`, and `declaration_node_ids` with `merge_ids(graph.documents(), graph.store_document_ids())` (and equivalents for definitions and declarations).
  - `rust/rubydex/src/model/graph.rs:797-807`: replace the duplicated body of `declaration_mut` with `#[cfg(feature = "redb-store")] self.materialize_declaration(id);` followed by `self.declarations.get_mut(&id)`.
  - `rust/rubydex/src/model/store.rs:463-516`: add `#[cfg(test)]` to `declaration_ids_matching`, update its doc comment, and remove the redundant `ids.sort_unstable()` from `declaration_ids_matching_parallel` (add a 1-line comment noting disjoint ascending B-tree shards preserve strict key order).
- [x] **Step 3: Verify Rust tests and clippy in both feature modes.**
  - Run: `cd rust && cargo test --workspace --features rubydex/redb-store,rubydex-sys/redb-store && cargo test --workspace && cargo clippy --workspace --all-targets --features rubydex/redb-store,rubydex-sys/redb-store -- -D warnings && cargo clippy --workspace --all-targets -- -D warnings`
  - Expected: all pass, 0 warnings.
- [x] **Step 4: Commit.**
  - Stage the 4 modified Rust files; commit with `git commit -F` (`Simplify store ID merges, declaration_mut, empty search, and parallel scan`).

---

## Task 6: Final branch verification

- [x] **Step 1: Rebuild native extension & manager binary.**
  - Run: `bundle exec rake compile`
- [x] **Step 2: Run linters (RuboCop, clippy, rustfmt).**
  - Run: `bundle exec rake lint`
  - Expected: 0 offenses, 0 warnings, clean formatting.
- [x] **Step 3: Run disk-index & MCP Ruby test suites with isolated git config.**
  - Run: `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest -e 'ARGV.each { |f| require File.expand_path(f) }' test/disk_index_*.rb test/index_manager_registry_test.rb test/valgrind_selection_test.rb test/integration/mcp_server_test.rb`
  - Expected: 0 failures, 0 errors.
- [x] **Step 4: Check `git status` and diff stats against `main`.**
  - Run: `git status --short && git log --oneline origin/disk-persisted-index..HEAD`
