# Redb Storage & Build-Speed Refactor Plan

> **For agentic workers:** Execute phases strictly in order (Phase 0 → Phase 1 → Phase 2 → Phase 3A → Phase 3B → Phase 4). Every phase has explicit file targets, wire formats, pitfalls to avoid, tests to update/add, and a benchmark gate. Do **not** proceed to the next phase until the current phase compiles cleanly under **both** feature configurations, passes its tests, and passes its benchmark gate.

**Goal:** Reduce `index.redb` disk size by **~65–73%** (128.5 MB → ~35–45 MB on the 5k-file gems corpus; ~500 MB → ~140–175 MB on large Rails repos) while making cold store builds **3–5× faster** (1.11s → ~0.20–0.30s on 5k files) and workspace symbol search **faster**, with **zero regression** on hover, completion, live edits, or server RSS.

**Why both size and upfront indexing speed improve together:** On the 5,002-file gems corpus, listing + indexing + resolution takes only `0.214s`, whereas `RedbStore::build` alone takes `~1.11s` (**80% of cold build wall time**), writing 535,399 individual small B-tree entries sequentially on one thread with 535k `postcard::to_allocvec` heap allocations and 32% redb leaf-page slack. Removing redundant bytes and collapsing 535k B-tree rows into ~7.5k parallel-encoded, compressed hash-prefix buckets fixes both size and build time simultaneously.

## Progress

- [x] Phase 0.1 — `store_build` phase in `Timer` + `time_it!` around `RedbStore::build`
- [x] Phase 0.2 — Phase 0 baseline recorded (see below)
- [x] Phase 0.3 — clippy both feature sets; committed
- [x] Phase 1A — comment offsets only; `Comment::resolve_text` reads the source on demand
- [x] Phase 1B — fixed 8-byte `Id<T>` encoding; `STORE_FORMAT_VERSION = 3`
- [x] Phase 1 gate — Rust + focused Ruby tests; measured; committed
- [x] Phase 2 — `StoreError::CorruptBlock`, front-coded codec, sharded `SEARCH_NAMES`, packed `DOCUMENT_URIS`
- [x] Phase 2 gate — Rust + `test/disk_index_enumerators_test.rb`; measured; committed
- [x] Phase 3A — hash-prefix bucketed node tables + parallel build
- [x] Phase 3B — LZ4 block compression implemented, measured, **reverted**: it buys 6% of payload and
      costs 50% of fuzzy search (see "Phase 3B outcome" below)
- [x] Phase 4.1 — record `"workspace"` in `index.redb.files`
- [x] Phase 4.2 — `prune.rs` + wiring in `rubydex-index-manager`
- [x] Phase 4 gate — `cargo test -p rubydex-index-manager`, `rubocop`; committed
- [x] Final validation — `rake lint` (110 files, no offenses), full `rake test` (493 runs, 0 failures),
      `cargo test` (1167 lib tests, default features), end-to-end table in `database_diet.md`

