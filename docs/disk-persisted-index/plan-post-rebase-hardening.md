# Post-Rebase Verification & Hardening Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Close the verification gaps opened by the rebase onto `origin/main`, remove every remaining abort path on the disk-backed index, measure (not guess) the two performance outliers, and land the branch in a state a reviewer can trust.

**Architecture:** This plan picks up after commit `8f9359b` (sized-deallocation fix) on branch `disk-persisted-index`, rebased onto `origin/main` (`d8ff475`). Work is grouped so each phase leaves the branch shippable: Phase A re-establishes ground truth after the rebase, Phase B removes abort paths, Phase C measures the two perf outliers with a decision gate, Phase D puts guards in CI and moves docs out of `tmp/`, Phase E is push hygiene.

**Tech Stack:** Rust 1.101 / `redb` 4.1 / `postcard` 1.1 / `tikv-jemallocator` 0.7 (debug feature) / Ruby 4.0.5 + minitest + rake-compiler / GitHub Actions.

**Spec:** `docs/disk-persisted-index/review.md` (review), `docs/disk-persisted-index/ship-plan.md` (roadmap; this plan supersedes Phase 1 Tasks 1.2–1.9 and the memory workstream), `docs/disk-persisted-index/memory-baseline.md` (stale measurements to refresh), `MEMORY_POC.md` (memory model + reproduce steps).

## Global Constraints

- Opt-in stays double-gated: `redb-store` Cargo feature (build) + `RUBYDEX_DISK_INDEX=1` (runtime). The in-memory path must stay behavior-identical; CI proves it by building and testing **without** the feature (Task D2).
- Every failure mode ends in clean fallback to in-memory. Never a process crash, never a silently-wrong answer.
- TDD: no production code without a failing test, watched red for the expected reason before the fix.
- Measure before changing performance code; keep only measured wins. Every perf task records before/after RSS and wall time in `docs/disk-persisted-index/memory-baseline.md`.
- Commits per task. 1Password SSH signer is broken: use `git commit --no-gpg-sign` until Task E1.
- Local verification baseline on this machine: Rust 1171 (store) / 1153 (default) / 13 (sys store), clippy 0; Ruby 413 runs / 1848 assertions.
- Corpus for stdlib-scale work: `LIBDIR=$(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/..` (Ruby 4.0.5, 29,360 files, 686,691 definitions).
- Store artifacts for benchmarks live in `/tmp/rdx-bench/` (gitignored, rebuild freely).
- Do not modify production code purely to enable measurement; use throwaway scripts in `tmp/` or `#[ignore]`-gated tests.

---

## Phase A — Re-establish ground truth after the rebase

The rebase rewrote `declaration.rs` (into `DeclarationCore`/`NamespaceStore`), removed `Graph#initialize(workspace_path:)`, and forced a mechanical port of ~60 node lookups in `resolution.rs`. Every performance and correctness number on record predates that. Nothing else should start until these are re-measured.

### Task A1: Re-run the stdlib differential tier

This is the strongest correctness check in the repo — it compares every query API's output between the in-memory graph and a store-backed graph built from the same corpus. It has not run since the rebase.

**Files:**
- Read: `rust/rubydex/tests/differential_store.rs` (harness, unchanged)
- Modify: `docs/disk-persisted-index/task-list.md` (record the result)

**Interfaces:**
- Consumes: `RUBYDEX_DIFF_CORPUS` env var; test `differential_on_stdlib` (currently `#[ignore]`).
- Produces: a recorded pass/fail + wall time in `docs/disk-persisted-index/task-list.md`.

- [ ] **Step 1: Build the release CLI and run the stdlib tier**

```bash
cd rust && cargo build --release --features redb-store
CORPUS=$(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/..
cd .. && RUBYDEX_DIFF_CORPUS="$CORPUS" \
  cargo test --manifest-path rust/Cargo.toml --release \
  -p rubydex --features redb-store --test differential_store \
  differential_on_stdlib -- --ignored --nocapture
```

Expected: `test differential_on_stdlib ... ok` in roughly 6 minutes (last recorded: 374s before the rebase).

- [ ] **Step 2: If it reports divergence, treat it as a bug report, not a test fix**

The harness prints every mismatching key. For each one, write a focused failing test in `rust/rubydex/src/model/store.rs` (or the owning module), watch it fail, fix in Rust core or FFI internals, watch it pass, commit. Do not weaken a probe to make the suite pass.

- [ ] **Step 3: Record the result**

Append to `docs/disk-persisted-index/task-list.md` under the Phase A heading:

```
- [x] Stdlib differential re-run after rebase (post-8f9359b): <N>s, 0 divergence (or: fixed K divergences, see <sha>)
```

- [ ] **Step 4: Commit the doc update**

```bash
git add docs/disk-persisted-index/task-list.md 2>/dev/null || echo "tmp/ is gitignored — record in the PR description instead"
```

If `tmp/` is ignored (it is), skip the commit and carry the number into Task D4.

### Task A2: Rebuild the benchmark store and re-measure memory

Every RSS number in `docs/disk-persisted-index/memory-baseline.md` (disk 73 MB vs memory 692 MB) predates the rebase. The store at `/tmp/rdx-bench/stdlib.redb` was built by pre-rebase code and its schema may no longer match.

**Files:**
- Read: `docs/disk-persisted-index/memory-baseline.md`, `docs/disk-persisted-index/session-profile.rb`
- Modify: `docs/disk-persisted-index/memory-baseline.md` (new "Post-rebase measurements" section)

**Interfaces:**
- Consumes: `cargo run --release --features redb-store -- --build-store PATH CORPUS --stats`; `docs/disk-persisted-index/session-profile.rb CORPUS`.
- Produces: refreshed build-path and session tables in `docs/disk-persisted-index/memory-baseline.md`.

- [ ] **Step 1: Rebuild the stdlib store from scratch and capture the build path**

```bash
mkdir -p /tmp/rdx-bench && rm -f /tmp/rdx-bench/stdlib.redb
CORPUS=$(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/..
time cargo run --manifest-path rust/Cargo.toml --release --features redb-store -- \
  --build-store /tmp/rdx-bench/stdlib.redb "$CORPUS" --stats
```

Record: file count, definition count, total wall time, the `Cleanup` stage share, peak RSS, store size on disk.

