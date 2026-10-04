# Code review: `disk-persisted-index` vs `main`

Branch: `disk-persisted-index` (50 commits, ~2,580 lines changed)
Reviewed: layered graph accessors, redb store, fork orchestration, FFI migration, Ruby glue.

**Verified first:** `cargo test` 1089 pass (default), 1099 pass (`--features redb-store`), clippy clean, `bundle exec rake ruby_test` 264 pass incl. new live-edit test. Then traced store-backed read/write paths end to end.

## P1 — Crashes in disk mode (reproducible)

### 1. Expression completion panics on store-backed graphs — **DONE (fixed in `4b24ebe`)**

`rust/rubydex/src/query.rs:488-491` (`expression_completion`) and `:582,:599` (`collect_class_variables_from_lexical_scope`) still use in-memory-only `graph.declarations().get(...).unwrap()`. In disk mode the lexical-scope declaration lives only in the store → `unwrap()` on `None` → panic crosses FFI → server process dies. This is the *most common* completion (typing inside a class body).

Reproduced with a scratch test (store-backed graph, `CompletionReceiver::Expression` with a real nested `NameId`):

```text
thread 'expression_completion_store_backed' panicked at rubydex/src/query.rs:490:10:
```

Fix: layered accessor `graph.declaration(...)` — `collect_members` in the same file already does this correctly. The migration (commits 3471e41/a255bf7/435f3bd) missed these call sites.

### 2. `follow_method_alias` — same pattern — **DONE (fixed in `f8ced2d`)**

`query.rs:822,836`: `.expect("member returned by find_member_in_ancestors must exist")` on the in-memory map, but member IDs come from store-backed namespaces. Panics on alias hover / signature help for gem classes (`definition_api.rs:555`, `signature_api.rs:147`).

## P1 — Silent functional regression in disk mode

### 3. Declaration search returns ~nothing — **DONE (fixed in `1afd7a6`)**

`declaration_search` (`query.rs:42`) iterates `graph.declarations()` — the in-memory overlay only. After `attach_store` the overlay is empty, so `Graph#search` / `fuzzy_search` (the LSP symbol-search path) and the `--search` CLI return 0 results in disk mode. MEMORY_POC.md lists this as "Stage 5" — fair — but it silently breaks a primary LSP feature behind a one-env-var opt-in, and the CLI prints a confident "0 match(es)". Either scan the store (name index table — the Stage 5 work) or make the gap loud (log/refuse) rather than empty.

## P2 — In-memory-only lookups

### 4. Require-path resolution — **DONE (fixed in `c76f454`)**

`resolve_require_path` (`query.rs:122`) and `require_paths` (`query.rs:141`) read only `graph.documents()` → require completion empty, `Graph#resolve_require_path` → nil in disk mode.

### 5. FFI `_document` getters — **DONE (fixed in `5bbe5bb`)**

`definition_api.rs:508`, `reference_api.rs:237,317`, `graph_api.rs:449` — in-memory only → Ruby `Definition#document` raises `"Definition not found"` for store-only nodes; `Graph#document(uri)` → nil for store documents.

### 6. Tombstone gap (latent resurrection) — **DONE (fixed in `3242d9f`)**

`removed_declarations` covers declarations only. Definitions/references/names/strings removed from the overlay by a live edit are still in the store, and layered getters resurrect them — e.g. `rdx_method_reference_location` will serve a stale location for a reference deleted by an edit. The resolver is safe: `prepare_units` deliberately checks in-memory maps with a good comment (`resolution.rs:1940`). Options: one shared tombstone set checked by all layered getters (cheapest, one `contains` per fallback), or document the invariant "layered getters only for IDs referenced by overlay index structures" and enforce with a debug assert.

## P3 — Nits — **DONE (fixed in `28897e8`)**

- `REBUILT_LINE_INDEXES` thread-local memo (`location_api.rs`) never invalidates: fine for gem/stdlib; a workspace file edited on-disk outside the LSP serves stale positions until process restart. One comment stating the cache lifetime. — **done**
- `build_store_via_fork` (`lib/rubydex/graph.rb`): `*.building` temp leaked when the child fails. `ensure` cleanup. — **done** (plus regression test `test_failed_store_build_cleans_up_temp_files`)
- `NodeRef` doc (`graph.rs`) overstates: read paths never materialize — only write paths do. Gem-code hovers re-deserialize every access; "repeated lookups hit the in-memory map" only holds for write-touched nodes. — **done**
- `MEMORY_POC.md` stale: lists live edits (Stage 4) as remaining; they're implemented on the disk path (75caeed). — **done**
- `extconf.rb` compiles redb into every gem build by default (opt-out `RUBYDEX_NO_REDB_STORE=1`). Fork-fine; for upstream that's build-time + staticlib size for non-users — make it a flag. — **done** (now opt-in `RUBYDEX_REDB_STORE=1`; this repo's Rakefile keeps it on for dev/tests)
- `RedbStore::build` `.expect("node should serialize")` — already documented spike-only; fine until the Stage 1 `StoreError`. — **left as-is by design**

## What's good

- Layered accessor design is clean; the FFI migration is thorough and in several spots *better* than main (proper `Option` handling where main had `.unwrap()`).
- `prepare_units` stale-unit guard is explicit and well-reasoned.
- Atomic publish order (store → marker) with per-pid temps, and the concurrent-reader argument, is correct and documented.
- LineIndex rebuild-from-disk with offset clamping instead of panicking across FFI — good call.
- Test coverage is real: byte-identical round-trips, materialize-on-write, live-edit resolution, store-backed completion, plus the Ruby live-edit test with a correct Windows skip guard.

## Bottom line

The store core (redb layer, serialization, fork orchestration, tombstones for declarations, live-edit resolve) is solid and well-tested. The blocker is the incomplete accessor migration in `query.rs` — items 1–2 are process crashes in disk mode, item 3 a silent dead feature. Fix those three and this is close.
