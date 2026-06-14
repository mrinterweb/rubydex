# Proposal: optional disk-backed (low-resident-memory) index for rubydex

> Draft for a Shopify/rubydex issue or PR description. Not yet posted — review and
> send when ready.

## Problem

rubydex holds the entire graph resident in RAM. For a large application that is
~800 MB–1.3 GB, which dominates the memory footprint of a language server (e.g.
Ruby LSP) that embeds it. For multi-workspace or memory-constrained setups this
is the single largest cost.

## Proposal

Add an **opt-in** disk-backed store behind a `redb-store` Cargo feature that
persists the graph to an embedded [redb](https://github.com/cberner/redb)
key-value store and answers queries from disk. The default build is unchanged.

redb fits rubydex unusually well: every node is already keyed by an interned
`u64` content hash (`Id<T>`), which maps directly onto `u64 -> bytes` tables.
redb is **pure Rust** (no C dependency for the precompiled multi-arch gem) and
**mutable in place**, so an incremental edit just rewrites the affected node —
no immutable-snapshot + copy-on-write overlay needed.

## Result (measured)

Release build, Ruby 4.0.5 stdlib + gems (20,514 files, 472,299 definitions):

| Path | RSS |
|------|----:|
| index + resolve in RAM, build store | 1,311 MB |
| open prebuilt store, answer go-to-definition + a 20-result prefix search from disk | **~4 MB** |

On-disk store: 515 MB. **~325× resident-memory reduction**, queries served correctly.

## Design

- One redb table per node map (`declarations`, `definitions`, `names`, …), value
  = postcard-serialized node. `RedbStore::build` writes all maps in one
  transaction; typed getters read individual nodes back.
- A redb multimap secondary index (short name → declaration IDs) serves prefix
  search via an ordered range scan, without iterating the graph.
- `put_node`/`delete_node` provide in-place incremental mutation.
- Two representative queries implemented end-to-end from disk: `definition_location`
  (go-to-definition) and `search_prefix` (workspace symbols).

## Scope / what would need maintainer input

This branch is a proof of concept. Turning it into a full backend needs:
1. **Routing the live query layer through the store** — accessors return owned/
   `Cow` values (the store deserializes), ideally fronted by an LRU cache, with
   `query.rs`/`resolution.rs` call sites migrated.
2. **Live-edit integration** — workspace edits and incremental resolution writing
   through to the store (the primitives exist; the indexing/resolution wiring does not).
3. **`Document` source retention** so an accurate `LineIndex` can be rebuilt from
   the store (currently skipped, so line/column is unavailable for store-loaded docs).

## Question for maintainers

Is an optional, feature-gated disk-backed backend a direction you'd accept in
rubydex? If so, I'd value guidance on the read-path abstraction (owned/`Cow`
accessors vs a `GraphView` trait) before investing in the full query-layer
migration.