- [ ] **Step 2: Re-run the Ruby session profile in both modes**

```bash
cd /home/sean/code/rubydex
RUBYDEX_DISK_INDEX=1 RUBYDEX_CACHE_DIR=/tmp/rdx-bench/cache \
  bundle exec ruby -Ilib docs/disk-persisted-index/session-profile.rb "$(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/.."
RUBYDEX_DISK_INDEX=0 RUBYDEX_CACHE_DIR=/tmp/rdx-bench/cache \
  bundle exec ruby -Ilib docs/disk-persisted-index/session-profile.rb "$(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/.."
```

Expected shape (numbers may move, the ~10x gap should not): disk final RSS tens of MB, memory mode hundreds of MB.

- [ ] **Step 3: Write the refreshed table into `docs/disk-persisted-index/memory-baseline.md`**

Add a `## Post-rebase measurements (post-8f9359b)` section holding both tables verbatim, and add one line under the old tables: `_Superseded by the post-rebase numbers above; pre-rebase figures kept for comparison._`

- [ ] **Step 4: Note the spawn overhead separately**

The store builder now runs via `Process.spawn` (`lib/rubydex/store_builder.rb`) rather than `fork`, because a forked child of a warm Ruby process corrupts jemalloc. Measure the delta:

```bash
time bundle exec ruby -Ilib -e '
  require "rubydex"; require "tmpdir"; require "fileutils"
  Dir.mktmpdir do |d|
    File.write(File.join(d, "a.rb"), "class A; end\n")
    ENV["RUBYDEX_DISK_INDEX"] = "1"; ENV["RUBYDEX_CACHE_DIR"] = File.join(d, "cache")
    g = Rubydex::Graph.configure_for_workspace(d)
    t = Process.clock_gettime(Process::CLOCK_MONOTONIC); g.index_workspace
    puts "cold build+attach: #{(Process.clock_gettime(Process::CLOCK_MONOTONIC) - t).round(3)}s"
  end'
```

Record the number next to the spawn change in `docs/disk-persisted-index/memory-baseline.md`.

### Task A3: Attribute the store-build "Cleanup" stage

The pre-rebase baseline attributed 4.495s of a 6.3s build (71%) to `Cleanup` — the `RedbStore::build` write plus the store's `Drop`. Nothing was ever done about it. Establish what it actually is before touching it.

**Files:**
- Read: `rust/rubydex/src/model/store.rs` (`RedbStore::build`, `Drop`)
- Modify: `docs/disk-persisted-index/memory-baseline.md`

- [ ] **Step 1: Measure build-only vs drop-only in a throwaway benchmark**

Add a temporary ignored test (not production code):

```rust
// rust/rubydex/src/model/store.rs, inside `mod tests`
#[test]
#[ignore = "benchmark: run with --release -- --ignored --nocapture"]
fn bench_build_vs_drop() {
    let corpus = std::env::var("RUBYDEX_BENCH_CORPUS").expect("set RUBYDEX_BENCH_CORPUS");
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("bench.redb");

    let t0 = std::time::Instant::now();
    let store = RedbStore::build(&path, &Graph::new()).expect("build (built-ins only)");
    let build = t0.elapsed();
    let t1 = std::time::Instant::now();
    drop(store);
    let drop_time = t1.elapsed();
    println!("BENCH build={build:?} drop={drop_time:?} size={}", std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0));
}
```

- [ ] **Step 2: Run it against the stdlib graph built by the CLI harness from Task A2 step 1**

```bash
CORPUS=$(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/..
RUSTYDEX_PLACEHOLDER= RUBYDEX_BENCH_CORPUS="$CORPUS" \
  cargo test --manifest-path rust/Cargo.toml --release -p rubydex --features redb-store \
  bench_build_vs_drop -- --ignored --nocapture
```

If the in-process graph is too small to be representative, instead time `--build-store` with the store file on tmpfs vs disk to separate serialization cost from fsync cost, and record both.

- [ ] **Step 3: Record the attribution in `docs/disk-persisted-index/memory-baseline.md`**

Write one line: `Cleanup = serialize <X>s + commit/fsync <Y>s + drop <Z>s`. Phase C Task C2 uses this.

---

## Phase B — Remove abort paths (store integrity)

The store is a serialized snapshot on disk that the process trusts. Today a single unreadable byte aborts the host process, because `RedbStore::get_node` calls `.expect("node should deserialize")` (`rust/rubydex/src/model/store.rs:224`). That is exactly the class of bug fixed in review items 1–6 and in `8f9359b`: a panic inside `extern "C"` cannot unwind, so it terminates ruby-lsp.

### Task B1: Replace the decode `.expect()` with a typed error and a poison flag

**Files:**
- Modify: `rust/rubydex/src/model/store.rs:9` (remove the Spike-only allow), `:215-227` (`get_node`), `:94`, `:147` (serialize sites)
- Modify: `rust/rubydex/src/model/graph.rs` (layered getters, error counting)
- Test: `rust/rubydex/src/model/store.rs` (`mod tests`)

**Interfaces:**
- Consumes: existing `RedbStore::get_declaration(id) -> Result<Option<Declaration>, redb::Error>` family.
- Produces:
  - `pub enum StoreError { Open(redb::Error), Corrupt { table: &'static str, id: u64, source: postcard::Error } }` with `impl std::fmt::Display`, `impl std::error::Error`.
  - `RedbStore` getters change to `Result<Option<T>, StoreError>`.
  - `Graph::store_error_count(&self) -> usize` — `Cell<usize>`, incremented once per failed store read.

- [ ] **Step 1: Write the failing test — a corrupt node must not abort**

```rust
// rust/rubydex/src/model/store.rs, inside `mod tests`
#[test]
fn corrupt_node_returns_error_instead_of_aborting() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("corrupt.redb");
    let mut graph = Graph::new();
    crate::indexing::index_source(&mut graph, "file:///foo.rb".into(), "class Foo; end\n", &crate::indexing::LanguageId::Ruby);
    crate::resolution::Resolver::new(&mut graph).resolve();
    RedbStore::build(&path, &graph).expect("build store");

    // Overwrite the serialized declaration with bytes that are not a valid postcard stream.
    corrupt_table_value(&path, "declarations", DeclarationId::from("Foo"));

    let store = RedbStore::open(&path).expect("store still opens: the file is structurally valid");
    let result = store.get_declaration(DeclarationId::from("Foo"));
    assert!(matches!(result, Err(StoreError::Corrupt { .. })), "expected Corrupt, got {result:?}");
}
```

