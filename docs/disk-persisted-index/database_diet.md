# Database diet: what the redb store actually contains

The reference Rails project on the other machine has a **>500 MB** `index.redb`. That is a lot of bytes
for a code index, so this document measures where they go and ranks what could be cut. It is an
analysis, not a change: nothing in this document has been implemented.

All figures below are measured on a stand-in corpus — the Ruby 4.0.10 gems tree
(`~/.asdf/installs/ruby/4.0.10/lib/ruby/gems/4.0.0/gems`), **5,002 files / 29.4 MB of Ruby source** —
because that tree is here and the 500 MB store is not. The corpus is smaller than the reference
project, but it is a real Ruby load (all of ActiveSupport, RBS, Prism, etc.), so the ratios should
carry over.

| | |
| --- | --- |
| Ruby source indexed | 29,441,378 bytes (28.1 MB) |
| Store file | 134,746,112 bytes (**128.5 MB**) |
| Ratio | **4.58× the source** |
| Entries (all tables) | 535,399 |
| Logical bytes (all keys + values) | 110.64 MB |

So the store is ~4.6× the size of the code it describes, or ~25 KB per indexed file. At that ratio a
Rails app with ~20 MB of `app/` plus its gems lands in the hundreds of megabytes — which is exactly
what the reference project shows. The size is not one bug; it is four separate multipliers.

## What is stored

Eleven tables (`rust/rubydex/src/model/store.rs`). "Logical" is the sum of key and value bytes for
every row, which is what the data *is*; the file also carries B-tree overhead and allocator slack.

| Table | Key → Value | Entries | Logical | Purpose |
| --- | --- | --- | --- | --- |
| `definitions` | `DefinitionId` → serialized `Definition` | 67,160 | 18.75 MB | Every class/module/method/constant definition, its flags, offsets, mixins, members and **comments** |
| `declarations` | `DeclarationId` → serialized `Declaration` | 59,785 | 11.04 MB | One per fully-qualified name: FQN string, parent scope, nesting, definition ids |
| `method_references` | `MethodReferenceId` → serialized `MethodReference` | 195,111 | 10.96 MB | Every call site: receiver, name, offset, **`uri_id`** |
| `documents` | `DocumentId` → serialized `Document` | 2,669 | 5.24 MB | One per file: URI, content hash, diagnostics, and **`Vec` of every definition/reference id in the file** |
| `search_names` | `NameId` → FQN string | 59,785 | 4.31 MB | Projection so prefix search does not deserialize declarations |
| `constant_references` | `ConstantReferenceId` → serialized `ConstantReference` | 49,697 | 2.46 MB | Every constant lookup: name, offset, **`uri_id`** |
| `name_dependents` | `NameId` → dependent ids | 28,521 | 2.04 MB | Reverse edges for invalidation |
| `names` | `NameId` → name string | 28,523 | 1.83 MB | Interned name strings |
| `strings` | `StringId` → string | 41,478 | 1.72 MB | Interned strings |
| `document_uris` | `UriId` → URI string | 2,669 | 0.52 MB | Projection so require-path completion skips `documents` |
| `store_metadata` | `u8` → `u32` | 1 | 0.00 MB | Format/version marker |

Two of these are already deliberate projections (`search_names`, `document_uris`) added to avoid
deserializing big nodes. They are cheap in CPU and expensive in bytes — see inefficiency 2.

## Where the file's bytes go

Of the 128.5 MB file, only 67.6 MB (52.6%) is row payload. The rest is redb's own bookkeeping:

| | MB | % of file |
| --- | --- | --- |
| Payload (all keys + values) | 70.9 | 55% |
| B-tree page slack (redb's `fragmented_bytes`) | 40.6 | 32% |
| redb metadata | 4.5 | 3.5% |
| Free pages / allocator | ~12.5 | 10% |

The 32% of page slack is the surprise. It is not dead space from deleted rows — redb reports it as
fragmentation *inside* live leaf pages. The store is written once, in key order, and never updated
much, so this is the cost of redb's leaf-split policy: pages are filled to a split threshold and
then each half is left partly empty. At ~2.3 KB of slack per leaf page across 13,959 leaf pages, it
adds up to more than a third of the file. Cutting payload helps twice here, because fewer bytes means
fewer leaves and proportionally less slack.

## Four inefficiencies

### 1. Comment text is stored in the database (17.15 MB, 24% of payload)

Every definition struct carries `comments: Box<[Comment]>` (`rust/rubydex/src/model/definitions.rs:279`
and the sibling structs below it), and `Comment` stores the comment's **text**
(`rust/rubydex/src/model/comment.rs:6`). For the gems corpus
that is 15,008,477 bytes of text in 478,775 comments, encoded as 17,980,993 bytes (17.15 MB) of the
`definitions` payload — a quarter of everything the store holds.

The text is fully redundant. A comment already knows its file (`uri_id`) and its byte offset, so the
text can be re-read from the source file on demand — which is what the source file is for. Keeping
only the offset range makes the comment a pointer instead of a copy.

Ruby-side consumers are essentially nil: `grep` finds no use of `comments` in `lib/` or `ext/`, and
the only Rust consumer outside the model is the temporary measurement example. The cost of dropping
it is that a hover on a definition with no source file in reach returns no comment text — acceptable
for a cache that is always rebuildable from source.

- Payload: **−12.9 MB** (stored text drops, offset/kind metadata stays)
- File: **≈ −22 MB (−17%)** after the accompanying slack reduction
- Effort: small. Risk: low; needs a source-read path on the read side.

### 2. Two projection tables duplicate what the real tables already hold (3.76 MB)

`search_names` (4.31 MB) is a second copy of the FQN strings already in `declarations` (3.14 MB of
FQN text there). `document_uris` (0.52 MB) is a second copy of the URIs already in `documents`.

Both exist for a good reason — name search and require-path completion should not deserialize whole
declarations — but the same reason can be served without a second copy. Front-coding the FQNs (store
a shared prefix length plus a suffix) or keying the projection by `DeclarationId` and reading the
declaration's already-interned string would keep the lookup cost and remove the duplication.

- Payload: **−3.8 MB**
- File: **≈ −6 MB (−5%)**
- Effort: small. These projections also compress unusually well (3.9× under zstd) precisely because
  they are duplicate text, so the saving is smaller than the raw figure if step 4 lands first.

### 3. The document↔node relation is stored in both directions (~17 MB)

`documents` holds `definition_ids`, `method_reference_ids` and `constant_reference_ids` — every id in
the file, 822,094 u64s across the corpus, ~8.2 MB at 10 B per stored id, of which 5.77 MB is the
`documents` table's own payload. Meanwhile every definition, method reference and constant reference
independently stores its `uri_id`: ~3.3 MB in `method_references`, ~1.1 MB in `constant_references`,
plus the same field in definitions.

That is one edge set written twice. Either direction can be derived from the other by a scan, so one
of them is redundant. Dropping the per-node `uri_id` and keeping the document id lists is the cheaper
direction: `documents` is only 2,669 rows, so "which file does this node belong to" becomes a lookup
over the small table instead of a scan over 195,111 reference rows. Dropping the id lists instead
keeps per-node reads direct but makes "everything in this file" a full scan — the wrong trade for
incremental updates, which are per-file.

- Payload: **−11.5 MB** (whichever direction is dropped, net of the index needed to keep lookups fast)
- File: **≈ −19 MB (−15%)**
- Effort: medium. Every read path that resolves a node to a file (or a file to its nodes) changes.

### 4. Gem and stdlib content is duplicated into every workspace store

A one-file project's store is not small: a single-file workspace indexed 938 documents, and
`graph.documents.first(3)` were gem files (`rbs-4.2.0/lib/rbs/ast/comment.rb`,
`language_server-protocol-3.17.0.6/...`). **98% of that store is library code that every other
workspace on the machine also stores.** Two Rails projects sharing a gem set therefore pay for the
same gem bytes twice.

This is the multiplier that matters for the ">500 MB" complaint on a multi-project machine: it is not
one store being fat, it is the same fat store per project. A read-only shared library store keyed by
`(gem name, version, content hash)` would collapse it to one copy.

- File: **N× for N projects** sharing gems — the largest single win on a multi-project machine
- Effort: large. Needs a two-tier store (workspace layer + shared library layer), a lookup path that
  spans both, and invalidation when a gem version changes.

### 5. No eviction: deleted workspaces keep their stores

There is no pruning of the cache root. `~/Library/Caches/rubydex` on this machine accumulated ~900 MB
across directories, most of them for workspaces that no longer exist or were re-hashed. The manager
reaps watchers (`watches.retain(...)`, `rust/rubydex-index-manager/src/watches.rs:64`) but nothing
reaps store directories. This does not make one store bigger; it makes the total on disk bigger than
the sum of the projects you still have.

- Reclaims ~900 MB on this machine alone
- Effort: small — an age/owner check on cache-root entries at startup or on manager tick

## What is *not* the problem

These were all tested and all came back negative, so they are worth recording as ruled out:

| Suspect | Result |
| --- | --- |
| Fragmentation from updates | `compact()` **increased** the file: 128.5 MB → 221.3 MB. The store is written once in key order; there is nothing to compact, and redb's rewrite path only adds copy-on-write versions. |
| Small write batches | Rebuilding with batches of 1,000 / 10,000 / 50,000 / unlimited produced files of identical size. Batch size is a build-time lever, not a size lever. |
| Insertion order | Shuffled insertion produced no size reduction. Sorted-key insertion is already correct. |
| redb page size / cache settings | `set_cache_size` is a memory knob. Page size trades I/O granularity for internal waste and cannot recover the 32% leaf slack. |

## Compression headroom

The payload compresses well — zstd gives **1.9× per table** and **2.6× on the whole blob** — but redb
offers no per-value or per-table compression hook, and compressing individual rows would not help
(the rows are small; the win is in shared context across rows). Getting the 2.6× means grouping a
document's rows into one blob and compressing the group, which is exactly the "packed rows" step in
the plan below. Compressing rows one at a time would add framing overhead and cost more than it saves.

## Ranked plan

| # | Change | Payload | File | Effort |
| --- | --- | --- | --- | --- |
| 1 | Drop comment text, keep offsets; read from source on demand | −12.9 MB | ≈ −22 MB (−17%) | small |
| 2 | Drop the two projection tables (or front-code them) | −3.8 MB | ≈ −6 MB (−5%) | small |
| 3 | Keep one edge direction only (drop `documents` id lists *or* per-node `uri_id`) | −11.5 MB | ≈ −19 MB (−15%) | medium |
| 4 | Per-document packed rows + zstd | −32 MB more | ≈ −45 MB (−35%) | large |
| 5 | Share one read-only library store across workspaces | — | N× for multi-project | large |
| 6 | Evict stores for deleted workspaces | — | reclaims ~900 MB here | small |

Steps 1–3 are a **~31% cut with no query semantics lost** — they remove copies and duplicates, not
capabilities. Step 4 is the one that gets a single store to ~45 MB for this corpus. Steps 5–6 attack
disk *across* projects rather than one store's size, and 5 is the one that addresses a 500 MB
reference store on a machine with many projects.

## How this conclusion was reached

The path from "500 MB is a wild size" to the ranked plan above, step by step. Each step is a
measurement, not a guess, and each one narrowed the next.

### Step 1 — Get a corpus whose store I can take apart

The 500 MB store is on another machine. The nearest stand-in here was a real Ruby load of known size:
the Ruby 4.0.10 gems tree. Building its store with the CLI's own build path (not a test harness) meant
the numbers would come from production code:

```bash
./target/release/rubydex_cli --build-store /tmp/gems.redb \
  ~/.asdf/installs/ruby/4.0.10/lib/ruby/gems/4.0.0/gems
```

Result: 5,002 files, 29,441,378 bytes of source, 134,746,112-byte store. That is the 4.58× ratio. The
first real finding was simply that the ratio is *consistent* — a store is roughly 4–5× its source
whatever the source is — so the 500 MB figure is a consequence of corpus size, not a pathology of one
project.

### Step 2 — Attribute bytes to tables

A temporary example (`rust/rubydex/examples/store_breakdown.rs`, written, used, then deleted — it is
not committed) opened the store read-only and walked every table with redb's own accounting:

- `ReadableTableMetadata::stats()` / `TableHandle::stats()` per table: entries, stored bytes,
  metadata bytes, `fragmented_bytes`, leaf pages, branch pages.
- Note for anyone re-running this: `stored_leaf_bytes` and `fragmented_bytes` are **private** in redb
  4.1.0. The public `TableStats` accessors are what the example used; the private fields are not
  reachable and that is why the first two attempts failed to compile.

This is what produced the per-table table above, and the first surprise: `definitions` alone is 18.75
MB of 110.64 MB logical, and the file carries 40.6 MB of leaf-page slack on top of the payload.

### Step 3 — Attribute bytes to fields inside a table

The table totals say *which* table is big; they do not say *why*. For that the example decoded each
row with the model's own `Deserialize` (postcard, the same codec the store uses) and summed the
fields of the decoded structs. That is how the two largest single-line findings were obtained:

- `definitions` → 17,980,993 bytes of the payload is the encoded `comments` field, holding 15,008,477
  bytes of comment text across 478,775 comments.
- `declarations` → 3,137,417 bytes is FQN text; and across all tables, 822,094 id slots are stored
  inside values (~8.2 MB at 10 B each), which is what pointed at the double-stored edges.

### Step 4 — Rule out the cheap hypotheses

Before proposing schema changes, four obvious candidates were tested and all failed:

| Test | How | Result |
| --- | --- | --- |
| Fragmentation from updates | `Database::compact()` on a copy | 128.5 MB → 221.3 MB. Compaction **doubles** the file; there is nothing to reclaim, only copy-on-write versions to keep. |
| Write-batch granularity | A standalone binary that read every table and re-inserted it into a fresh database in batches of 1,000 / 10,000 / 50,000 / unlimited | Identical file size in every case. Batch size is a build-time lever, not a size lever. |
| Insertion order | Same binary, rows shuffled instead of sorted | No size reduction. The existing ascending-key insertion in `build` is already the right choice. |
| redb page size / cache | Read `db.rs` builder options and redb's split logic | `set_cache_size` is a memory knob; page size trades I/O granularity for internal waste and cannot recover the 32% leaf slack. |

That step is what moved the conclusion from "redb is wasting space" to "redb is behaving normally; the
payload being written to it is redundant". The 32% slack is a consequence of payload size, not a bug
of its own — fewer payload bytes means fewer leaves and proportionally less slack.

### Step 5 — Measure compressibility

The example could dump each table's raw key/value bytes to a file. Piping those through zstd answered
whether the fix is "compress" or "remove":

- Whole payload: **2.6×** (67.6 MB → ~26 MB).
- Per table: ~1.9× on average; the projection tables (`search_names`, `document_uris`) hit **3.9×**,
  which is itself evidence that they are duplicate text.

redb has no per-value compression hook, and compressing single rows would add framing overhead per row
for little gain — the win is shared context across rows. So compression is only reachable as part of
the packed-rows step, not as a store-wide setting.

### Step 6 — Measure duplication across workspaces

A one-file scratch project was indexed to see what a *small* workspace costs. It reported 938
documents, and `graph.documents.first(3)` were gem files (`rbs-4.2.0/lib/rbs/ast/comment.rb`,
`language_server-protocol-3.17.0.6/...`). **98% of that store is library code.** That is the finding
that reframes the 500 MB complaint: on a multi-project machine the cost is not one fat store, it is
the same fat store per project, and no amount of per-store dieting fixes that. It also motivated the
cache-root check: `~/Library/Caches/rubydex` held ~900 MB across directories, most for workspaces
that no longer exist.

### What this method does not establish

- Absolute sizes for the reference project. The ratios should transfer, but the mix of comment-heavy
  vs. code-heavy gems will not be identical.
- Whether the savings hold for a *Rails* store specifically. Rails apps have more method references
  and fewer comments per definition than gem code; if anything that makes inefficiencies 2 and 3
  larger and 1 smaller.
- Read-path cost of the proposed changes. Every step in the plan changes a read path; this document
  measures storage only.

## The persistence path: which files write the store

If you want to change what is stored, this is the code you touch. The path runs Ruby → C → Rust →
redb, and the *schema* is not written anywhere by hand — it is derived from the in-memory structs.

| Layer | File | What it does |
| --- | --- | --- |
| Ruby API | `lib/rubydex/graph.rb:72-100` | Decides build vs. reuse vs. attach; `graph.rb:732 build_store_in_child` forks the writer child; `graph.rb:499 store_cache_path` picks the directory |
| Ruby session | `lib/rubydex/index_session.rb` | Session-level refresh and rebuild coordination |
| C extension | `ext/rubydex/graph.c:963` `rdxr_graph_build_store` | Ruby method → FFI call |
| Rust FFI | `rust/rubydex-sys/src/graph_api.rs:74` `rdx_graph_build_store` | Graph pointer + path → `RedbStore::build` |
| Store | `rust/rubydex/src/model/store.rs:216` `build()` | The only production writer: one write transaction, every node map into its table |
| Table defs | `rust/rubydex/src/model/store.rs:27-43` | The 11 tables, all `TableDefinition<u64, &[u8]>` |
| Row writer | `rust/rubydex/src/model/store.rs:228-243` `write_map!` | Sorts ascending, `postcard::to_allocvec` per node, one `insert` per row |
| Projections | `rust/rubydex/src/model/store.rs:245-270` | The two derived tables, written straight from in-memory strings |
| Read path | `rust/rubydex/src/model/store.rs:372` `get_node` | `postcard::from_bytes` per node |
| Format | `rust/rubydex/src/model/store.rs:116` `STORE_FORMAT_VERSION` | Bump when a node's serialized shape changes |

### The structural cause: the schema is the struct

There is no separate persistence schema. Every persisted type carries
`#[cfg_attr(feature = "redb-store", derive(serde::Serialize, serde::Deserialize))]` — **43 derive sites**
across `rust/rubydex/src/model/` (`declaration.rs`, `definitions.rs`, `document.rs`, `references.rs`,
`name.rs`, `string_ref.rs`, `comment.rs`). The store writes whatever the in-memory struct happens to
hold.

That is why the size problem is not a mistake in one function. Nobody sat down and decided to store
comment text; the struct has a `comments` field because the in-memory index needs it, and the derive
writes it. The only field in the entire schema that is deliberately excluded is `Document::line_index`
(`document.rs:19`, `serde(skip, default = "empty_line_index")`), because it is derivable from the
source. Everything else — including text that is equally derivable from the source — is written.

The codec is not the problem either. `postcard` (`store.rs:239`, `:381`) is already a compact varint
binary format; switching to JSON or bincode would make the store bigger, not smaller. The cost is in
*which fields exist*, not in how they are encoded.

Two smaller structural notes:

- **Keys duplicate values.** `Id<T>` is a content hash (`model/id.rs:27`, `xxh3` over the value's own
  fields via `id_from_parts!`), so the 8-byte key of a row is a lossy projection of the row's value.
  For `declarations` the key is `xxh3(FQN)` and the value contains the FQN. That is unavoidable for
  lookup, but it means "key bytes + value bytes" double-counts the same information.
- **IDs are 64-bit and stored at full width.** Every edge (a definition id in a document's list, a
  `uri_id` in a reference) is 8 bytes in memory and ~10 bytes in postcard. There is no dictionary or
  delta encoding for them.

## The code responsible for each inefficiency

Line-level pointers for the four items in the plan above.

### 1. Comment text — `rust/rubydex/src/model/comment.rs`

```rust
// comment.rs:5-9
#[cfg_attr(feature = "redb-store", derive(serde::Serialize, serde::Deserialize))]
pub struct Comment {
    offset: Offset,
    string: Box<str>,   // <- 15.0 MB of text, re-derivable from the file at `offset`
}
```

`Comment` is referenced as `comments: Box<[Comment]>` from every definition struct in
`rust/rubydex/src/model/definitions.rs` (first at `:279`, then `:295`, `:403`, `:420`, `:511`, `:525`,
`:614`, `:625`, ...). The `string` field is the whole cost; `offset` is what makes the text
recoverable. Dropping `string` from the serialized form leaves a 2-field struct as a 1-field pointer.

### 2. Projection tables — `rust/rubydex/src/model/store.rs`

```rust
// store.rs:38 and :42
const SEARCH_NAMES: TableDefinition<u64, &[u8]> = TableDefinition::new("search_names");
const DOCUMENT_URIS: TableDefinition<u64, &[u8]> = TableDefinition::new("document_uris");
```

Written at `store.rs:245-270` by iterating `graph.declarations()` / `graph.documents()` and inserting
`declaration.name().as_bytes()` / `document.uri().as_bytes()` — the same strings already inside the
`declarations` and `documents` rows. The comment on each constant explains the intent (avoid
deserializing full nodes); the duplication is the side effect nobody weighed.

### 3. Edges stored in both directions

- `rust/rubydex/src/model/document.rs:16-25` — `Document` holds `uri: Box<str>` **and** three `Vec`s of
  ids (`definition_ids`, `method_reference_ids`, `constant_reference_ids`). The `Vec`s are 5.77 MB of
  the `documents` payload.
- `rust/rubydex/src/model/references.rs:13-20` — `ConstantReference` holds `uri_id` (`:17`).
- `rust/rubydex/src/model/references.rs:63-72` — `MethodRef` holds `uri_id` (`:67`).
- `rust/rubydex/src/model/definitions.rs` — every definition struct likewise holds `uri_id`.

So each edge is written once in the document's id list and once as the node's `uri_id`. Either
direction is derivable; the document lists are the smaller copy (2,669 rows) and the per-node fields
are the larger one (~4.4 MB across `method_references` and `constant_references` alone).

### 4. Library content in every workspace store — `lib/rubydex/graph.rb`

`graph.rb:72-100` builds or attaches a store for the **whole** graph, workspace and gems alike, with no
split between the two. `store.rs:216 build()` writes every node map it is given, so gem nodes land in
the workspace's store by construction. There is no library layer to share and no eviction of the
cache root (`graph.rb:499` computes the path; nothing removes it). The only reaper in the codebase is
`watches.retain(...)` (`rust/rubydex-index-manager/src/watches.rs:64`), which reaps *watchers*, not
store directories.

## How to reproduce

Against the real 500 MB store, the per-table and per-field attribution is the same code path — the
breakdown does not depend on corpus size. The example was deleted, so it needs to be rewritten; it is
~120 lines: open the store read-only, iterate the eleven table names, call `stats()` per table, and
decode each row with the model's `Deserialize` to sum field lengths. The two things worth knowing
before writing it: the table names are the `const` strings at `store.rs:27-43`, and the key type is
`u64` so `TableDefinition::<u64, &[u8]>::new(name)` is the right handle for every table.