**Measured** (30,738-file gems corpus under `Gem.dir`; redb `TableStats` page footprint, because the
`.redb` file itself grows in redb's doubling steps — see the Phase 1 note):

| | Phase 0 | Phase 1 | Phase 2 | Phase 3A (final) |
| --- | --- | --- | --- | --- |
| leaf pages (live data) | 164,461 / 673 MB | 124,318 / 509 MB | 124,318 / 509 MB | 70,520 / 288 MB |
| `stored + fragmented + metadata` | 723 MB | 646 MB | 539 MB | 415 MB |
| `.redb` file | 1028 MB | 1028 MB | 515 MB | 515 MB |
| `Store build` | 3.86 s | 3.68 s | 3.52 s | 0.67 s |
| total cold build (`--build-store`) | 5.87 s | 5.75 s | 5.55 s | 2.45 s |
| peak RSS | 1222 MB | 1257 MB | 1249 MB | 1252 MB |
| search exact / fuzzy | 12.6 / 61.0 ms | — | 6.2 / 62.1 ms | 6.7–9.7 / 61.3 ms |
| `resolve_constant` / comments / `require_paths` | 0.64 / 1.35 / 12.4 ms | — | 0.36 / 0.81 / 6.0 ms | 0.36 / 0.83 / 6.4 ms |

**Note on the `.redb` file size.** redb doubles its file as it grows
(`page_manager.rs`'s `grow`), so the file size moves in large steps and is not a live-data metric: it
stayed at 1028 MB through Phases 1 and 2 while a third of the payload went away, then dropped to
515 MB during Phase 3. The live leaf-page footprint is the honest number. 288 MB of live pages sits
just above redb's 256 MB step, which is why the file is 515 MB instead of ~290 MB; roughly 27 MB less
live data would halve it again.

### Phase 3B outcome: compression rejected on measurement

Implemented as specified (LZ4 behind a 4-byte length header on every bucket and shard), measured on
the 30k-file gems corpus:

| | no LZ4 (Phase 3A) | LZ4 |
| --- | --- | --- |
| payload stored | 292.2 MB | 277.2 MB (1.06×) |
| leaf pages | 73,866 | 70,520 |
| `.redb` file | 515 MB | 515 MB |
| `Store build` | 0.69 s | 0.71 s |
| search exact / fuzzy | 10.5 / 61.0 ms | 10.8 / 93.4 ms |
| `resolve_constant` | 0.360 ms | 0.385 ms |

Per-table ratios: `declarations` 1.18×, `documents` 1.08×, `search_names` 1.36×, and **1.00×** for
`definitions`, `method_references`, `constant_references`, `names`, `strings`, `name_dependents`.

Two facts sink it. The 2.6× compressibility measured in `database_diet.md` was mostly natural-language
comment text (17 MB of `definitions`) and the duplicated FQN/URI projection tables — Phases 1 and 2
already removed both, and what is left is dominated by uniformly-distributed 64-bit content hashes,
which LZ77 cannot match. And LZ4 has to decompress a whole ~10 KB bucket to answer one point read, so
the fuzzy search path (48k point reads) pays ~0.6 µs per read: 61 ms → 93 ms. A 6% payload cut that
does not even move the file size (redb doubles its file; see the note above) is not worth a 50%
regression on search.

**Background analysis:** `docs/disk-persisted-index/database_diet.md`, `docs/disk-persisted-index/perf-and-cleanup.md`, `docs/disk-persisted-index/memory-baseline.md`.

---

## Global Constraints & Compiler Traps (READ FIRST)

1. **`clippy::pedantic` is promoted to errors.** `rust/Cargo.toml` sets `pedantic = "warn"` and CI runs `cargo clippy --workspace --all-targets -- -D warnings`. Watch out for:
   - `cast_possible_truncation` / `cast_sign_loss`: use `u32::try_from(...)` / `u16::try_from(...)` instead of `as u32` / `as u16`, or add a narrow `#[allow(clippy::cast_possible_truncation)]` only with a range proof comment.
   - `missing_errors_doc` / `missing_panics_doc`: every `pub fn` returning `Result` needs `# Errors`; every `pub fn` that can panic needs `# Panics`.
   - `must_use_candidate`: put `#[must_use]` on pure `pub fn`s.
   - `doc_markdown`: wrap technical identifiers (`redb`, `postcard`, `LZ4`, `UTF-8`, `UriId`, etc.) in backticks inside `///` doc comments.
2. **Dual feature builds are mandatory.** Every Rust verification step must pass both:
   - Default (no `redb-store`): `cd rust && cargo clippy --workspace --all-targets -- -D warnings`
   - With `redb-store`: `cd rust && cargo clippy --workspace --all-targets --features rubydex/redb-store,rubydex-sys/redb-store -- -D warnings`
   - Any new `#[cfg(feature = "redb-store")]` method on `Graph` needs a matching `#[cfg(not(feature = "redb-store"))]` stub with `#[allow(clippy::unused_self)]` and leading-underscore unused parameters.
3. **Never panic across `extern "C"`.** Any corrupt byte sequence read from redb (bad header, truncated varint, invalid UTF-8, out-of-range offset, corrupt LZ4 block, huge uncompressed length) MUST return `Err(StoreError::...)` — never `unwrap()`, `expect()`, or unchecked slice indexing `&buf[a..b]`. Always use `.get(a..b)` and check bounds.
4. **Never allocate unbounded memory from untrusted store headers.** When decoding any length prefix (varint, `u32` count, or LZ4 `uncompressed_len`), validate against an explicit cap (`MAX_BUCKET_UNCOMPRESSED_BYTES`, `bytes.len()`, etc.) before calling `Vec::with_capacity` or `vec![0; len]`.
5. **Recompile before Ruby tests.** After any Rust change, run `bundle exec rake compile` (or `bundle exec rake compile_release` before benchmarking!) before running Ruby tests.
6. **Run focused Ruby tests during iteration; save `bundle exec rake test` for final validation.** Full `ruby_test` is slow. Run targeted files with:
   ```bash
   GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest test/specific_test.rb
   ```
   Run `bundle exec rake test` once at the very end.
7. **Ruby style:** Blank line after `# frozen_string_literal: true`, double quotes, trailing commas in multiline calls. Run `bundle exec rubocop <edited_files>` on edited Ruby files. Ruby 4.0 does not ship `benchmark` as a default gem; use `Process.clock_gettime(Process::CLOCK_MONOTONIC)`.
8. **Git commits:** Write commit messages to a temp file and pass `git commit -F /tmp/msg.txt`.
9. **Format version:** Bump `STORE_FORMAT_VERSION` in `rust/rubydex/src/model/store.rs:120` from `2` to `3` in Phase 1 (the first breaking schema change). Keep it at `3` through Phases 1–3B if landing on one branch before release (or bump once per merged PR).

## Explicit "DO NOT" Guardrails

- **DO NOT delete `SEARCH_NAMES` and scan `DECLARATIONS` instead.** `perf-and-cleanup.md` Task 5 measured scanning `DECLARATIONS` as **10× slower** (42.0 ms vs. 4.1 ms). Phase 2 replaces `SEARCH_NAMES` with sharded front-coded blobs (`SEARCH_SHARDS`) so it is both **~5× smaller AND faster**.
- **DO NOT remove `Document`'s ID lists (`definition_ids`, `method_reference_ids`, `constant_reference_ids`) or node `uri_id`s.** `graph.rs:1961-2078` needs `Document -> node IDs` in $O(\text{nodes in file})$ on every live edit, and hover/find-refs needs `node -> uri_id` in $O(1)$.
- **DO NOT group point-lookup tables by `Document`.** That would break $O(1)$ lookup by `Id<T>`. Phase 3 uses **hash-prefix bucketing** (`bucket_id = id >> shift`), which preserves $O(1)$ single-read lookup with zero secondary index.
- **DO NOT call `String::truncate(shared_prefix_len)` on front-coded UTF-8 strings.** Two valid UTF-8 strings can share leading bytes of a multi-byte character (e.g. `é` `[0xC3, 0xA9]` vs `è` `[0xC3, 0xA8]` share `0xC3`), which makes `String::truncate` panic. Decode front-coded strings into a `Vec<u8>` and call `std::str::from_utf8(&buf)` *after* appending the suffix.
- **DO NOT write `key >> (64 - bucket_bits)` without guarding `bucket_bits == 0`.** Shifting a `u64` right by `64` panics in debug mode and wraps in release mode. Always use a helper `bucket_id(key: u64, bits: u8) -> u32`.
- **DO NOT call `serializer.serialize_bytes(&bytes)` for `Id<T>`.** In `postcard`, `serialize_bytes` prepends a 1-byte length prefix (9 bytes total). Serialize `[u8; 8]` directly via `<[u8; 8] as serde::Serialize>::serialize` to emit **exactly 8 bytes**.
- **DO NOT call `lz4_flex::block::decompress_size_prepended`.** A corrupted 4-byte length prefix (`0xFFFFFFFF`) causes an immediate 4 GiB OOM abort. Store the `u32` uncompressed length explicitly, validate `<= MAX_BUCKET_UNCOMPRESSED_BYTES`, and call `lz4_flex::block::decompress_into`.

---

## Scope and File Map

| File | Responsibility | Phases |
| --- | --- | --- |
| `rust/rubydex/src/stats/timer.rs` | Add `store_build` timing phase to `make_timer!` | 0 |
| `rust/rubydex/src/main.rs` | Wrap `RedbStore::build` in `time_it!(store_build, ...)` | 0 |
| `rust/rubydex/src/model/comment.rs` | `string: Option<Box<str>>` with `#[serde(skip)]`, `resolve_text`, `byte_len` | 1A |
| `rust/rubydex/src/model/document.rs` | Helper to read source once for `LineIndex` + comment extraction | 1A |
| `rust/rubydex/src/model/graph.rs` | Use `c.byte_len()` in `print_query_statistics` (`:2466`) | 1A |
| `rust/rubydex/src/indexing/ruby_indexer.rs` | Avoid extra `String` alloc on `# @deprecated` check (`:1544`) | 1A |
| `rust/rubydex-sys/src/definition_api.rs` | Resolve store-backed comments from document source once per definition (`:241`) | 1A |
| `rust/rubydex/src/model/id.rs` | Fixed 8-byte `[u8; 8]` serde impl for `Id<T>` (`:95`) | 1B |
| `rust/rubydex/src/model/store.rs` | `STORE_FORMAT_VERSION = 3`, `StoreError::CorruptBlock`, front-coded `SEARCH_SHARDS` & `DOCUMENT_URIS`, hash-prefix bucketed tables, parallel build, LZ4 compression | 1, 2, 3A, 3B |
| `rust/rubydex/Cargo.toml` | Add optional `lz4_flex` behind `redb-store` feature | 3B |
| `rust/rubydex/tests/store_roundtrip.rs` | Assert resolved comment text round-trips for store-backed definitions | 1A |
| `lib/rubydex/graph.rb` | Record `"workspace"` in `index.redb.files` JSON manifest (`:763`) | 4 |
| `rust/rubydex-index-manager/src/prune.rs` (new) & `main.rs` | Periodic safe eviction of deleted-workspace cache directories | 4 |

---

## Phase 0: Measurement Harness & Build-Stage Timing

**Goal:** Make `RedbStore::build` visible in `--stats` and establish a reproducible before/after baseline.

### Step 0.1 — Add `store_build` phase to `Timer`
- **File:** `rust/rubydex/src/stats/timer.rs` (bottom `make_timer!` invocation around line 121).
- Read lines 115–130 of `rust/rubydex/src/stats/timer.rs` first. Add:
  ```rust
  store_build, "Store build";
  ```
  right after `resolution, "Resolution";`.
- **File:** `rust/rubydex/src/main.rs:221-226`.
  Wrap `RedbStore::build` in `time_it!`:
  ```rust
  #[cfg(feature = "redb-store")]
  if let Some(path) = args.build_store.as_deref() {
      time_it!(store_build, {
          rubydex::model::store::RedbStore::build(std::path::Path::new(path), &graph).expect("build store");
      });
      println!("Built redb store at {path}");
  }
  ```

### Step 0.2 — Record the Phase 0 baseline
Build release CLI and measure on the Ruby gems tree (or `$DEFAULT_BENCH_WORKSPACE` if set):
```bash
cd rust && cargo build --release --features redb-store --bin rubydex_cli
cd ..
CORPUS="${DEFAULT_BENCH_WORKSPACE:-$(ruby -e 'puts Gem.dir')/gems}"
./rust/target/release/rubydex_cli --stats --build-store /tmp/rdx-phase0.redb "$CORPUS"
ls -la /tmp/rdx-phase0.redb
```
Also record Ruby-side query timings (hover + comments + search + `require_paths`) using a one-liner:
```bash
bundle exec rake compile_release
bundle exec ruby -Ilib -rrubydex -e '
  g = Rubydex::Graph.new
  g.attach_store("/tmp/rdx-phase0.redb")
  t = ->(label, n, &blk) {
    t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    n.times(&blk)
    dt = (Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0) * 1000.0 / n
    puts format("%-24s %.3f ms/op (%d runs)", label, dt, n)
  }
  t.call("search exact (String)", 20) { g.search("String") }
  t.call("search fuzzy (arbs)",   20) { g.fuzzy_search("arbs") }
  t.call("resolve_constant",      200) { g.resolve_constant("Object", []) }
  t.call("comments (hover)",      50) {
    d = g["Object"]&.definitions&.first
    d&.comments
  }
  t.call("require_paths",         10) { g.require_paths([]) }
'
rm -f /tmp/rdx-phase0.redb
```
Record all numbers in a scratch table before starting Phase 1.

### Step 0.3 — Verify & commit Phase 0
- `cd rust && cargo clippy --workspace --all-targets -- -D warnings && cargo clippy --workspace --all-targets --features rubydex/redb-store,rubydex-sys/redb-store -- -D warnings`
- Commit `timer.rs` and `main.rs` (`Time RedbStore::build as its own phase in --stats`).

---

## Phase 1: Omit Comment Text on Disk + Fixed 8-Byte `Id<T>` Encoding

**Target:** ~25–28% smaller store (~128.5 MB → ~96 MB), ~20–25% faster build, zero query latency regression.

### Part 1A — Persist comment offsets only; resolve text from disk on demand

#### Exact design
1. **`rust/rubydex/src/model/comment.rs`**:
   - Change the struct to:
     ```rust
     #[derive(Debug, Clone)]
     #[cfg_attr(feature = "redb-store", derive(serde::Serialize, serde::Deserialize))]
     pub struct Comment {
         offset: Offset,
         #[cfg_attr(feature = "redb-store", serde(skip))]
         string: Option<Box<str>>,
     }
     assert_mem_size!(Comment, 24);
     ```
     *Why `Option<Box<str>>`?* Niche optimization keeps `size_of::<Option<Box<str>>>() == 16`, so `assert_mem_size!(Comment, 24)` passes unchanged!
   - Methods on `Comment`:
     - `pub fn new(offset: Offset, string: Box<str>) -> Self { Self { offset, string: Some(string) } }`
     - `pub fn string(&self) -> &str { self.string.as_deref().unwrap_or("") }` — keeps all existing in-memory callers and tests compiling with zero signature churn.
     - `#[must_use] pub fn has_inline_text(&self) -> bool { self.string.is_some() }`
     - `#[must_use] pub fn byte_len(&self) -> usize { self.string.as_deref().map_or_else(|| (self.offset.end().saturating_sub(self.offset.start())) as usize, str::len) }`
     - `#[must_use] pub fn resolve_text<'a>(&'a self, source: Option<&'a str>) -> &'a str`:
       ```rust
       #[must_use]
       pub fn resolve_text<'a>(&'a self, source: Option<&'a str>) -> &'a str {
           if let Some(inline) = self.string.as_deref() {
               return inline;
           }
           let Some(src) = source else {
               return "";
           };
           let start = self.offset.start() as usize;
           let end = self.offset.end() as usize;
           if start <= end && end <= src.len() && src.is_char_boundary(start) && src.is_char_boundary(end) {
               src[start..end].trim()
           } else {
               ""
           }
       }
       ```
       *Why check `is_char_boundary` and `end <= src.len()`?* If the file on disk was edited or truncated since the store was built, slicing raw byte offsets could panic across `extern "C"`. Returning `""` (or checking `content_hash`) is 100% panic-proof.
       *Wait — check `.trim()` fidelity:* In `ruby_indexer.rs:1551` and `ruby_builder.rs:1400`, `Comment::new` stores `text.trim().into()` for `Offset::from_prism_location(&comment.location())`. In `rbs_indexer.rs:200-203`, `offset` already points to `&line[line_indent..]` (already trimmed). So `src[start..end].trim()` matches both Ruby and RBS comments identically.

2. **`rust/rubydex/src/model/document.rs`**:
   - Add a small helper for reading a document's source from disk when needed for comment resolution:
     ```rust
     /// Reads the document's source text from disk if its URI is a `file://` path that still exists.
     /// Returns `None` for synthetic/non-file URIs or unreadable files.
     #[must_use]
     pub fn read_source(&self) -> Option<String> {
         let path = self.file_path()?;
         std::fs::read_to_string(path).ok()
     }
     ```
   - *Note on `content_hash` staleness:* Check `xxh3_64(source.as_bytes()) == self.content_hash`? Think carefully: if a user is hovering over a store-backed gem file, the file hasn't changed so the hashes match. If a workspace file was edited externally *before* refresh ran, returning `""` on hash mismatch avoids showing garbled text from shifted offsets. **Decision:** verify `xxh3_64(source.as_bytes()) == self.content_hash` inside `read_matching_source(&self) -> Option<String>` (if the file changed on disk, offsets are stale anyway; returning `None` safely yields `""` rather than slicing wrong code!). Wait — verify `rbs_indexer.rs` and `ruby_indexer.rs` both construct `Document::new(uri, source)` which sets `content_hash = xxh3_64(source.as_bytes())` (confirmed in `document.rs:67`). So `read_matching_source` is exact!

3. **`rust/rubydex-sys/src/definition_api.rs:241-265` (`rdx_definition_comments`)**:
   - Currently:
     ```rust
     let mut entries = defn
         .comments()
         .iter()
         .map(|c| CommentEntry {
             string: OwnedString::new(c.string()),
             location: create_location_for_uri_and_offset(graph, &document, c.offset()),
         })
         .collect::<Vec<CommentEntry>>()
         .into_boxed_slice();
     ```
   - Change to read source **at most once** per call, and **only** when a comment lacks inline text (so in-memory/overlay definitions with synthetic URIs like `test_definition_comments_with_nul_byte` never touch disk!):
     ```rust
     let comments = defn.comments();
     let source = if comments.iter().any(|c| !c.has_inline_text()) {
         document.read_matching_source()
     } else {
         None
     };

     let mut entries = comments
         .iter()
         .map(|c| CommentEntry {
             string: OwnedString::new(c.resolve_text(source.as_deref())),
             location: create_location_for_uri_and_offset(graph, &document, c.offset()),
         })
         .collect::<Vec<CommentEntry>>()
         .into_boxed_slice();
     ```

4. **`rust/rubydex/src/model/graph.rs:2466` (`print_query_statistics`)**:
   - Replace `c.string().len()` with `c.byte_len()`, so `--stats` reports accurate documentation size even on store-backed graphs.

5. **`rust/rubydex/src/indexing/ruby_indexer.rs:1544-1553` & `rust/rubydex/src/operation/ruby_builder.rs:1395-1404`**:
   - Avoid the intermediate `.to_string()` allocation before `.trim().into()`:
     ```rust
     let slice = comment.location().as_slice();
     if slice.split(|&b| b == b'\n').any(|line| line.starts_with(b"# @deprecated")) {
         self.deprecated = true;
     }
     let text = String::from_utf8_lossy(slice);
     self.comments.push(Comment::new(
         Offset::from_prism_location(&comment.location()),
         text.trim().into(),
     ));
     ```

6. **Tests to update / add for Part 1A:**
   - `rust/rubydex/tests/store_roundtrip.rs:165-177`: Notice `postcard::to_allocvec(definition) == postcard::to_allocvec(&*loaded)` will STILL pass because `serde(skip)` omits `string` on both sides. **Add an explicit check right below it** verifying comment text recovery:
     ```rust
     if !definition.comments().is_empty() {
         let doc = store_backed.document(*definition.uri_id()).expect("doc present");
         let src = doc.read_matching_source();
         let before_texts: Vec<&str> = definition.comments().iter().map(|c| c.string()).collect();
         let after_texts: Vec<&str> = loaded
             .comments()
             .iter()
             .map(|c| c.resolve_text(src.as_deref()))
             .collect();
         if before_texts != after_texts {
             failures.push(format!("definition {} comment text differs: {before_texts:?} vs {after_texts:?}", id.get()));
         }
     }
     ```
   - Add a Ruby test (in `test/disk_index_corrupt_store_test.rb` or a new test method in `test/disk_index_live_edit_test.rb`) that builds a disk-backed graph on a real file with `# Doc comment\nclass Foo; end`, attaches the store, calls `definition.comments`, and asserts `["# Doc comment"]` is returned; then edits the file on disk without refreshing and asserts `definition.comments` does not crash.

### Part 1B — Fixed 8-byte `Id<T>` wire encoding + `STORE_FORMAT_VERSION = 3`

1. **`rust/rubydex/src/model/id.rs:95-108`**:
   - Replace the manual `Serialize` / `Deserialize` impls with fixed 8-byte little-endian array encoding:
     ```rust
     // Manual serde impls: serialize as a fixed `[u8; 8]` (8 raw bytes in postcard) rather than
     // `serialize_u64` (LEB128 varint, which averages 9.49 bytes for uniform 64-bit hashes).
     // Do NOT use `serialize_bytes` — postcard prefixes byte slices with a length varint (9 bytes).
     #[cfg(feature = "redb-store")]
     impl<T> serde::Serialize for Id<T> {
         fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
             self.value.get().to_le_bytes().serialize(serializer)
         }
     }

     #[cfg(feature = "redb-store")]
     impl<'de, T> serde::Deserialize<'de> for Id<T> {
         fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
             let bytes = <[u8; 8]>::deserialize(deserializer)?;
             Ok(Self::new(u64::from_le_bytes(bytes)))
         }
     }
     ```
   - Add a unit test in `rust/rubydex/src/model/id.rs`:
     ```rust
     #[cfg(feature = "redb-store")]
     #[test]
     fn id_serializes_to_exactly_eight_bytes_in_postcard() {
         let id = Id::<()>::from("Object");
         let encoded = postcard::to_allocvec(&id).expect("encode");
         assert_eq!(encoded.len(), 8);
         let decoded: Id<()> = postcard::from_bytes(&encoded).expect("decode");
         assert_eq!(decoded, id);
     }
     ```

2. **`rust/rubydex/src/model/store.rs:120`**:
   - Bump `pub const STORE_FORMAT_VERSION: u32 = 3;`.

### Phase 1 Verification & Gate
1. Rust tests + clippy (both features):
   ```bash
   cd rust
   cargo test -p rubydex --features redb-store model::id:: model::store::tests
   cargo test -p rubydex --features redb-store --test store_roundtrip --test differential_store
   cargo clippy --workspace --all-targets -- -D warnings
   cargo clippy --workspace --all-targets --features rubydex/redb-store,rubydex-sys/redb-store -- -D warnings
   ```
2. Ruby focused tests:
   ```bash
   bundle exec rake compile
   GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest -e 'ARGV.each { |f| require File.expand_path(f) }' test/definition_test.rb test/disk_index_live_edit_test.rb test/disk_index_corrupt_store_test.rb
   ```
3. Benchmark gate: re-run the Phase 0 measurement commands. Expect **≥20% smaller `.redb` file**, **faster `Store build`**, and **no regression on search/hover**.
4. Commit Phase 1 (`Omit comment text from redb store and encode Ids as fixed 8 bytes`).

---

## Phase 2: Front-Coded Packed `DOCUMENT_URIS` and Sharded `SEARCH_NAMES`

**Target:** Replace ~85k–400k individual projection B-tree rows with **1** `DOCUMENT_URIS` row and **32** `SEARCH_NAMES` shard rows → ~5.5 MB smaller on gems (~25 MB on stdlib), faster build, and **2–5× faster workspace symbol search** with lower transient RSS.

### Error Variant Addition (`rust/rubydex/src/model/store.rs`)
Add a corrupt-block error variant to `StoreError` (around line 52):
```rust
/// A packed shard or bucket failed structural validation or decompression.
CorruptBlock {
    table: &'static str,
    key: u32,
    reason: &'static str,
},
```
Update `Display` and `Error::source` (`None` for `CorruptBlock`). Because `Graph::scan_store` and `Graph::store_ids` are already generic over `E: std::fmt::Display`, changing `document_uris`, `search_names`, `declaration_ids_matching`, and `declaration_ids_matching_parallel` to return `Result<..., StoreError>` requires **zero changes to `Graph` signatures**.

### Shared Front-Coding Codec (private module or helpers in `store.rs`)

Use the exact same front-coded stream format for both `DOCUMENT_URIS` and `SEARCH_NAMES` shards!

#### Binary Wire Format of a Front-Coded Block
Inputs MUST be sorted lexicographically by `text: &[u8]` before encoding:
```
[entry_count: u32 LE]
Repeated entry_count times:
  [shared_prefix_len: varint u32 (LEB128)]
  [suffix_len:        varint u32 (LEB128)]
  [suffix:            suffix_len bytes]
  [id:                u64 LE (8 bytes)]
```

#### Exact Helper Functions (implement verbatim to avoid UTF-8 / overflow bugs)
```rust
fn write_varint_u32(out: &mut Vec<u8>, mut value: u32) {
    while value >= 0x80 {
        out.push(((value & 0x7f) as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn read_varint_u32(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = *bytes.get(*pos)?;
        *pos += 1;
        if shift >= 32 || (shift == 28 && byte > 0x0f) {
            return None;
        }
        result |= u32::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Some(result);
        }
        shift += 7;
    }
}

fn encode_front_coded(entries: &[(&[u8], u64)], out: &mut Vec<u8>) -> Result<(), StoreError> {
    out.clear();
    let count = u32::try_from(entries.len()).map_err(|_| StoreError::CorruptBlock {
        table: "front_coded",
        key: 0,
        reason: "block entry count exceeds u32::MAX",
    })?;
    out.extend_from_slice(&count.to_le_bytes());
    let mut prev: &[u8] = &[];
    for &(text, id) in entries {
        let shared = prev.iter().zip(text.iter()).take_while(|(a, b)| a == b).count();
        let suffix = &text[shared..];
        write_varint_u32(out, u32::try_from(shared).unwrap_or(u32::MAX));
        write_varint_u32(out, u32::try_from(suffix.len()).unwrap_or(u32::MAX));
        out.extend_from_slice(suffix);
        out.extend_from_slice(&id.to_le_bytes());
        prev = text;
    }
    Ok(())
}

/// Streams `(id, &str)` from a front-coded block using a single reused `Vec<u8>` buffer.
/// Never panics on corrupt input: validates bounds and UTF-8 on every entry.
fn decode_front_coded(
    table: &'static str,
    shard_key: u32,
    bytes: &[u8],
    mut visit: impl FnMut(u64, &str),
) -> Result<(), StoreError> {
    let err = |reason| StoreError::CorruptBlock {
        table,
        key: shard_key,
        reason,
    };
    let count_bytes: [u8; 4] = bytes.get(0..4).ok_or_else(|| err("truncated header"))?.try_into().unwrap();
    let count = u32::from_le_bytes(count_bytes);
    let mut pos = 4usize;
    let mut buf = Vec::<u8>::with_capacity(256);

    for _ in 0..count {
        let shared = read_varint_u32(bytes, &mut pos).ok_or_else(|| err("truncated shared len"))? as usize;
        let suffix_len = read_varint_u32(bytes, &mut pos).ok_or_else(|| err("truncated suffix len"))? as usize;
        if shared > buf.len() {
            return Err(err("shared prefix exceeds previous entry"));
        }
        let suffix = bytes.get(pos..pos.checked_add(suffix_len).ok_or_else(|| err("suffix overflow"))?)
            .ok_or_else(|| err("truncated suffix"))?;
        pos += suffix_len;
        let id_slice = bytes.get(pos..pos.checked_add(8).ok_or_else(|| err("id overflow"))?)
            .ok_or_else(|| err("truncated id"))?;
        pos += 8;
        let id = u64::from_le_bytes(id_slice.try_into().unwrap());

        // Safe on Vec<u8>: multi-byte UTF-8 prefixes truncate cleanly at byte level, then
        // appending `suffix` restores a complete UTF-8 string before `from_utf8` checks it.
        buf.truncate(shared);
        buf.extend_from_slice(suffix);
        let text = std::str::from_utf8(&buf).map_err(|_| err("invalid UTF-8 in front-coded entry"))?;
        visit(id, text);
    }
    Ok(())
}
```

### Table Definition & Build Changes (`rust/rubydex/src/model/store.rs`)

1. Change the two projection table definitions (lines 38 and 42):
   ```rust
   const SEARCH_SHARD_COUNT: usize = 32;
   const SEARCH_NAMES: TableDefinition<u32, &[u8]> = TableDefinition::new("search_names");
   const DOCUMENT_URIS: TableDefinition<u32, &[u8]> = TableDefinition::new("document_uris");
   ```
2. **Writing `SEARCH_NAMES` in `RedbStore::build`:**
   - Collect `let mut entries: Vec<(&[u8], u64)> = graph.declarations().iter().map(|(id, d)| (d.name().as_bytes(), id.get())).collect();`
   - `entries.sort_unstable_by_key(|(name, _)| *name);`
   - Split into up to `SEARCH_SHARD_COUNT` contiguous chunks (`chunk_size = entries.len().div_ceil(SEARCH_SHARD_COUNT).max(1)`).
   - Encode chunks in parallel via `std::thread::scope` (or sequentially if `entries.len() < 2048`), then insert `(shard_idx as u32, blob.as_slice())` into `SEARCH_NAMES`.
3. **Writing `DOCUMENT_URIS` in `RedbStore::build`:**
   - Collect `let mut entries: Vec<(&[u8], u64)> = graph.documents().iter().map(|(id, d)| (d.uri().as_bytes(), id.get())).collect();`
   - `entries.sort_unstable_by_key(|(uri, _)| *uri);`
   - Encode into a single `Vec<u8>` and insert at key `0_u32` into `DOCUMENT_URIS`.
4. **Reading `document_uris(&self) -> Result<Vec<(UriId, String)>, StoreError>`:**
   - Open `DOCUMENT_URIS`, get key `0_u32`. If `None` (empty store), return `Ok(Vec::new())`.
   - Call `decode_front_coded("document_uris", 0, guard.value(), |id, uri| uris.push((UriId::new(id), uri.to_owned())))?;`
5. **Reading `declaration_ids_matching_parallel(&self, predicate)`:**
   - Open `SEARCH_NAMES`? Wait: each worker thread opens its own `ReadTransaction` and reads its assigned subset of shard keys (`shard_idx % num_threads == worker_idx`, or contiguous ranges `start..end` of `0..shard_count`).
   - First check shard count via `let shard_count = u32::try_from(table.len()?).unwrap_or(0);` (or iterate `0..SEARCH_SHARD_COUNT as u32` skipping `None`).
   - Each worker decodes its shards via `decode_front_coded("search_names", shard, bytes, |raw_id, name| { let id = DeclarationId::new(raw_id); if predicate(&id, name) { matches.push(id); } })`.
   - Merge results across workers (`results?.concat()`).
6. **Reading `#[cfg(test)] search_names` and `#[cfg(test)] declaration_ids_matching`:**
   - Iterate `table.range(..=u32::MAX)?` and call `decode_front_coded`.

### Existing Tests Affected in Phase 2 (CRITICAL — CHECK THESE!)
- **`parallel_scan_matches_the_serial_scan` (`store.rs:943-965`)**:
  - Previously asserted `parallel.windows(2).all(|pair| pair[0] < pair[1])` because shards were split by `DeclarationId` (`u64`) key range.
  - Now shards are split by lexicographical FQN order, so IDs are **unique** (disjoint shards) and match `serial` as a `BTreeSet`, and even match `serial` in exact order if workers process contiguous shard ranges `[start_shard..end_shard)`!
  - **Update that assertion** to check `parallel.len() == serial.len()` (no duplicates) and `BTreeSet` equality (or exact `parallel == serial` since contiguous shard ranges preserve exact lexicographical order!), and remove the stale `pair[0] < pair[1]` `DeclarationId`-order check.
- **`declaration_ids_matching_filters_during_scan` (`store.rs:1162-1193`)**: passes unchanged if `search_names()` and `declaration_ids_matching()` both iterate shards in ascending shard key order!
- **Add new unit tests:**
  1. Front-coding round-trip with **non-ASCII UTF-8 strings sharing multi-byte leading bytes** (e.g. `"Café"`, `"Cafè"`, `"日本語::A"`, `"日本語::B"`) — proves `Vec<u8>` decoding never panics on shared partial UTF-8 sequences!
  2. Corrupt `SEARCH_NAMES` shard (invalid UTF-8, truncated varint, `shared > buf.len()`) returns `Err(StoreError::CorruptBlock { .. })` and increments `graph.store_error_count()` without panicking.

### Phase 2 Verification & Gate
- Run Rust tests + dual-feature clippy + `test/disk_index_enumerators_test.rb`.
- Re-run Phase 0 benchmark commands:
  - Expect **~5 MB further drop in store size**, **faster `Store build`**, **faster `search exact` and `search fuzzy`**, and **faster `require_paths`**.
- Commit Phase 2 (`Pack SEARCH_NAMES and DOCUMENT_URIS into front-coded shards`).

---

## Phase 3A: Hash-Prefix Bucketed Node Tables + Parallel Build (Zero New Deps)

**Target:** Collapse 450k+ remaining B-tree rows into ~7,400 bucket rows, eliminate most of redb's 32% leaf fragmentation, and parallelize `RedbStore::build` across CPU cores — **cutting build time by 2–4×** and cutting file size further *before* even adding compression.

> Keeping Phase 3A (bucketing + parallel build) separate from Phase 3B (LZ4 compression) makes debugging trivial: if a test fails in 3A, it is purely a bucket-format bug; if a test fails in 3B, it is purely compression.

### Table Definitions & Metadata Layout (`rust/rubydex/src/model/store.rs`)

1. **Change all 8 node tables from `u64` keys to `u32` bucket keys:**
   ```rust
   const STRINGS: TableDefinition<u32, &[u8]> = TableDefinition::new("strings");
   const DECLARATIONS: TableDefinition<u32, &[u8]> = TableDefinition::new("declarations");
   const DEFINITIONS: TableDefinition<u32, &[u8]> = TableDefinition::new("definitions");
   const NAMES: TableDefinition<u32, &[u8]> = TableDefinition::new("names");
   const CONSTANT_REFERENCES: TableDefinition<u32, &[u8]> = TableDefinition::new("constant_references");
   const METHOD_REFERENCES: TableDefinition<u32, &[u8]> = TableDefinition::new("method_references");
   const DOCUMENTS: TableDefinition<u32, &[u8]> = TableDefinition::new("documents");
   const NAME_DEPENDENTS: TableDefinition<u32, &[u8]> = TableDefinition::new("name_dependents");
   ```
2. **Store per-table `bucket_bits` in `STORE_METADATA` (`TableDefinition<u8, u32>`):**
   ```rust
   const META_FORMAT_VERSION: u8 = 0;
   const META_BITS_STRINGS: u8 = 1;
   const META_BITS_DECLARATIONS: u8 = 2;
   const META_BITS_DEFINITIONS: u8 = 3;
   const META_BITS_NAMES: u8 = 4;
   const META_BITS_CONSTANT_REFS: u8 = 5;
   const META_BITS_METHOD_REFS: u8 = 6;
   const META_BITS_DOCUMENTS: u8 = 7;
   const META_BITS_NAME_DEPENDENTS: u8 = 8;

   const MAX_BUCKET_BITS: u8 = 16; // max 65,536 buckets per table
   const TARGET_ENTRIES_PER_BUCKET: usize = 64;
   const TARGET_DOCS_PER_BUCKET: usize = 16; // Documents are larger (~1.2 KB each)
   ```
3. **Cache shifts on `RedbStore` so reads pay ZERO metadata lookups:**
   ```rust
   #[derive(Debug, Clone, Copy, Default)]
   struct BucketBits {
       strings: u8,
       declarations: u8,
       definitions: u8,
       names: u8,
       constant_references: u8,
       method_references: u8,
       documents: u8,
       name_dependents: u8,
   }

   pub struct RedbStore {
       db: StoreDb,
       bits: BucketBits,
   }
   ```
   - On `RedbStore::open`: read keys `1..=8` in the same metadata `ReadTransaction` that already checks `META_FORMAT_VERSION`! Validate each value `<= u32::from(MAX_BUCKET_BITS)`; return `Err(StoreError::Open(...))` if missing or out of range.
   - On `#[cfg(test)] RedbStore::create`: write `META_FORMAT_VERSION = STORE_FORMAT_VERSION` and keys `1..=8 = 0` (`BucketBits::default()`, meaning 1 bucket per table).

4. **Bucket ID helper (MUST guard `bits == 0`!):**
   ```rust
   #[inline]
   fn bucket_bits_for_count(count: usize, target_per_bucket: usize) -> u8 {
       if count <= target_per_bucket {
           return 0;
       }
       let buckets = count.div_ceil(target_per_bucket).next_power_of_two();
       #[allow(clippy::cast_possible_truncation)] // clamped to MAX_BUCKET_BITS (16)
       let bits = buckets.trailing_zeros().min(u32::from(MAX_BUCKET_BITS)) as u8;
       bits
   }

   #[inline]
   fn bucket_id(key: u64, bits: u8) -> u32 {
       if bits == 0 {
           0
       } else {
           #[allow(clippy::cast_possible_truncation)] // bits <= 16, so result < 65536 <= u32::MAX
           let id = (key >> (64 - u32::from(bits))) as u32;
           id
       }
   }
   ```

### Exact Bucket Binary Wire Format (Uncompressed in 3A)

```
Header:
  [count:   u32 LE]                       (4 bytes)
  [keys:    count × u64 LE, sorted asc]   (8 * count bytes)
  [offsets: (count + 1) × u32 LE]         (4 * (count + 1) bytes, byte offsets into payload)
Body:
  [payload: offsets[count] bytes]         (concatenated postcard-encoded values)
```
*Why `u32` count instead of `u16`?* On tiny tables or `RedbStore::create` (where `bits == 0`, 1 bucket holds everything), a test could theoretically insert >65,535 entries into bucket 0; `u32` costs 2 extra bytes per bucket (15 KB across the whole DB) and eliminates any `u16` overflow edge case.

#### Exact Bucket Lookup & Iteration Helpers
Implement these four functions cleanly in `store.rs`:
1. `encode_bucket<V: serde::Serialize>(entries: &mut [(u64, &V)], scratch_payload: &mut Vec<u8>, out: &mut Vec<u8>) -> Result<(), StoreError>`:
   - `entries.sort_unstable_by_key(|(k, _)| *k);`
   - Clear `scratch_payload` and `out`.
   - Write `count` (`u32::try_from(entries.len())`), then all `keys`, then reserve space for `(count + 1) * 4` offset bytes (or collect `offsets: Vec<u32>` into a reused `Vec<u32>`!).
   - For each `(_, value)`, record `offset = u32::try_from(scratch_payload.len())`, then `postcard::to_extend(value, scratch_payload)` (reusing the same `Vec<u8>` — **ZERO per-node heap allocations!**).
   - Write final `scratch_payload.len()` as `offsets[count]`, then copy `offsets` and `scratch_payload` into `out`.
2. `lookup_in_bucket<'a>(table: &'static str, bkey: u32, bucket: &'a [u8], key: u64) -> Result<Option<&'a [u8]>, StoreError>`:
   - Validate header bounds:
     - `count` from `bucket.get(0..4)`
     - `keys_start = 4`, `keys_end = 4 + count * 8` (using `checked_mul` / `checked_add`!)
     - `offsets_start = keys_end`, `offsets_end = offsets_start + (count + 1) * 4`
     - `payload = bucket.get(offsets_end..)`
   - Binary search `0..count` reading `u64::from_le_bytes` at `keys_start + mid * 8`.
   - On match at index `i`: read `start = offset[i]`, `end = offset[i + 1]`, validate `start <= end && end <= payload.len()`, return `Ok(Some(&payload[start..end]))`.
3. `iter_bucket_entries<'a>(table: &'static str, bkey: u32, bucket: &'a [u8], mut visit: impl FnMut(u64, &'a [u8]) -> Result<(), StoreError>) -> Result<(), StoreError>`:
   - Validates the exact same header once, then loops `i in 0..count`, yielding `(key_i, &payload[start_i..end_i])`.
4. `iter_bucket_keys(table: &'static str, bkey: u32, bucket: &[u8], out: &mut Vec<u64>) -> Result<(), StoreError>`:
   - Validates header bounds, reserves `count`, pushes each `key_i` without touching `offsets` or `payload`.

### Parallel Build Pipeline in `RedbStore::build`
Replace `write_map!` with a helper function:
```rust
fn write_bucketed_map<Id, V>(
    write_txn: &redb::WriteTransaction,
    table_def: TableDefinition<u32, &[u8]>,
    map: &IdentityHashMap<Id, V>,
    target_per_bucket: usize,
) -> Result<u8, StoreError>
where
    Id: Copy + std::ops::Deref<Target = u64> + Sync,
    V: serde::Serialize + Sync,
```
Algorithm inside `write_bucketed_map`:
1. `let bits = bucket_bits_for_count(map.len(), target_per_bucket);`
2. `let num_buckets = 1usize << bits;`
3. Partition references in one $O(N)$ pass:
   ```rust
   let mut buckets: Vec<Vec<(u64, &V)>> = (0..num_buckets)
       .map(|_| Vec::with_capacity(target_per_bucket + 8))
       .collect();
   for (id, value) in map {
       let bid = bucket_id(**id, bits) as usize;
       buckets[bid].push((**id, value));
   }
   ```
4. Encode buckets in parallel using `std::thread::scope` (chunk `buckets` across `available_parallelism()` threads; each worker reuses one `scratch_payload: Vec<u8>`, one `scratch_offsets: Vec<u32>`, and produces `Vec<(u32, Vec<u8>)>` for its non-empty buckets):
   - Note: skipping empty buckets avoids inserting empty rows into redb.
5. Concatenate chunks (already in strictly ascending `u32` bucket order!) and insert into `write_txn.open_table(table_def)?`.
6. Return `Ok(bits)` so `build` writes `bits` into `STORE_METADATA`.

### Updating All Readers & Test Mutators in `store.rs`
- `get_node`: compute `bkey = bucket_id(key, bits)`, read `table.get(bkey)?`, call `lookup_in_bucket`, decode via `postcard::from_bytes`.
- `for_each_declaration`: iterate `table.range(..=u32::MAX)?`, call `iter_bucket_entries`, decode each `Declaration`.
- `for_each_constant_reference_with_name`:
  - Open BOTH `CONSTANT_REFERENCES` and `NAMES` in one `ReadTransaction`.
  - **CRITICAL:** In Phase 3B (when LZ4 decompression uses a scratch buffer), NEVER share the same decompression buffer between the outer `CONSTANT_REFERENCES` bucket and the inner `NAMES` lookup! Use two distinct `Vec<u8>` buffers (`refs_buf` and `names_buf`). Designing it with separate buffers right now in Phase 3A makes 3B trivial.
- `node_ids`: change return type to `Result<Vec<T>, StoreError>`. Iterate `table.range(..=u32::MAX)?`, call `iter_bucket_keys`.
- `document_diagnostics`: iterate `DOCUMENTS` buckets, call `iter_bucket_entries`, decode each `Document`.
- `#[cfg(test)] put_node` and `#[cfg(test)] delete_node`:
  - Compute `bkey = bucket_id(key, bits)`.
  - Read existing raw bucket bytes (if any), parse into `Vec<(u64, Vec<u8>)>` (skipping `key` for delete, or replacing/inserting `(key, new_postcard_bytes)` for put), re-encode the bucket (or remove `bkey` if now empty), and commit.
  - This keeps `string_round_trips_through_redb`, `declaration_round_trips_through_redb`, and `incremental_mutation_persists` passing unchanged!

### Existing Tests Affected in Phase 3A
- **`for_each_store_declaration_records_a_corrupt_node_without_panicking` (`store.rs:1011-1041`)**:
  - Change `table.insert(id.get(), &[0xff_u8, 0xff, 0xff][..])` to `table.insert(bucket_id(id.get(), store_bits), &[0xff_u8, 0xff, 0xff][..])` (reading `META_BITS_DECLARATIONS` from `STORE_METADATA` or using `0_u32` after reading the first key of `DECLARATIONS`).
  - Also add a second corruption test where the bucket header is valid (`count = 1, key = id, offsets = [0, 3]`) but the inner postcard payload is `&[0xff, 0xff, 0xff]`, proving both bucket-level and postcard-level corruption increment `store_error_count()` cleanly!
- **Add a unit test for `bucket_id(key, 0) == 0`** and boundary keys `u64::MIN`, `u64::MAX`, `1`, `u64::MAX - 1` across `bits = 0..=16`.

### Phase 3A Verification & Gate
- Run all Rust store tests + differential + roundtrip + dual-feature clippy.
- Run focused Ruby tests (`test/disk_index_*_test.rb`).
- Run Phase 0 benchmark commands:
  - Expect **massive build-time drop** (2–4× faster `Store build`) and **significant size drop** (elimination of most of the 40.6 MB leaf slack).
  - Verify hover latency (`resolve_constant`) stays ~0.09–0.12 ms/op (no regression). If hover slows noticeably, try `TARGET_ENTRIES_PER_BUCKET = 32` vs `64` and keep the faster one.
- Commit Phase 3A (`Bucket redb node tables by hash prefix and parallelize store build`).

---

## Phase 3B: LZ4 Block Compression on Buckets & Search Shards

**Target:** Cut remaining store size in half (~70 MB → **~35–45 MB** on gems; ~500 MB → **~140–175 MB** on large Rails repos) while keeping build time **faster** (less disk I/O and smaller fsync!) and read latency **unchanged** (~1 µs LZ4 decompress per 4 KB bucket).

### Dependency Addition (`rust/rubydex/Cargo.toml`)
Add `lz4_flex` strictly behind the optional `redb-store` feature so default (in-memory) builds stay zero-extra-dependency:
```toml
[features]
redb-store = ["dep:redb", "dep:postcard", "dep:lz4_flex"]

[dependencies]
lz4_flex = { version = "0.11", default-features = false, features = ["safe-encode", "safe-decode"], optional = true }
```
*Why `safe-encode` + `safe-decode`?* Pure safe Rust, zero C dependencies, zero `unsafe`, compiles cleanly on Linux, macOS, and Windows.

### Exact Compressed Block Wire Format
Wrap every bucket (and every `SEARCH_NAMES` / `DOCUMENT_URIS` front-coded shard) with a 4-byte uncompressed length header:
```
[uncompressed_len: u32 LE]
[lz4_block_bytes...]
```
With an explicit safety cap against corrupt/malicious stores:
```rust
/// Upper bound on any single decompressed bucket or shard (64 MiB).
/// Prevents a corrupted 4-byte length prefix from triggering an OOM abort across `extern "C"`.
const MAX_BLOCK_UNCOMPRESSED_BYTES: usize = 64 * 1024 * 1024;

fn compress_block(raw: &[u8], out: &mut Vec<u8>) -> Result<(), StoreError> {
    let raw_len = u32::try_from(raw.len()).map_err(|_| StoreError::CorruptBlock {
        table: "lz4",
        key: 0,
        reason: "raw block exceeds u32::MAX",
    })?;
    out.clear();
    out.extend_from_slice(&raw_len.to_le_bytes());
    let max_compressed = lz4_flex::block::get_maximum_output_size(raw.len());
    out.resize(4 + max_compressed, 0);
    let written = lz4_flex::block::compress_into(raw, &mut out[4..]).map_err(|_| StoreError::CorruptBlock {
        table: "lz4",
        key: 0,
        reason: "lz4 compression failed",
    })?;
    out.truncate(4 + written);
    Ok(())
}

fn decompress_block(
    table: &'static str,
    key: u32,
    compressed: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), StoreError> {
    let err = |reason| StoreError::CorruptBlock { table, key, reason };
    let len_bytes: [u8; 4] = compressed.get(0..4).ok_or_else(|| err("truncated lz4 header"))?.try_into().unwrap();
    let uncompressed_len = u32::from_le_bytes(len_bytes) as usize;
    if uncompressed_len > MAX_BLOCK_UNCOMPRESSED_BYTES {
        return Err(err("uncompressed block length exceeds safety cap"));
    }
    out.resize(uncompressed_len, 0);
    let written = lz4_flex::block::decompress_into(&compressed[4..], out)
        .map_err(|_| err("lz4 decompression failed"))?;
    if written != uncompressed_len {
        return Err(err("lz4 decompressed length mismatch"));
    }
    Ok(())
}
```

### Zero-Allocation Thread-Local Scratch Buffer for Point Lookups
To avoid allocating a fresh 4–8 KB `Vec<u8>` on every `get_node` call:
```rust
thread_local! {
    static DECOMPRESS_SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}
```
- In `get_node`: borrow `DECOMPRESS_SCRATCH` mutably, call `decompress_block(..., &mut scratch)?`, call `lookup_in_bucket(..., &scratch, key)?`, and `postcard::from_bytes` before releasing the borrow (keep the borrow strictly inside `get_node` so re-entrancy is impossible!).
- In streaming scans (`for_each_declaration`, `for_each_constant_reference_with_name`, `node_ids`, `document_diagnostics`, `declaration_ids_matching_parallel`, `document_uris`, `#[cfg(test)] search_names`, `#[cfg(test)] declaration_ids_matching`): allocate local `Vec<u8>` scratch buffers on the stack of the scan function (two separate buffers in `for_each_constant_reference_with_name`: `refs_buf` and `names_buf`!) — do **not** hold `DECOMPRESS_SCRATCH` across user callbacks (`visit(...)`), because `visit` might call back into `graph.declaration(...)` → `get_node` and trigger a `RefCell` borrow panic!
- **Do not forget `#[cfg(test)] put_node` and `delete_node`:** they read and rewrite buckets too — call `decompress_block` when reading an existing bucket and `compress_block` before inserting the updated bucket, otherwise `string_round_trips_through_redb`, `declaration_round_trips_through_redb`, and `incremental_mutation_persists` will fail with `CorruptBlock`.
- Also compress `SEARCH_NAMES` shards and the `DOCUMENT_URIS` blob with `compress_block` during `build` and decompress with `decompress_block` on read.

### Tests to Add in Phase 3B
1. Corrupt LZ4 header with `uncompressed_len = u32::MAX` (`[0xff, 0xff, 0xff, 0xff, 0x00]`) returns `Err(StoreError::CorruptBlock { .. })` immediately without allocating or panicking.
2. Corrupt LZ4 body bytes return `Err(StoreError::CorruptBlock { .. })` and increment `graph.store_error_count()`.

### Phase 3B Verification & Benchmark Gate
- Run all Rust tests (`model::store::tests`, `store_roundtrip`, `differential_store`) + dual-feature clippy.
- Run focused Ruby tests (`test/disk_index_*_test.rb`).
- Run Phase 0 benchmark commands and compare against Phase 0 AND Phase 3A:
  - Expect **total size 65–73% smaller than Phase 0** (~35–45 MB on gems).
  - Expect **`Store build` 3–5× faster than Phase 0** (~0.20–0.30s on gems).
  - Verify hover latency (`resolve_constant`) and search latency are **equal or faster** than Phase 0.
- Commit Phase 3B (`Compress redb buckets and search shards with LZ4`).

---

## Phase 4: Safe Eviction of Stale Workspace Caches

**Target:** Reclaim ~900 MB+ of orphaned stores under `platform_cache_root/rubydex/` for deleted workspaces/worktrees, with zero risk of deleting active stores or unrelated files.

### Step 4.1 — Record `"workspace"` in the JSON manifest (`lib/rubydex/graph.rb:763`)
Currently `lib/rubydex/graph.rb:763` writes:
```ruby
File.write(manifest_tmp, JSON.generate({ "lockfile" => lockfile, "files" => scan }))
```
Change to:
```ruby
File.write(manifest_tmp, JSON.generate({ "workspace" => workspace_path, "lockfile" => lockfile, "files" => scan }))
```
Verify `adopt_manifest` in `lib/rubydex/graph.rb` ignores unknown top-level keys (it reads `data["lockfile"]` and `data["files"]`). Keep backward compatibility with manifests that lack `"workspace"` (never evict a directory whose manifest has no `"workspace"` key!).

### Step 4.2 — Prune orphaned store directories in `rubydex-index-manager`
- **New file:** `rust/rubydex-index-manager/src/prune.rs` (export from `lib.rs`).
- **Cache root path derivation (CRITICAL — DO NOT GET THIS WRONG):**
  - `lib/rubydex/graph.rb:275` sets `registry = File.join(platform_cache_root, "rubydex", "manager", "sessions")`.
  - So in `rust/rubydex-index-manager/src/main.rs`, `args.registry` is `<cache_root>/rubydex/manager/sessions`.
  - `args.registry.parent()` is `<cache_root>/rubydex/manager`.
  - `let cache_root = args.registry.parent().and_then(Path::parent);` is `<cache_root>/rubydex`.
- **Safety rules for `pub fn prune_orphaned_stores(cache_root: &Path, active_stores: &[PathBuf]) -> usize`:**
  Iterate entries directly under `cache_root`. For each entry `dir`:
  1. Must be a real directory (not a symlink — check `entry.file_type()?.is_dir()`).
  2. Skip `"manager"` and any directory name starting with `"."`.
  3. Must contain `index.redb.files` (proves it is a Rubydex store directory; never delete arbitrary user directories!).
  4. Skip if `dir.join("index.redb")` is in `active_stores`.
  5. Skip if `dir.join("index.redb.rebuild")` exists (the create-exclusive rebuild lock directory created by `Graph#claim_rebuild` at `lib/rubydex/graph.rb:381`).
  6. Parse `index.redb.files` as JSON (`serde_json::Value`). Read `json.get("workspace").and_then(Value::as_str)`. If missing or not an absolute path, **skip** (do not delete unknown/older manifests).
  7. Check `Path::new(workspace).exists()` — **only remove `dir` if `!Path::new(workspace).exists()`** (use `std::fs::metadata(workspace)` and only treat `ErrorKind::NotFound` as deleted! Do NOT delete on `PermissionDenied` or transient mount errors!).
  8. Remove with `std::fs::remove_dir_all(&dir)` (ignore errors).
- **Wiring in `rust/rubydex-index-manager/src/main.rs`:**
  - Call `prune_orphaned_stores` once at startup (after acquiring `manager.lock` and reading initial `live_sessions`) and periodically every `Duration::from_secs(600)` inside the main loop.

### Tests to Add in Phase 4
- Unit tests in `rust/rubydex-index-manager/src/prune.rs`:
  1. Removes a store directory whose manifest `"workspace"` points to a non-existent path.
  2. Keeps a store directory whose `"workspace"` still exists.
  3. Keeps a store directory listed in `active_stores` even if workspace vanished.
  4. Keeps `"manager"` and any directory lacking `index.redb.files` or lacking `"workspace"` in its manifest.
  5. Keeps a store directory when `index.redb.rebuild` is present.
- Focused Ruby test verifying `index.redb.files` includes `"workspace"`.

### Phase 4 Verification
- `cd rust && cargo test -p rubydex-index-manager && cargo clippy --workspace --all-targets -- -D warnings`
- `bundle exec rubocop lib/rubydex/graph.rb`
- Commit Phase 4 (`Evict platform cache stores for deleted workspaces`).

---

## Final Validation (After All Phases Complete)

Run the full validation suite once:
```bash
bundle exec rake compile_release
bundle exec rake lint
GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec rake test
```
And run the Phase 0 benchmark script one final time to record the end-to-end before/after table (store size, `Store build` time, hover latency, search latency, `require_paths` latency, and peak RSS) in `docs/disk-persisted-index/database_diet.md`.

---

## Deferred (Follow-On Epic, Out of Scope Here)

- **Phase 5 — Shared Read-Only Library Store (Gems + Stdlib):** Layering a shared resolved base store keyed by `(Ruby version, Gemfile.lock hash)` underneath per-worktree workspace stores eliminates N× gem duplication across worktrees/projects. Treat as a separate design & PR once Phases 0–4 are merged and measured.