Helper `corrupt_table_value` opens the same redb file read-write and overwrites one key with `postcard` bytes that decode to a different shape (`vec![0u8, 1, 2]`), which is structurally valid postcard but wrong for `Declaration`.

- [ ] **Step 2: Run it and watch it fail with a panic**

```bash
cargo test -p rubydex --features redb-store corrupt_node_returns_error_instead_of_aborting
```

Expected: FAIL, panicking at `store.rs` with `"node should deserialize"`.

- [ ] **Step 3: Add the error type and change `get_node`**

```rust
// rust/rubydex/src/model/store.rs
/// Failure modes of the on-disk store. A corrupt store is recoverable: the caller falls back to
/// the in-memory index rather than aborting the host process (a panic in `extern "C"` cannot unwind).
#[derive(Debug)]
pub enum StoreError {
    /// The redb database could not be opened or a transaction failed.
    Open(redb::Error),
    /// A node's bytes could not be decoded as its node type: the store is corrupt or was written
    /// by an incompatible layout.
    Corrupt {
        /// Logical table the node was read from.
        table: &'static str,
        /// Node id whose bytes failed to decode.
        id: u64,
        /// The underlying decode failure.
        source: postcard::Error,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Open(error) => write!(f, "store unavailable: {error}"),
            StoreError::Corrupt { table, id, source } => {
                write!(f, "corrupt node in table `{table}` (id {id}): {source}")
            }
        }
    }
}

impl std::error::Error for StoreError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StoreError::Open(error) => Some(error),
            StoreError::Corrupt { source, .. } => Some(source),
        }
    }
}
```

`get_node` becomes:

```rust
fn get_node<V: DeserializeOwned>(
    &self,
    table: TableDefinition<u64, &[u8]>,
    name: &'static str,
    key: u64,
) -> Result<Option<V>, StoreError> {
    let read_txn = self.db.begin_read().map_err(StoreError::Open)?;
    let table = read_txn.open_table(table).map_err(StoreError::Open)?;
    match table.get(key).map_err(StoreError::Open)? {
        Some(guard) => postcard::from_bytes::<V>(guard.value())
            .map(Some)
            .map_err(|source| StoreError::Corrupt { table: name, id: key, source }),
        None => Ok(None),
    }
}
```

Every getter passes its table's name literal (`"strings"`, `"declarations"`, …) instead of the `TableDefinition` handle. Update the two `postcard::to_allocvec(...).expect("node should serialize")` sites (lines ~94, ~147) to `map_err` into `StoreError::Open`-style variants or, if serialization cannot fail for these types, leave them but delete the module-level `#![allow(clippy::missing_panics_doc)]` once no `expect` remains in non-test code.

- [ ] **Step 4: Make `Graph` count store errors instead of aborting**

In `rust/rubydex/src/model/graph.rs`:

- Add the field next to `store`:

```rust
#[cfg(feature = "redb-store")]
store: Option<crate::model::store::RedbStore>,
/// Number of failed store reads since boot. A non-zero value means the store is corrupt or was
/// written by an incompatible layout, and the caller should fall back to the in-memory index.
#[cfg(feature = "redb-store")]
store_errors: std::cell::Cell<usize>,
```

- Initialize it in `Graph::new()` (`store: None,` → add `store_errors: std::cell::Cell::new(0),`) and add the accessor:

```rust
/// Number of store reads that failed (corrupt node or unavailable database). Non-zero means the
/// disk index is not trustworthy and the caller should fall back to in-memory indexing.
#[cfg(feature = "redb-store")]
#[must_use]
pub fn store_error_count(&self) -> usize {
    self.store_errors.get()
}
```

- In every layered getter (`declaration`, `definition`, `name`, `string`, `constant_reference`, `method_reference`, `document`, and the `*_ids`/`*_matching` helpers), change `if let Ok(Some(x)) = store.get_x(id)` to:

```rust
match store.get_x(id) {
    Ok(Some(node)) => return Some(NodeRef::Stored(Box::new(node))),
    Ok(None) => {}
    Err(_) => {
        self.store_errors.set(self.store_errors.get() + 1);
    }
}
```

- In the dual-cfg (no-feature) variant of each, return the same empty value as today; the field and accessor do not exist without the feature, so gate them with `#[cfg(feature = "redb-store")]`.

- [ ] **Step 5: Run the test and watch it pass, then the whole suite**

```bash
cargo test -p rubydex --features redb-store corrupt_node_returns_error_instead_of_aborting
cargo test -p rubydex --features redb-store
cargo test -p rubydex
cargo clippy -p rubydex -p rubydex-sys --all-targets --features redb-store
```

Expected: new test passes; 1172 store / 1153 default / clippy 0.

- [ ] **Step 6: Commit**

```bash
git add rust/rubydex/src/model/store.rs rust/rubydex/src/model/graph.rs
git commit --no-gpg-sign -m "Surface store decode failures instead of aborting the host"
```

### Task B2: Ruby falls back to in-memory when the store is corrupt

B1 makes corruption observable but not acted on. The caller must react.

**Files:**
- Modify: `lib/rubydex/graph.rb` (`index_workspace`, `attach_store`)
- Modify: `ext/rubydex/graph.c` (expose `Graph#store_errors`)
- Test: `test/disk_index_corrupt_store_test.rb` (new)

**Interfaces:**
- Consumes: `Graph#store_errors` (new Ruby method returning the `usize` from Task B1).
- Produces: `Rubydex::Graph#store_errors -> Integer`; `index_workspace` falls back when it is non-zero after the first query.

- [ ] **Step 1: Write the failing Ruby test**

