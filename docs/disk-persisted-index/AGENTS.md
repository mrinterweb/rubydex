# Disk-Persisted Index — Agent Guide

The disk-persisted index backs `Rubydex::Graph` with a read-only `redb` database (`index.redb`) so
long-lived editor/MCP sessions stay low-RSS while edits accumulate in an in-memory overlay and a
background daemon keeps stores fresh.

## Architecture & File Map

- **Storage engine (`rust/rubydex/src/model/store/`)**: `RedbStore` (format v3) in `mod.rs` and
  pure binary codecs (front-coding + hash-prefix buckets) in `format.rs`. Eight node tables are
  hash-prefix bucketed (~64 nodes/row, 16 for `documents`) under `u32` bucket keys and encoded in
  parallel; `search_names` (32 shards) and `document_uris` (1 block) are front-coded. `Id<T>`
  encodes as fixed 8-byte LE.
- **Layered graph (`rust/rubydex/src/model/graph.rs`)**: `Graph::with_store` / `attach_store`
  layers an in-memory overlay over `RedbStore`. Reads check memory/tombstones first, then disk.
- **Ruby lifecycle (`lib/rubydex/graph.rb`)**: cache path resolution (`RUBYDEX_CACHE_DIR` >
  `[disk_index] location` > workspace `tmp/` > platform cache), generation-numbered store files
  (`index.<16hex>.redb` named by `"store"` in `index.redb.files` so rebuilds never overwrite open
  handles on Windows), JSON manifest (`index.redb.files`) and signature (`index.redb.hash`),
  create-exclusive `.rebuild` lock, surgical refresh, and child build process.
- **Background daemon (`rust/rubydex-index-manager/`)**: watches active workspaces via `notify` v9
  (`fs_events.rs`), tracks live sessions (`registry.rs`), triggers incremental/full rebuilds
  (`builder.rs`), and evicts stores of deleted workspaces (`prune.rs`).
- **Tests**:
  - Rust: `rust/rubydex/tests/{store_roundtrip,differential_store,store_corruption,overlay_growth}.rs`,
    `cargo test -p rubydex-index-manager`.
  - Ruby: `test/disk_index_*.rb` (cache location, corrupt store, cypher, enumerators, live edit,
    manager sessions/settings, surgical/session refresh).

## Invariants & Traps

- **Dual feature builds are mandatory.** `redb-store` is an optional Cargo feature. Every Rust
  change must compile and pass `clippy --workspace --all-targets -- -D warnings` (`clippy::pedantic`
  is active) under **both** default features and `--features rubydex/redb-store,rubydex-sys/redb-store`.
  Builds without `redb-store` must preserve the in-memory fallback cleanly.
- **Bump `STORE_FORMAT_VERSION` on layout changes.** Any change to serialization, bucket layout, or
  metadata keys in `model/store/` must bump `STORE_FORMAT_VERSION` (currently `3`) so existing
  caches trigger a rebuild instead of misreading bytes.
- **Gate storage size on `TableStats`, not `.redb` file size.** `redb` doubles its file as it grows,
  so `.redb` file size moves in coarse 2× steps and hides real savings. Always compare
  `stored + fragmented + metadata` ("Live footprint") and `leaf_pages` via
  `rubydex_cli --stats --build-store` or `utils/bench_store.rb`.
- **Comments are offsets only on disk.** `Comment.string` is `#[serde(skip)]` under `redb-store`
  and sliced from disk on demand via `Document::read_matching_source()` guarded by the document's
  `xxh3_64` content hash (stale/missing files return `""`).
- **Manifests are untrusted JSON; eviction is fail-closed.** `index.redb.files` is JSON (never
  `Marshal`) with keys `"workspace"`, `"lockfile"`, and `"files"`. `prune.rs` removes a store
  directory only when it is a real directory (not a symlink), has a readable manifest naming an
  absolute `workspace` whose `stat` returns `NotFound` (never other I/O errors), is not held by a
  live session, and has no `.rebuild` lock.
