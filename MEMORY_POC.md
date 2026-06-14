# Disk-persisted index: low-resident-memory proof of concept

rubydex holds its entire graph resident in RAM. For a large codebase that is
~800 MB–1.3 GB, which dominates the memory footprint of a language server that
embeds it. This branch (`disk-persisted-index`) adds an **opt-in** on-disk
backing store (the `redb-store` Cargo feature) that persists the graph to an
embedded [redb](https://github.com/cberner/redb) key-value store and answers
queries from disk, so the bulk of the index need not be resident.

## Result

Measured with a release build on the Ruby 4.0.5 standard library
(20,514 files, 354,574 names, 472,299 definitions):

| Path | What it does | Peak RSS |
|------|--------------|---------:|
| **A — index in RAM** | index + resolve the corpus, then build the store | **1,311 MB** |
| **B — read from disk** | open the prebuilt store and answer go-to-definition, no indexing | **~4 MB** |

On-disk store size: **515 MB**. Resident-memory reduction: **~325×**, while
still serving correct queries:

```
Pathname -> file://.../lib/ruby/4.0.0/pathname.rb @ 214
```

## Reproduce

```bash
cd rust
cargo build --release -p rubydex --features redb-store --bin rubydex_cli
CLI=target/release/rubydex_cli
CORPUS=$(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/..   # or any Ruby tree

# Path A: index in RAM and build the store (peak RSS reported by --stats)
$CLI "$CORPUS" --build-store /tmp/index.redb --stats

# Path B: answer go-to-definition from the store, no indexing (RSS reported)
$CLI --open-store /tmp/index.redb --query Pathname --stats
```

## How it works

Every graph node is keyed by an interned `u64` content hash (`Id<T>`), which
maps directly onto a redb table (`u64 -> serialized node bytes`, postcard).
`RedbStore::build` writes every node map in one transaction; `RedbStore::open`
+ the typed getters read individual nodes back. `definition_location` walks
`declaration -> definition -> document` as on-disk reads, holding nothing
resident beyond what each lookup touches.

## Caveats / not yet done

- **Path B is a query-only process**, not a full language server. A real server
  also holds the workspace + unsaved edits and serves many requests, so its RSS
  would be higher — but the gem/stdlib bulk stays on disk, which is the win.
- **No in-memory cache yet.** Reads deserialize from disk each time. An LRU in
  front of hot nodes would trade a little memory for latency; omitted here so
  the measurement reflects the floor.
- **`Document` line/column is unavailable from the store.** The foreign
  `LineIndex` is skipped on serialize and rebuilt empty on load; rebuilding it
  needs source retention (tracked).
- **First-definition selection.** `definition_location` returns the first of a
  name's definitions (e.g. `Set` may resolve to a monkey-patch rather than core);
  multi-definition ranking is a refinement, not a memory concern.
- **Mutation / live edits (Stage 4)** and **prefix/fuzzy search indexes
  (Stage 5)** are the remaining work to turn this POC into a full backend.