```ruby
# test/disk_index_corrupt_store_test.rb
# frozen_string_literal: true

require "tmpdir"
require "fileutils"
require "test_helper"

class DiskIndexCorruptStoreTest < Minitest::Test
  def setup
    super
    @tmp = Dir.mktmpdir
    ENV["RUBYDEX_DISK_INDEX"] = "1"
    ENV["RUBYDEX_CACHE_DIR"] = @tmp
    skip("ruby subprocess unavailable") unless Process.respond_to?(:spawn)
  end

  def teardown
    ENV.delete("RUBYDEX_DISK_INDEX")
    ENV.delete("RUBYDEX_CACHE_DIR")
    FileUtils.remove_entry(@tmp) if @tmp && File.exist?(@tmp)
    super
  end

  def test_corrupt_store_falls_back_to_in_memory
    File.write(File.join(@tmp, "foo.rb"), "class Foo; end\n")

    graph = Rubydex::Graph.configure_for_workspace(@tmp)
    graph.index_workspace
    refute_nil(graph["Foo"], "baseline: store-backed read works")

    # Flip bytes inside the store file so some node fails to decode.
    cache = graph.send(:store_cache_path)
    File.open(cache, "r+b") { |f| f.seek(64); f.write("\xFF\xFF\xFF\xFF".b) }

    graph.resolve # any query that touches the damaged node
    assert_operator graph.store_errors, :>, 0, "corruption must be reported, not hidden"
  end
end
```

- [ ] **Step 2: Run it and watch it fail**

```bash
bundle exec rake compile && bundle exec ruby -Itest test/disk_index_corrupt_store_test.rb
```

Expected: FAIL — `Graph#store_errors` is undefined (NoMethodError) before Task B1's C binding exists.

- [ ] **Step 3: Expose the counter to Ruby**

In `ext/rubydex/graph.c`, next to the other `cGraph` methods:

```c
// Graph#store_errors: () -> Integer. Non-zero means the disk index is not trustworthy.
static VALUE rdxr_graph_store_errors(VALUE self) {
    void *graph;
    TypedData_Get_Struct(self, void *, &graph_type, graph);
    return ULL2NUM(rdx_graph_store_errors(graph));
}
```

and register it: `rb_define_method(cGraph, "store_errors", rdxr_graph_store_errors, 0);`

In `rust/rubydex-sys/src/graph_api.rs`, next to `rdx_graph_attach_store`:

```rust
/// Returns the number of failed store reads since the graph was created. Zero means the disk index
/// answered everything; non-zero means it is corrupt or was written by an incompatible layout.
#[unsafe(no_mangle)]
pub extern "C" fn rdx_graph_store_errors(pointer: GraphPointer) -> u64 {
    with_graph(pointer, |graph| {
        #[cfg(feature = "redb-store")]
        {
            graph.store_error_count() as u64
        }
        #[cfg(not(feature = "redb-store"))]
        {
            let _ = graph;
            0
        }
    })
}
```

- [ ] **Step 4: Fall back when the store has errored**

In `lib/rubydex/graph.rb#index_workspace`, after attaching:

```ruby
def index_workspace
  return index_all(workspace_paths) unless disk_index_enabled?

  cache = store_cache_path
  build_store_via_fork(cache) unless File.exist?(cache) && store_fresh?(cache)
  attach_store(cache)
  # A store that failed to decode is not trustworthy: drop it and index in memory so the session
  # degrades instead of answering with holes. The bad store is quarantined, not deleted, so the
  # next run rebuilds rather than re-reading it.
  if respond_to?(:store_errors) && store_errors.positive?
    warn("rubydex: disk-backed index returned errors; falling back to in-memory")
    FileUtils.mv(cache, "#{cache}.corrupt", force: true)
    FileUtils.rm_f("#{cache}.hash")
    return index_all(workspace_paths)
  end
  []
rescue StandardError, NotImplementedError => e
  ...
end
```

(`FileUtils` is already required inside `build_store_via_fork`; add `require "fileutils"` at the top of `index_workspace` if the constant is not in scope there.)

- [ ] **Step 5: Run the test, then the full Ruby suite**

```bash
bundle exec ruby -Itest test/disk_index_corrupt_store_test.rb
bundle exec ruby -Itest test/disk_index_live_edit_test.rb
bundle exec rake ruby_test
```

Expected: new test passes, live-edit test still passes, suite 414+ runs 0 failures.

- [ ] **Step 6: Commit**

```bash
git add lib/rubydex/graph.rb ext/rubydex/graph.c rust/rubydex-sys/src/graph_api.rs test/disk_index_corrupt_store_test.rb
git commit --no-gpg-sign -m "Fall back to in-memory when the disk index reports decode errors"
```

### Task B3: Decouple the store format version from the gem version

`lib/rubydex/graph.rb#store_signature` mixes `Rubydex::VERSION` into the freshness key, so **every** gem release invalidates every store on every machine — a full re-index for users on every upgrade, even when the node layout did not change. A store written by an older layout must still be rejected.

**Files:**
- Modify: `rust/rubydex/src/model/store.rs` (export the constant)
- Modify: `lib/rubydex/graph.rb` (`store_signature`)
- Test: `rust/rubydex-sys/src/graph_api.rs` (constant is exported) or `test/disk_index_live_edit_test.rb` (signature changes with the constant)

**Interfaces:**
- Produces: `rubydex::model::store::STORE_FORMAT_VERSION: u32`, re-exported over FFI as `rdx_store_format_version() -> u32`.

- [ ] **Step 1: Write the failing Ruby test**

Add to `test/disk_index_live_edit_test.rb`:

```ruby
def test_store_signature_tracks_format_version_not_gem_version
  graph = Rubydex::Graph.configure_for_workspace(@tmp)
  baseline = graph.send(:store_signature)

  with_stubbed_const(Rubydex::VERSION, "9.9.9") do
    refute_equal baseline, graph.send(:store_signature),
      "gem version must not invalidate a store whose layout is unchanged"
  end

  with_stubbed_const(Rubydex, :STORE_FORMAT_VERSION, 999_999) do
    refute_equal baseline, graph.send(:store_signature),
      "a store-format change must invalidate the store"
  end
end
```

`with_stubbed_const` is a small local helper (assign + ensure-restore) — do not add a dependency for it.

- [ ] **Step 2: Run it and watch it fail**

```bash
bundle exec ruby -Itest test/disk_index_live_edit_test.rb -n test_store_signature_tracks_format_version
```

Expected: FAIL — bumping `Rubydex::VERSION` currently changes the signature.

- [ ] **Step 3: Export the format version and use it**