- **Defaults stay opt-in.** Only `rdx mcp` defaults to disk-backed indexing (`RUBYDEX_DISK_INDEX=0`
  must still force in-memory); Ruby LSP and `rdx query` keep their existing defaults, and
  `RUBYDEX_INDEX_MANAGER` remains opt-in.
- **MCP instructions are built from the running mode.** `initialize` returns disk-backed
  instructions (out-of-band edits checked before each call, picked up within `Graph::REFRESH_TTL`
  = 2 s or immediately after a commit moves HEAD, previous snapshot serving during a rebuild) only
  when `Graph#disk_index_enabled?` holds; otherwise it says out-of-band writes are not seen until the
  session restarts. Change the mode and the instructions change with it.
- **Git config isolation in Ruby tests.** Run disk-index Ruby tests with
  `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null` so local git config cannot affect
  subprocess tests.

## Measured Dead Ends (Do Not Re-Try Without Changing Assumptions)

- **LZ4 block compression on buckets/shards**: measured and reverted (Phase 3B). Buys 6% payload
  (0% file size) and regresses fuzzy search by 50% (61 → 93 ms) because point reads decompress whole
  ~10 KB buckets while remaining data is mostly uniformly-distributed 64-bit hashes.
- **Empty-query `declaration_search` via `store_declaration_ids()`**: measured 10× slower (42 ms vs
  4.1 ms) than the parallel sharded `search_names` scan because `declarations` streams large value
  pages.
- **Pre-loaded resident built-ins in disk mode**: tried and reverted (added resident heap without
  justifying the complexity).
- **Dropping one document↔node edge direction or sharing a global gem store**: changes invalidation
  paths that the live-edit overlay and surgical refresh depend on; treat as separate architectural
  projects.

## Verification & Benchmarks

```bash
# Clippy (both feature sets)
(cd rust && cargo clippy --workspace --all-targets -- -D warnings)
(cd rust && cargo clippy --workspace --all-targets --features rubydex/redb-store,rubydex-sys/redb-store -- -D warnings)

# Focused Rust & Ruby disk-index tests
(cd rust && cargo test -p rubydex --features redb-store && cargo test -p rubydex-index-manager)
GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest -e 'ARGV.each { |f| require File.expand_path(f) }' test/disk_index_*.rb

# Benchmarks (build release binaries first; never benchmark debug builds)
bundle exec rake compile_release
(cd rust && cargo build --release --features redb-store --bin rubydex_cli)
bundle exec ruby utils/bench_store.rb <workspace> --json > /tmp/base.json
bundle exec ruby utils/bench_store.rb <workspace> --compare /tmp/base.json
bundle exec ruby utils/bench_edit_update.rb <workspace>
```

## Detailed Docs in This Folder

- `database_diet.md`: byte-level breakdown of every redb table, current Discourse before/after
  numbers, and why specific cuts landed or were rejected. Read before changing store encoding or
  table layout.
- `storage-refactor-plan.md`: wire formats, bucket sizing measurements, and the Phase 3B LZ4
  post-mortem. Read for exact format details or historical phase benchmarks.
- `perf-and-cleanup.md`: streaming enumeration, warm-boot deduplication, non-blocking manager
  polling, and query perf measurements. Read before touching store iterators or daemon scheduling.
- `memory-baseline.md`: build vs open RSS attribution, session profile methodology, and past
  measurement traps. Read when investigating RSS regressions.
- `fs-event-crate.md`: why `notify` v9 was chosen for `rubydex-index-manager` and accepted
  platform limitations. Read before changing file watching.
- `mcp-ontology-host.md`: `rdx mcp` disk-default constraints, tool serialization across refresh, and
  why method receivers and unbounded Cypher are not exposed as resolved semantics. Read before
  modifying MCP tools or server lifecycle.
- `INSTALL.md`: building from source, local Gemfile wiring, and Neovim + Ruby LSP add-on setup.
  Read when testing editor integration end to end.