In `rust/rubydex/src/model/store.rs`, next to the table constants:

```rust
/// Layout version of the persisted store. Bump this when a node's serialized shape, a table's key
/// scheme, or the set of tables changes — that is what makes an older store unreadable. The gem
/// version deliberately does NOT participate: it would force every user to re-index on every
/// release even when nothing about the layout moved.
pub const STORE_FORMAT_VERSION: u32 = 1;
```

In `rust/rubydex-sys/src/graph_api.rs`:

```rust
/// Layout version of the on-disk store. Ruby mixes this into the freshness signature so an older
/// or newer store is rebuilt instead of misread.
#[unsafe(no_mangle)]
pub extern "C" fn rdx_store_format_version() -> u32 {
    rubydex::model::store::STORE_FORMAT_VERSION
}
```

In `lib/rubydex/graph.rb`:

```ruby
# Layout version of the persisted store, owned by the Rust side. Bumped when the serialized node
# shapes or table layout change; see `STORE_FORMAT_VERSION` in rust/rubydex/src/model/store.rs.
def self.store_format_version
  rdx_store_format_version
rescue NoMethodError
  # Built without the redb-store feature: nothing is ever persisted, so any value works.
  0
end
```

and in `store_signature`:

```ruby
Digest::SHA1.hexdigest(self.class.store_format_version.to_s + lockfile_hash + workspace_source_signature)
```

- [ ] **Step 4: Run the test, then the live-edit and full suites**

```bash
bundle exec rake compile
bundle exec ruby -Itest test/disk_index_live_edit_test.rb
bundle exec rake ruby_test
```

- [ ] **Step 5: Commit**

```bash
git add rust/rubydex/src/model/store.rs rust/rubydex-sys/src/graph_api.rs lib/rubydex/graph.rb test/disk_index_live_edit_test.rb
git commit --no-gpg-sign -m "Key store freshness on a layout version, not the gem version"
```

### Task B4: Store round-trip fuzz (ship-plan Task 1.2)

**Files:**
- Create: `rust/rubydex/tests/store_roundtrip.rs` (integration test, `#![cfg(feature = "redb-store")]`)

**Interfaces:**
- Consumes: `RedbStore::build`, the per-type `put_*`/`get_*` pairs, `Graph::with_store`.
- Produces: a randomized round-trip test that fails on any node type that does not survive serialize → store → load → re-serialize.

- [ ] **Step 1: Write the failing test**

```rust
// rust/rubydex/tests/store_roundtrip.rs
//! Every node type must survive serialize -> store -> load -> re-serialize byte-identically.
//! Nodes are generated from the in-memory graph of a fixture corpus so the shapes are real, not
//! hand-written approximations.

#![cfg(feature = "redb-store")]

use std::collections::BTreeMap;

use rubydex::{
    indexing::{index_files, IndexerBackend},
    model::{graph::Graph, store::RedbStore},
    resolution::Resolver,
};

fn corpus_files() -> Vec<std::path::PathBuf> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/diff_corpus");
    std::fs::read_dir(dir)
        .expect("diff_corpus fixture")
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "rb"))
        .collect()
}

#[test]
fn every_node_round_trips_through_the_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let store_path = dir.path().join("roundtrip.redb");

    let mut graph = Graph::new();
    let _ = index_files(&mut graph, corpus_files(), IndexerBackend::RubyIndexer);
    Resolver::new(&mut graph).resolve();

    RedbStore::build(&store_path, &graph).expect("build store");
    let store = RedbStore::open(&store_path).expect("reopen store");
    let mut reloaded = Graph::with_store(store);

    // Compare one table at a time: name -> sorted "id:serialized" pairs.
    let mut failures: Vec<String> = Vec::new();
    macro_rules! compare {
        ($ids:expr, $get:expr, $label:literal) => {{
            let mut expected: BTreeMap<u64, String> = BTreeMap::new();
            for id in $ids {
                let before = postcard::to_allocvec(&graph.get_node_for_test(*id)).expect("serialize");
                expected.insert(id.get(), String::from_utf8_lossy(&before).into_owned());
            }
            for (id, want) in expected {
                let got = $get(id);
                match got {
                    Ok(Some(node)) => {
                        let after = postcard::to_allocvec(&node).expect("re-serialize");
                        let got = String::from_utf8_lossy(&after).into_owned();
                        if got != want {
                            failures.push(format!("{}({id}) differs", $label));
                        }
                    }
                    Ok(None) => failures.push(format!("{}({id}) missing", $label)),
                    Err(error) => failures.push(format!("{}({id}) error: {error}", $label)),
                }
            }
        }};
    }

    compare!(graph.declarations().keys().copied().collect::<Vec<_>>(), |id| reloaded.declaration(id).map(|n| (*n).clone()), "declaration");
    compare!(graph.definitions().keys().copied().collect::<Vec<_>>(), |id| reloaded.definition(id).map(|n| (*n).clone()), "definition");

    assert!(failures.is_empty(), "round-trip mismatches:\n{}", failures.join("\n"));
}
```

Adjust the two accessor closures to whatever the layered getters expose (`Declaration`, `Definition`, …); `Clone` must be derived or reconstructed via a serialization helper already available (`Definition::serialize_roundtrip`). If a node type is not `Clone`, serialize both sides instead of cloning.

- [ ] **Step 2: Run it and see which types fail**

```bash
cargo test -p rubydex --features redb-store --test store_roundtrip
```

Every failure listed is a real bug: a node type that does not round-trip will misread after a store reload. Fix each by making the node's serde representation complete (skip nothing that affects behavior), TDD-style, one type per commit.

- [ ] **Step 3: Extend to the remaining tables**

Add the same comparison for `strings`, `names`, `constant_references`, `method_references`, `documents`, `name_dependents`, using the layered getters plus `RedbStore`'s existing key-only scans for ids.

- [ ] **Step 4: Commit**

```bash
git add rust/rubydex/tests/store_roundtrip.rs
git commit --no-gpg-sign -m "Round-trip every node type through the store"
```

---

## Phase C — Performance, measured not assumed

Two outliers are on record. Neither has a decided fix, and both predate the rebase. The gate in each task is: measure, then choose, then re-measure.

### Task C1: Store-backed search latency (decision gate)

Recorded: store-backed `Graph#search` ≈ 100 ms/op vs ≈ 0.05 ms/op in-memory, because `RedbStore::declaration_ids_matching` scans all ~686k `search_names` rows per call. `MatchMode::Exact` is `name.contains(query)` and `MatchMode::Fuzzy` is a subsequence score, so **neither mode can be answered by a prefix range** — a name-sorted index does not help, and correctness must not be weakened to make it help.

- [ ] **Step 1: Confirm the current number on the rebuilt store**

```bash
CORPUS=$(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/..
cargo run --manifest-path rust/Cargo.toml --release --features redb-store -- \
  --open-store /tmp/rdx-bench/stdlib.redb --search "enum" --stats
```

Record wall time and peak RSS. Repeat with `--search "e"` (worst case: matches nearly every row) and `--search "Enumerable"` (best case).

- [ ] **Step 2: Measure where the time goes**

Add a throwaway ignored benchmark next to the existing store tests that times, over the real store: (a) the range scan alone with a no-op predicate, (b) the scan plus `matches_query` at `Exact`, (c) at `Fuzzy`. That splits redb iteration cost from matching cost.

- [ ] **Step 3: Choose exactly one of these, based on the split**

- **If the scan dominates (>60%)** — parallelize it. `RedbStore::search_names` gets a sibling `declaration_ids_matching_par(query)` that splits the id keyspace into `available_parallelism()` ranges and scans them on `std::thread::scope`, merging into one `Vec`. Semantics are unchanged (the memory path already parallelizes), so no probe changes.
- **If matching dominates** — cut per-row cost. Keep `&str` (already non-allocating via `from_utf8_lossy` on valid UTF-8) and add an early reject on a cheap 8-byte prefix compare before the full `contains`/score.
- **If neither helps** — accept it and write the ceiling down in `MEMORY_POC.md` with the measured number, plus the mitigation: the LSP can batch a prefix into one query instead of per keystroke. Do not add an n-gram index on speculation; that is a multi-megabyte space cost and needs its own plan.

- [ ] **Step 4: Re-measure and keep only a win**

Run Step 1's three commands again plus the Ruby session profile (`docs/disk-persisted-index/session-profile.rb`, disk mode). If the win is under 20% wall time, revert the change and record the negative result in `docs/disk-persisted-index/memory-baseline.md`.

- [ ] **Step 5: Commit only if it won**

```bash
git add rust/rubydex/src/model/store.rs rust/rubydex/src/query.rs
git commit --no-gpg-sign -m "Cut store-backed search latency from <before>ms to <after>ms"
```

### Task C2: Store-build time (decision gate)

Recorded pre-rebase: 4.495s of a 6.3s build attributed to `Cleanup` — `RedbStore::build`'s write transaction plus the store's `Drop`. Task A3 supplies the current attribution.

- [ ] **Step 1: Use the A3 attribution to pick the work**

- **If serialization dominates** — the write loop is `write_map!` per table plus two projection tables. Check whether `RedbStore::build` can open one write transaction per *table* (redb commits are the expensive part) or use a single `WriteBatch`.
- **If commit/fsync dominates** — the store is 1.1 GB for 29k files; check whether the payload can be compressed (`redb` features) or whether `postcard` output can shrink. Measure the size delta before accepting a CPU cost.
- **If drop dominates** — the store's `Drop` closes redb; confirm with `strace`-level timing whether it is the OS flushing, which cannot be optimized in-process. If so, delete the open/drop timing from the profile and say so.

- [ ] **Step 2: Implement the chosen change behind the measurement**

No speculative work: if Step 1's numbers do not point at a Rust-side cost, close this task with a written negative result.

- [ ] **Step 3: Re-measure with the same command as A3 step 1 and update `docs/disk-persisted-index/memory-baseline.md`**

- [ ] **Step 4: Commit only if it won**

### Task C3: Overlay growth under live edits

A store-backed session keeps every live-edited node in the in-memory overlay forever; nothing evicts it. A day of editing in a large repo could grow the overlay without bound, eroding the memory win.

**Files:**
- Read: `rust/rubydex/src/model/graph.rs` (`materialize_*`, `declaration_mut`, tombstone set)
- Create: `rust/rubydex/tests/overlay_growth.rs` (integration test, store feature)

**Interfaces:**
- Consumes: `index_source`, `delete_document`, `attach_store`.
- Produces: a measured overlay size after N synthetic edits, and a documented ceiling.

- [ ] **Step 1: Write the measurement test**

Index a fixture corpus into a store, reopen store-backed, then apply 5,000 synthetic edits (rewrite one file 5,000 times with a changing body) and assert the in-memory maps stay bounded:

```rust
assert!(
    graph.declarations().len() < 2_000,
    "overlay must not retain every historical version: {} declarations after 5000 edits",
    graph.declarations().len(),
);
```

- [ ] **Step 2: Run it and record the real number**

If it passes, the cascade already prunes and this task closes with the number recorded. If it fails, record how badly (this is the ship-plan "overlay ceiling" decision input).

- [ ] **Step 3: If it fails, add an LRU only with evidence**

The ponytail ceiling: an unbounded overlay is acceptable while the measured post-edit size stays small. If it does not, cap the overlay (evict least-recently-used materialized nodes that are not tombstoned) with a documented constant, and re-measure both overlay size and search latency — eviction trades one problem for another and must be shown not to regress the read path.

---

## Phase D — CI guards, platforms, docs

### Task D1: Debug-jemalloc CI job for the FFI array APIs

Commit `8f9359b` was a sized-deallocation mismatch that glibc tolerated and jemalloc aborted on. `tikv-jemallocator` ships a `debug` feature that turns on jemalloc's `--enable-debug`, which makes that class deterministic (3/3 aborts instead of a 2/3 flaky segfault). Nothing in CI would have caught it.

**Files:**
- Modify: `rust/rubydex/Cargo.toml` (new `jemalloc_debug` feature)
- Modify: `ext/rubydex/extconf.rb` (opt-in via `RUBYDEX_JEMALLOC_DEBUG=1`)
- Modify: `.github/workflows/ci.yml` (new job)

**Interfaces:**
- Consumes: `RUBYDEX_JEMALLOC_DEBUG` env var; existing `jemalloc_dylib` feature.
- Produces: Cargo feature `rubydex/jemalloc_debug`; CI job `ffi-allocator-invariants`.

- [ ] **Step 1: Add the feature**

```toml
# rust/rubydex/Cargo.toml, next to `jemalloc_dylib`
# jemalloc built with --enable-debug: asserts on double free / sized-deallocation mismatch.
# Used by the FFI allocator-invariants CI job; never enabled for released builds.
jemalloc_debug = ["jemalloc_dylib", "tikv-jemallocator/debug"]
```

- [ ] **Step 2: Wire the build flag**

In `ext/rubydex/extconf.rb`, next to the redb-store flag:

```ruby
# Allocator invariant checks (double free, sized-deallocation mismatch). Debug jemalloc only;
# set RUBYDEX_JEMALLOC_DEBUG=1 to build with it.
cargo_args << "--features rubydex/jemalloc_debug" if ENV["RUBYDEX_JEMALLOC_DEBUG"]
```

- [ ] **Step 3: Prove it catches the bug it exists for**

Temporarily revert the `into_boxed_slice()` in `DiagnosticArray::from_vec`, build with the flag, run the suite, and watch jemalloc abort with `size mismatch detected`:

```bash
RUBYDEX_JEMALLOC_DEBUG=1 bundle exec rake compile
bundle exec rake ruby_test   # expect: <jemalloc>: size mismatch detected ... Abort
```

Restore the fix, rebuild, and confirm the suite passes.

- [ ] **Step 4: Add the CI job**

```yaml
  ffi-allocator-invariants:
    # Debug jemalloc turns sized-deallocation mismatches and double frees into hard failures
    # instead of the heap corruption glibc silently tolerates (see 8f9359b).
    runs-on: ubuntu-latest
    name: FFI allocator invariants (debug jemalloc)
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - name: Set up Ruby
        uses: ruby/setup-ruby@14594264cd68ce8a2345dd349bc3d138a4ef85c8 # v1.327.0
        with:
          ruby-version: "4.0"
          bundler-cache: true
      - name: Build with debug jemalloc
        run: RUBYDEX_JEMALLOC_DEBUG=1 bundle exec rake compile
        env:
          RUBYDEX_REDB_STORE: "1"
      - name: Ruby suite
        run: bundle exec rake ruby_test
```

- [ ] **Step 5: Commit**

```bash
git add rust/rubydex/Cargo.toml ext/rubydex/extconf.rb .github/workflows/ci.yml
git commit --no-gpg-sign -m "CI: assert allocator invariants with debug jemalloc"
```

### Task D2: CI job proving the in-memory path is unaffected

The store feature is opt-in for gem consumers (`RUBYDEX_REDB_STORE=1`), but this repo's `Rakefile` forces it on, so nothing in CI proves the default build still works.

**Files:**
- Modify: `.github/workflows/ci.yml`

- [ ] **Step 1: Add the job**

```yaml
  no-store-feature:
    # Proves the default (no redb-store) build is behavior-identical: the disk layer must be
    # entirely absent, not merely unused.
    runs-on: ubuntu-latest
    name: Default build without redb-store
    steps:
      - uses: actions/checkout@3d3c42e5aac5ba805825da76410c181273ba90b1 # v7.0.1
      - name: Set up Ruby
        uses: ruby/setup-ruby@14594264cd68ce8a2345dd349bc3d138a4ef85c8 # v1.327.0
        with:
          ruby-version: "4.0"
          bundler-cache: true
      - name: Rust tests without the feature
        run: bundle exec rake cargo_test
        env:
          RUBYDEX_NO_REDB_STORE: "1"
      - name: Ruby suite without the feature
        run: bundle exec rake ruby_test
        env:
          RUBYDEX_NO_REDB_STORE: "1"
```

If `Rakefile`'s forced `ENV["RUBYDEX_REDB_STORE"] = "1"` overrides the external value, change the Rakefile to `ENV["RUBYDEX_REDB_STORE"] ||= "1"` so an explicit external `0`/unset wins.

- [ ] **Step 2: Verify locally that the no-feature build compiles and passes**

```bash
RUBYDEX_NO_REDB_STORE=1 bundle exec rake compile
RUBYDEX_NO_REDB_STORE=1 bundle exec rake ruby_test
cd rust && cargo test -p rubydex
```

- [ ] **Step 3: Commit**

```bash
git add .github/workflows/ci.yml Rakefile
git commit --no-gpg-sign -m "CI: test the default build without the redb-store feature"
```

### Task D3: Verify the disk path on Windows

`build_store_via_fork` raised `NotImplementedError` on Windows because `fork` does not exist there, and the store builder now runs through `Process.spawn`, which does. The disk index may now work on Windows and nobody has checked.

**Files:**
- Modify: `test/disk_index_live_edit_test.rb` (skip condition)
- Modify: `MEMORY_POC.md` (platform support statement)

- [ ] **Step 1: Run the disk tests on Windows CI and record the outcome**

Add a step to the existing `build` matrix job (`windows-latest` is already there):

```yaml
      - name: Disk-index tests
        if: runner.os == 'windows'
        run: bundle exec ruby -Itest test/disk_index_live_edit_test.rb
        env:
          RUBYDEX_REDB_STORE: "1"
```

- [ ] **Step 2: If it passes, update the docs to say so and drop the Windows caveat**

In `MEMORY_POC.md` and the extconf comment, replace "unavailable on Windows" with the measured statement. If it fails, keep the caveat and record the exact failure — do not paper over it with a skip that hides a real platform bug; a `skip` is only acceptable with the failure documented next to it.

### Task D4: Move the `tmp/` docs into version control

`tmp/` is gitignored, so the review, ship plan, task list, memory baseline, differential plan and `session_profile.rb` exist only on this machine and would be lost.

**Files:**
- Create: `docs/disk-persisted-index/review.md`, `ship-plan.md`, `task-list.md`, `memory-baseline.md`, `session-profile.rb`, `plan-differential-harness.md`
- Modify: `MEMORY_POC.md`, `rust/rubydex/src/model/store.rs` (doc link), `lib/rubydex/graph.rb` (comment)

**Interfaces:**
- Produces: a tracked `docs/disk-persisted-index/` directory that `MEMORY_POC.md` links to.

- [ ] **Step 1: Copy the docs out of `tmp/` and fix their internal links**

```bash
mkdir -p docs/disk-persisted-index
cp docs/disk-persisted-index/review.md docs/disk-persisted-index/review.md
cp docs/disk-persisted-index/ship-plan.md docs/disk-persisted-index/ship-plan.md
cp docs/disk-persisted-index/task-list.md docs/disk-persisted-index/task-list.md
cp docs/disk-persisted-index/memory-baseline.md docs/disk-persisted-index/memory-baseline.md
cp docs/disk-persisted-index/plan-differential-harness.md docs/disk-persisted-index/plan-differential-harness.md
cp docs/disk-persisted-index/session-profile.rb docs/disk-persisted-index/session-profile.rb
```

Replace every `tmp/...` path inside those files with the new `docs/disk-persisted-index/...` path.

- [ ] **Step 2: Point the code and the top-level doc at the tracked copies**

- `MEMORY_POC.md`: add a line under its heading — `Design docs, review and benchmarks: docs/disk-persisted-index/.`
- `rust/rubydex/src/model/store.rs`: the module doc comment gets `//! Design docs: docs/disk-persisted-index/ship-plan.md`.
- `lib/rubydex/graph.rb`: `index_workspace`'s comment gets `See docs/disk-persisted-index/ship-plan.md for the rollout.`

- [ ] **Step 3: Make the session profiler runnable from its new home**

`docs/disk-persisted-index/session-profile.rb` hardcodes `/home/sean/...` paths and `/tmp/rdx-bench/stdlib.redb`. Parameterize it so a reviewer can run it:

```ruby
# frozen_string_literal: true

# Session profile: RSS + latency of a long-lived Ruby process using the graph API,
# disk-store-backed vs in-memory. Usage:
#   ruby -Ilib docs/disk-persisted-index/session-profile.rb <corpus> <store> [disk|memory]
# <store> must be a store built from <corpus>:
#   cargo run --manifest-path rust/Cargo.toml --release --features redb-store -- \
#     --build-store <store> <corpus>
corpus = File.expand_path(ARGV.fetch(0))
store_src = ARGV.fetch(1)
mode = ARGV[2] || "disk"
```

- [ ] **Step 4: Commit**

```bash
git add docs/disk-persisted-index MEMORY_POC.md rust/rubydex/src/model/store.rs lib/rubydex/graph.rb
git commit --no-gpg-sign -m "Track the disk-index design docs, review and benchmarks"
```

---

## Phase E — Branch hygiene

### Task E1: Push, and restore commit signing

Two commits are unpushed and unsigned: `31527f6` (Restore store-awareness after rebase) and `8f9359b` (sized-deallocation fix). The 1Password SSH signer is broken in this environment, so they were committed with `--no-gpg-sign`.

**Files:** none (git operations only)

- [ ] **Step 1: Diagnose the signer**

```bash
git config --get gpg.format; git config --get gpg.ssh.allowed-signers-file; ssh-add -L | head -3
gpg --list-secret-keys --keyid-format=long 2>/dev/null | head -5
```

- [ ] **Step 2a: If the SSH signer is fixed** — re-sign the two commits and push

```bash
git rebase --exec 'git commit --amend --no-edit -S' a0c3edf
git push origin disk-persisted-index
```

- [ ] **Step 2b: If it is still broken** — push unsigned and open a follow-up item

```bash
git push origin disk-persisted-index
```

Record in `docs/disk-persisted-index/task-list.md`: `Commits 31527f6/8f9359b are unsigned (1Password SSH agent broken); re-sign once the agent works.`

- [ ] **Step 3: Verify the push landed and CI is green**

```bash
git log --oneline origin/disk-persisted-index -3
```

### Task E2: Refresh the roadmap and task list to match reality

The ship plan's Phase 1 baseline (1104 tests / commit `28897e8`) and the task list's "Current state" no longer describe the branch.

**Files:**
- Modify: `docs/disk-persisted-index/ship-plan.md`, `docs/disk-persisted-index/task-list.md`

- [ ] **Step 1: Update the ship plan's baseline line**

Replace the pre-rebase baseline with: `post-rebase on origin/main (d8ff475): 1171 Rust (store) / 1153 (default) / 13 (sys store), 414 Ruby; clippy 0.`

- [ ] **Step 2: Mark Phase 1 tasks 1.2–1.6, 1.8, 1.9 done or carried**

1.2 → Task B4, 1.3 → Task B1+B2, 1.4 → superseded (spawn replaced fork), 1.5 → carried (hostile-env matrix not yet written), 1.6 → Task B3, 1.8 → Task D2, 1.9 → Task D4.

- [ ] **Step 3: Add the deferred Phase 2/3 items with owners**

Live-edit fuzzer, soak test, overlay ceiling (Task C3 feeds this), write-back decision, Windows build (Task D3 feeds this), LRU if C3 shows growth.

- [ ] **Step 4: Commit**

```bash
git add docs/disk-persisted-index
git commit --no-gpg-sign -m "Refresh the roadmap and task list after the rebase"
```

---

## Deferred (roadmap only, no task here)

- Live-edit fuzzer and soak test (Phase 2) — needs the overlay ceiling number from C3 first.
- Persisting overlay writes back to the store (Phase 2) — a design task, not a bug fix.
- LRU for the overlay — only if C3 shows growth.
- An n-gram index for substring/fuzzy search — only if C1's measurement says matching dominates *and* LSP query shapes justify the space cost.

## Self-review

- **Spec coverage:** items 1–3 (unverified work) → A1–A3; items 4–7 (plan remainder) → B4, B1, B2, B3, D2, D4, C1–C3; items 8–9 (hygiene) → E1, E2; the debug-jemalloc structural risk → D1. Every item from the "remaining known issues" list maps to a task.
- **Placeholder scan:** Tasks C1–C3 are decision gates by design (measure → choose → re-measure), and each option carries its concrete trigger condition and implementation sketch; no task says "improve performance" without a measurement command and a keep/revert rule.
- **Type consistency:** `StoreError`, `Graph::store_error_count`, `Graph#store_errors`, `STORE_FORMAT_VERSION`, `rdx_store_format_version`, and the `jemalloc_debug` feature are each defined once (B1, B2, B3, D1) and referenced by that same name everywhere else.
