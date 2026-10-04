# Task 1.1 Detailed Plan: Differential Test Harness

**STATUS: COMPLETE (Tasks A–E).** Deviation from plan: the harness immediately caught a real divergence — the Cypher `GraphProvider` was in-memory-only (store-backed graphs returned 0 rows for every label scan). Fixed in the Task E commit (union enumerators + layered lookups in `query/cypher/schema.rs`, new `RedbStore::definition_ids()`/`Graph::store_definition_ids()`). Stdlib tier runs sampled (`sample=2000`) — unsampled is >15 min.

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A regression net that fails the build whenever any query API returns different results on the in-memory graph vs the store-backed graph built from the same corpus.

**Architecture:** One integration test (`rust/rubydex/tests/differential_store.rs`, gated on `redb-store`) that builds the same fixture corpus two ways — fully in-memory, and indexed → `RedbStore::build` → `Graph::with_store` — then runs an identical "probe battery" over both and asserts the normalized outputs are byte-identical. A sensitivity test proves the net actually catches divergences. A `#[ignore]`d variant runs the same battery on the Ruby stdlib corpus (env-gated, heavy).

**Tech Stack:** Rust, existing `rubydex` public API, `tempfile` (already a dev-dependency), redb via the existing `redb-store` feature.

**Spec:** `docs/disk-persisted-index/ship-plan.md` (Phase 1, Task 1.1).

## Global Constraints

- TDD: each task's test is written and watched red (for the stated reason) before the code that makes it pass.
- No production behavior changes in this plan except two visibility bumps (`store_declaration_names`, `store_document_uris` → `pub`). If a probe exposes a real divergence, STOP: that is a new TDD bugfix task (test from the harness, fix, then continue).
- Probes must be order-independent (sort everything) and must never compare `Debug` of a `NodeRef` (the `Mem` vs `Stored` wrapper differs between graphs by design — always deref to `&T` first or use raw id/`&str` values).
- Commits per task with `--no-gpg-sign`.
- Verify commands (run after every task):
  ```bash
  cd rust && cargo test -p rubydex --features redb-store --test differential_store
  cargo test -p rubydex --features redb-store 2>&1 | grep -E "^test result"   # full suite still green
  cargo clippy -p rubydex --features redb-store --all-targets 2>&1 | grep -cE "^warning: |^error"  # must print 0
  ```

## File Structure

- Create: `rust/rubydex/tests/fixtures/diff_corpus/base.rb` — module, constant, method.
- Create: `rust/rubydex/tests/fixtures/diff_corpus/parent.rb` — require, `include`, `attr_accessor`, method.
- Create: `rust/rubydex/tests/fixtures/diff_corpus/child.rb` — subclass, `alias_method`, method, `def self.` singleton.
- Create: `rust/rubydex/tests/fixtures/diff_corpus/kid.rb` — top-level constant alias (`KID = Child`).
- Create: `rust/rubydex/tests/fixtures/diff_corpus/util.rb` — module with constant + `def self.`.
- Create: `rust/rubydex/tests/fixtures/diff_corpus/refs.rb` — top-level code: object creation, cross-file method refs, nested constant ref (`Base::BASE_CONST`), top-level constant refs (`KID`), module-function call.
- Create: `rust/rubydex/tests/differential_store.rs` — the harness.
- Modify: `rust/rubydex/src/model/graph.rs` — `pub(crate)` → `pub` on `store_declaration_names` (both cfg variants) and `store_document_uris` (both cfg variants). Needed because integration tests are outside the crate.

## Verified API surface (use exactly these)

- Pipeline: `Graph::new()`, `graph.set_workspace_path(PathBuf)`, `graph.excluded_patterns()`, `listing::collect_file_paths(Vec<String>, &patterns) -> (Vec<PathBuf>, Vec<_>)`, `indexing::index_files(&mut graph, Vec<PathBuf>, IndexerBackend::RubyIndexer) -> Vec<_>`, `Resolver::new(&mut graph).resolve()`, `RedbStore::build(&Path, &Graph) -> Result<RedbStore, redb::Error>`, `Graph::with_store(RedbStore) -> Graph`.
- Enumeration: `graph.declarations() / definitions() / documents() / names() / strings()` (in-memory maps), `graph.store_declaration_names() -> Vec<(DeclarationId, String)>`, `graph.store_document_uris() -> Vec<(UriId, String)>` (store-backed; empty on memory graphs).
- Layered getters: `graph.declaration(DeclarationId)`, `graph.definition(DefinitionId)`, `graph.document(UriId)`, `graph.name(NameId)`, `graph.string(StringId)`, `graph.constant_reference(ConstantReferenceId)`, `graph.method_reference(MethodReferenceId)`. All return `Option<NodeRef<T>>`; `NodeRef` derefs to `&T`.
- Nodes: `Declaration::name() -> &str`, `kind() -> &'static str`, `owner_id() -> &DeclarationId`, `definitions() -> &[DefinitionId]`, `as_namespace() -> Option<&Namespace>`, `singleton_class_id() -> Option<&DeclarationId>`; `Namespace::members() -> &IdentityHashMap<StringId, DeclarationId>`, `ancestors() -> &Ancestors`, `descendants() -> &IdentityHashSet<DeclarationId>`; `Ancestors::iter() -> Iter<Ancestor>`, `Ancestor::Complete(DeclarationId) | Partial(NameId)` (in `rubydex::model::declaration`); `Definition::kind() -> &'static str`, `uri_id() -> &UriId`, `offset() -> &Offset` (`.start()`); `Document::uri() -> &str`, `definitions() -> &[DefinitionId]`, `method_references() -> &[MethodReferenceId]`, `constant_references() -> &[ConstantReferenceId]`; `ConstantReference::name_id() -> &NameId`, `offset() -> &Offset`; `MethodRef::str() -> &StringId`, `offset() -> &Offset`, `receiver() -> Option<NameId>`; `StringRef::as_str() -> &str`; `Keyword::name() -> &'static str`.
- Queries: `query::declaration_search(&Graph, &[&str], &MatchMode) -> Vec<DeclarationId>` (`MatchMode::Exact | Fuzzy`); `query::require_paths(&Graph, &[PathBuf]) -> Vec<String>`; `query::resolve_require_path(&Graph, &str, &[PathBuf]) -> Option<UriId>`; `query::completion_candidates(&Graph, CompletionContext) -> Result<Vec<CompletionCandidate>, Box<dyn Error>>`; `CompletionContext::new(CompletionReceiver)`; `CompletionReceiver::Expression { self_decl_id: Option<DeclarationId>, nesting_name_id: NameId } | NamespaceAccess { self_decl_id, namespace_decl_id } | MethodCall { self_decl_id, receiver_decl_id }`; `CompletionCandidate::Declaration(DeclarationId) | KeywordArgument(StringId) | Keyword(&'static Keyword)`; `query::follow_method_alias(&Graph, DefinitionId) -> Result<DeclarationId, AliasResolutionError>`; `query::find_member_in_ancestors(&Graph, DeclarationId, StringId, bool) -> Result<DeclarationId, FindMemberError>` (both error enums derive `Debug`); `query::cypher::run_query(&Graph, &str, OutputFormat) -> Result<String, CypherError>`, `OutputFormat` re-exported at `query::cypher::OutputFormat` (has `Table` variant).
- Ids (in `rubydex::model::ids`): `DeclarationId`, `NameId`, `StringId`, `UriId`, `DefinitionId`, `ConstantReferenceId`, `MethodReferenceId` — all have `.get() -> u64`; `DeclarationId::from(&str)`, `DeclarationId::new(u64)`, `StringId::from(&str)` exist.
- `graph.definition_id_to_declaration_id(DefinitionId) -> Option<DeclarationId>`.

---

### Task A: Corpus fixtures + harness skeleton + sensitivity test (RED first)

**Files:**
- Create: `rust/rubydex/tests/fixtures/diff_corpus/*.rb` (6 files, contents below)
- Create: `rust/rubydex/tests/differential_store.rs`
- Modify: `rust/rubydex/src/model/graph.rs` (4 × `pub(crate)` → `pub`)

**Interfaces:**
- Produces: `build_graph_from(root: &Path) -> Graph`, `build_store_graph_from(root: &Path, store_dir: &Path) -> Graph`, `corpus_dir() -> PathBuf`, `probe_all(graph: &Graph, name_ids: &[NameId], out: &mut Vec<(String, String)>)` (empty body in this task), `normalize(&mut Vec<(String, String)>)`, tests `differential_memory_vs_store`, `harness_detects_divergence`.

- [ ] **Step 1: Write the fixture corpus**

`rust/rubydex/tests/fixtures/diff_corpus/base.rb`:
```ruby
module Base
  BASE_CONST = "base"

  def base_method
    "base"
  end
end
```

`rust/rubydex/tests/fixtures/diff_corpus/parent.rb`:
```ruby
require "base"

class Parent
  include Base

  attr_accessor :state

  def parent_method
    state
  end
end
```

`rust/rubydex/tests/fixtures/diff_corpus/child.rb`:
```ruby
require "parent"

class Child < Parent
  alias_method :nickname, :parent_method

  def child_method
    self.parent_method
  end

  def self.solo
    "solo"
  end
end
```

`rust/rubydex/tests/fixtures/diff_corpus/kid.rb`:
```ruby
KID = Child
```

`rust/rubydex/tests/fixtures/diff_corpus/util.rb`:
```ruby
module Util
  UTIL_CONST = 7

  def self.util_method
    UTIL_CONST
  end
end
```

`rust/rubydex/tests/fixtures/diff_corpus/refs.rb`:
```ruby
child = Child.new
child.parent_method
child.child_method
Child.solo
KID
Base::BASE_CONST
Util.util_method
```

Coverage: nesting, `include` (ancestors), subclass (ancestors + descendants), `attr_accessor` (ivar + method declarations), `alias_method` (follow_method_alias), singleton `def self.`, top-level constant alias, cross-file constant refs (incl. nested `Base::BASE_CONST`), cross-file method refs, `require` (require_paths / resolve_require_path).

- [ ] **Step 2: Bump store enumeration accessors to `pub`**

In `rust/rubydex/src/model/graph.rs`, change the four signatures (two cfg variants each, around lines 322/336/343/357):
```rust
    pub fn store_declaration_names(&self) -> Vec<(DeclarationId, String)> {
    pub fn store_document_uris(&self) -> Vec<(UriId, String)> {
```
(Keep the existing doc comments and any `#[allow(clippy::unused_self)]` on the no-op variants.)

- [ ] **Step 3: Write the harness skeleton with the sensitivity test**

Create `rust/rubydex/tests/differential_store.rs`:
```rust
//! Differential tests: every query API must return identical results on the in-memory
//! graph and on the store-backed graph built from the same corpus. The in-memory graph
//! is the reference; the store path is only correct if it reproduces it exactly.

#![cfg(feature = "redb-store")]

use rubydex::{
    indexing::{index_files, IndexerBackend},
    listing::collect_file_paths,
    model::{
        graph::Graph,
        ids::NameId,
        store::RedbStore,
    },
    resolution::Resolver,
};
use std::path::{Path, PathBuf};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/diff_corpus")
}

/// Indexes + resolves every Ruby file under `root` into a fresh in-memory graph.
fn build_graph_from(root: &Path) -> Graph {
    let mut graph = Graph::new();
    graph.set_workspace_path(root.to_path_buf());
    let (files, _errors) = collect_file_paths(
        vec![root.to_string_lossy().into_owned()],
        &graph.excluded_patterns(),
    );
    let _ = index_files(&mut graph, files, IndexerBackend::RubyIndexer);
    Resolver::new(&mut graph).resolve();
    graph
}

/// Builds the store from a corpus rooted at `root` and reopens it store-backed.
fn build_store_graph_from(root: &Path, store_dir: &Path) -> Graph {
    let memory = build_graph_from(root);
    let path = store_dir.join("index.redb");
    let store = RedbStore::build(&path, &memory).expect("build store");
    drop(memory);
    Graph::with_store(store)
}

/// Runs the full probe battery. Probes are added by later tasks; entries are
/// `(case, normalized_value)` pairs.
fn probe_all(graph: &Graph, name_ids: &[NameId], out: &mut Vec<(String, String)>) {
    let _ = (graph, name_ids);
}

fn normalize(entries: &mut Vec<(String, String)>) {
    entries.sort_by(|a, b| a.0.cmp(&b.0));
}

/// The net must catch a known divergence: the store graph is built from a corpus with an
/// extra file, so it must probe differently than the in-memory graph.
#[test]
fn harness_detects_divergence() {
    let dir = tempfile::tempdir().expect("tempdir");
    let extended = dir.path().join("extended_corpus");
    std::fs::create_dir_all(&extended).expect("mkdir");
    for entry in std::fs::read_dir(corpus_dir()).expect("read corpus") {
        let entry = entry.expect("dir entry");
        std::fs::copy(entry.path(), extended.join(entry.file_name())).expect("copy fixture");
    }
    std::fs::write(extended.join("extra.rb"), "class Extra\n  def extra_method; end\nend\n")
        .expect("write extra");

    let memory = build_graph_from(&corpus_dir());
    let store_graph = build_store_graph_from(&extended, dir.path());
    let name_ids: Vec<NameId> = memory.names().keys().copied().collect();

    let mut mem_probe = Vec::new();
    probe_all(&memory, &name_ids, &mut mem_probe);
    let mut store_probe = Vec::new();
    probe_all(&store_graph, &name_ids, &mut store_probe);
    normalize(&mut mem_probe);
    normalize(&mut store_probe);

    assert_ne!(
        mem_probe, store_probe,
        "harness failed to detect a known divergence (extra.rb present only in the store graph)"
    );
}

#[test]
fn differential_memory_vs_store() {
    let dir = tempfile::tempdir().expect("tempdir");
    let memory = build_graph_from(&corpus_dir());
    let store_graph = build_store_graph_from(&corpus_dir(), dir.path());
    let name_ids: Vec<NameId> = memory.names().keys().copied().collect();

    let mut mem_probe = Vec::new();
    probe_all(&memory, &name_ids, &mut mem_probe);
    let mut store_probe = Vec::new();
    probe_all(&store_graph, &name_ids, &mut store_probe);
    normalize(&mut mem_probe);
    normalize(&mut store_probe);

    assert_eq!(mem_probe.len(), store_probe.len(), "probe case count differs");
    for (m, s) in mem_probe.iter().zip(store_probe.iter()) {
        assert_eq!(m.0, s.0, "probe case sets differ");
        assert_eq!(m.1, s.1, "divergence in case {}", m.0);
    }
}
```

- [ ] **Step 4: Watch the sensitivity test fail (RED)**

Run: `cd rust && cargo test -p rubydex --features redb-store --test differential_store harness_detects_divergence`
Expected: FAIL — `harness failed to detect a known divergence` (both probes are empty because `probe_all` is a stub). `differential_memory_vs_store` passes trivially (empty probes) — that is expected and fine in this task.

- [ ] **Step 5: Commit**

```bash
git add rust/rubydex/tests/fixtures/diff_corpus rust/rubydex/tests/differential_store.rs rust/rubydex/src/model/graph.rs
git commit --no-gpg-sign -m "Add differential store harness skeleton with sensitivity test"
```

---

### Task B: Declaration sweep + search probes

**Files:**
- Modify: `rust/rubydex/tests/differential_store.rs`

**Interfaces:**
- Consumes: `probe_all`, `build_graph_from` (Task A).
- Produces: `all_declaration_ids(graph: &Graph) -> BTreeSet<u64>`, `probe_declarations`, `probe_search`; the sensitivity test goes GREEN with these two probes.

- [ ] **Step 1: Add the declaration sweep probe**

Add to `differential_store.rs` (imports: `std::collections::BTreeSet`, `rubydex::model::declaration::Ancestor`, `rubydex::model::ids::DeclarationId`):

```rust
/// All declaration ids a graph can see: in-memory map ∪ store-backed names.
fn all_declaration_ids(graph: &Graph) -> BTreeSet<u64> {
    let mut ids: BTreeSet<u64> = graph.declarations().keys().map(|id| id.get()).collect();
    for (id, _name) in graph.store_declaration_names() {
        ids.insert(id.get());
    }
    ids
}

fn declaration_fqn(graph: &Graph, id: DeclarationId) -> String {
    graph
        .declaration(id)
        .map(|d| d.name().to_string())
        .unwrap_or_else(|| format!("<missing {}>", id.get()))
}

/// Probes every declaration's full structure: name, kind, owner, members, ancestors,
/// descendants, singleton class, and each definition's kind/uri/offset.
fn probe_declarations(graph: &Graph, out: &mut Vec<(String, String)>) {
    for raw in all_declaration_ids(graph) {
        let id = DeclarationId::new(raw);
        let Some(decl) = graph.declaration(id) else {
            out.push((format!("decl:id:{raw}"), "<missing>".into()));
            continue;
        };
        let fqn = decl.name().to_string();
        let owner = declaration_fqn(graph, *decl.owner_id());
        let (members, ancestors, descendants): (Vec<String>, Vec<String>, Vec<String>) =
            match decl.as_namespace() {
                Some(ns) => (
                    ns.members()
                        .values()
                        .map(|m| declaration_fqn(graph, *m))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                    ns.ancestors()
                        .iter()
                        .map(|a| match a {
                            Ancestor::Complete(ancestor_id) => declaration_fqn(graph, *ancestor_id),
                            Ancestor::Partial(name_id) => format!("partial:{}", name_id.get()),
                        })
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                    ns.descendants()
                        .iter()
                        .map(|d| declaration_fqn(graph, *d))
                        .collect::<BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                ),
                None => (Vec::new(), Vec::new(), Vec::new()),
            };
        let singleton = decl
            .singleton_class_id()
            .map(|id| declaration_fqn(graph, *id))
            .unwrap_or_default();
        let definitions: Vec<String> = decl
            .definitions()
            .iter()
            .map(|def_id| {
                let def = graph.definition(*def_id).expect("definition present for declaration");
                let uri = graph
                    .document(*def.uri_id())
                    .map(|d| d.uri().to_string())
                    .unwrap_or_default();
                format!("{}@{}@{}", def.kind(), uri, def.offset().start())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        out.push((
            format!("decl:{fqn}"),
            format!(
                "kind:{}|owner:{}|members:{}|ancestors:{}|descendants:{}|singleton:{}|definitions:{}",
                decl.kind(),
                owner,
                members.join(","),
                ancestors.join(","),
                descendants.join(","),
                singleton,
                definitions.join(";"),
            ),
        ));
    }
}
```

- [ ] **Step 2: Add the search probes**

```rust
fn probe_search(graph: &Graph, out: &mut Vec<(String, String)>) {
    use rubydex::query::{declaration_search, MatchMode};

    // Every declaration must be findable by exact-FQN search (search is substring-based,
    // so the result set may include other names — the invariant is an identical result
    // set on both graphs).
    let fqns: Vec<String> = all_declaration_ids(graph)
        .iter()
        .filter_map(|raw| graph.declaration(DeclarationId::new(*raw)).map(|d| d.name().to_string()))
        .collect();
    for fqn in &fqns {
        let found = declaration_search(graph, &[fqn.as_str()], &MatchMode::Exact);
        let names: Vec<String> = found
            .iter()
            .map(|id| declaration_fqn(graph, *id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        out.push((format!("search:exact:{fqn}"), names.join(",")));
    }

    for (mode, q) in [(MatchMode::Exact, "Ch"), (MatchMode::Fuzzy, "chd"), (MatchMode::Fuzzy, "base")] {
        let found = declaration_search(graph, &[q], &mode);
        let names: Vec<String> = found
            .iter()
            .map(|id| declaration_fqn(graph, *id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        out.push((format!("search:{mode:?}:{q}"), names.join(",")));
    }
}
```

- [ ] **Step 3: Wire into `probe_all`**

```rust
fn probe_all(graph: &Graph, name_ids: &[NameId], out: &mut Vec<(String, String)>) {
    probe_declarations(graph, out);
    probe_search(graph, out);
    let _ = name_ids;
}
```

- [ ] **Step 4: Run both tests (sensitivity test must now be GREEN)**

Run: `cd rust && cargo test -p rubydex --features redb-store --test differential_store`
Expected: 2 passed. If `differential_memory_vs_store` FAILS, a real divergence was found — stop, file it as a bugfix task, fix via TDD, then continue. If `harness_detects_divergence` still fails, the sweep is missing the divergence surface — extend the probe before moving on.

- [ ] **Step 5: Run global verification, then commit**

```bash
cd rust && cargo test -p rubydex --features redb-store 2>&1 | grep -E "^test result"
cargo clippy -p rubydex --features redb-store --all-targets 2>&1 | grep -cE "^warning: |^error"
git add rust/rubydex/tests/differential_store.rs
git commit --no-gpg-sign -m "Differential harness: declaration sweep and search probes"
```

---

### Task C: Completion probes (expression, namespace access, method call)

**Files:**
- Modify: `rust/rubydex/tests/differential_store.rs`

**Interfaces:**
- Consumes: `all_declaration_ids`, `declaration_fqn` (Task B).
- Produces: `probe_completion`; candidate rendering shared with later probes.

- [ ] **Step 1: Add the completion probes**

```rust
fn candidate_names(graph: &Graph, candidates: &[rubydex::query::CompletionCandidate]) -> String {
    use rubydex::query::CompletionCandidate;
    candidates
        .iter()
        .map(|c| match c {
            CompletionCandidate::Declaration(id) => declaration_fqn(graph, *id),
            CompletionCandidate::KeywordArgument(str_id) => graph
                .string(*str_id)
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|| format!("<missing string {}>", str_id.get())),
            CompletionCandidate::Keyword(keyword) => format!("kw:{}", keyword.name()),
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",")
}

fn probe_completion(graph: &Graph, name_ids: &[NameId], out: &mut Vec<(String, String)>) {
    use rubydex::query::{completion_candidates, CompletionContext, CompletionReceiver};

    // Expression completion at every interned name (lexical scope = that name, self derived).
    // Name ids are taken from the in-memory graph: ids are deterministic content hashes,
    // so the same ids are valid keys in the store.
    for name_id in name_ids.iter() {
        let ctx = CompletionContext::new(CompletionReceiver::Expression {
            self_decl_id: None,
            nesting_name_id: *name_id,
        });
        let entry = match completion_candidates(graph, ctx) {
            Ok(candidates) => candidate_names(graph, &candidates),
            Err(error) => format!("Err({error})"),
        };
        out.push((format!("complete:expression:{}", name_id.get()), entry));
    }

    // Namespace-access and method-call completion at every namespace declaration.
    let namespaces: Vec<String> = all_declaration_ids(graph)
        .iter()
        .filter(|raw| {
            graph
                .declaration(DeclarationId::new(*raw))
                .and_then(|d| d.as_namespace())
                .is_some()
        })
        .map(|raw| declaration_fqn(graph, DeclarationId::new(*raw)))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    for fqn in &namespaces {
        let id = DeclarationId::from(fqn.as_str());
        let ns_ctx = CompletionContext::new(CompletionReceiver::NamespaceAccess {
            self_decl_id: None,
            namespace_decl_id: id,
        });
        let ns_entry = match completion_candidates(graph, ns_ctx) {
            Ok(candidates) => candidate_names(graph, &candidates),
            Err(error) => format!("Err({error})"),
        };
        out.push((format!("complete:namespace:{fqn}"), ns_entry));

        let call_ctx = CompletionContext::new(CompletionReceiver::MethodCall {
            self_decl_id: None,
            receiver_decl_id: id,
        });
        let call_entry = match completion_candidates(graph, call_ctx) {
            Ok(candidates) => candidate_names(graph, &candidates),
            Err(error) => format!("Err({error})"),
        };
        out.push((format!("complete:methodcall:{fqn}"), call_entry));
    }
}
```

Note: `DeclarationId::from(fqn)` round-trips because declaration FQNs are exactly the interned name strings used to mint ids.

- [ ] **Step 2: Wire into `probe_all`**

```rust
fn probe_all(graph: &Graph, name_ids: &[NameId], out: &mut Vec<(String, String)>) {
    probe_declarations(graph, out);
    probe_search(graph, out);
    probe_completion(graph, name_ids, out);
}
```

- [ ] **Step 3: Run tests**

Run: `cd rust && cargo test -p rubydex --features redb-store --test differential_store`
Expected: 2 passed. A `complete:*` divergence = real bug (the store completion paths were the review's item 1) — stop and fix via TDD before continuing.

- [ ] **Step 4: Run global verification, then commit**

```bash
cd rust && cargo test -p rubydex --features redb-store 2>&1 | grep -E "^test result"
cargo clippy -p rubydex --features redb-store --all-targets 2>&1 | grep -cE "^warning: |^error"
git add rust/rubydex/tests/differential_store.rs
git commit --no-gpg-sign -m "Differential harness: completion probes"
```

---

### Task D: Alias, member, require, document/reference probes

**Files:**
- Modify: `rust/rubydex/tests/differential_store.rs`

**Interfaces:**
- Consumes: `all_declaration_ids`, `declaration_fqn` (Task B).
- Produces: `probe_aliases`, `probe_find_member`, `probe_require`, `probe_documents`.

- [ ] **Step 1: Add the alias + find_member probes**

```rust
fn probe_aliases(graph: &Graph, out: &mut Vec<(String, String)>) {
    use rubydex::query::follow_method_alias;

    // Every definition of every declaration: alias definitions resolve to a target,
    // non-alias definitions error deterministically. Both must match across graphs.
    for raw in all_declaration_ids(graph) {
        let id = DeclarationId::new(raw);
        let Some(decl) = graph.declaration(id) else { continue };
        for def_id in decl.definitions() {
            let rendered = match follow_method_alias(graph, *def_id) {
                Ok(target) => format!("Ok({})", declaration_fqn(graph, target)),
                Err(error) => format!("Err({error:?})"),
            };
            out.push((format!("alias:{}:{}", decl.name(), def_id.get()), rendered));
        }
    }
}

fn probe_find_member(graph: &Graph, out: &mut Vec<(String, String)>) {
    use rubydex::query::find_member_in_ancestors;
    use rubydex::model::ids::StringId;

    let cases: &[(&str, &str)] = &[
        ("Child", "parent_method"), // inherited
        ("Child", "nickname"),      // alias
        ("Child", "state"),         // attr_accessor
        ("Child", "solo"),          // inherited singleton? no — solo is on Child's singleton; expect Err on instance lookup
        ("Child", "zzz_missing"),   // expect Err(MemberNotFound)
        ("Parent", "base_method"),  // included module
        ("Util", "util_method"),    // def self.
    ];
    for (owner, member) in cases {
        let result = find_member_in_ancestors(
            graph,
            DeclarationId::from(*owner),
            StringId::from(*member),
            false,
        );
        let rendered = match result {
            Ok(target) => format!("Ok({})", declaration_fqn(graph, target)),
            Err(error) => format!("Err({error:?})"),
        };
        out.push((format!("member:{owner}:{member}"), rendered));
    }
}
```

(If `("Child", "solo")` turns out to be found via the singleton chain on one graph and not the other, that is a real divergence to investigate, not a probe bug.)

- [ ] **Step 2: Add the require probes**

```rust
fn probe_require(graph: &Graph, out: &mut Vec<(String, String)>) {
    use rubydex::query::{require_paths, resolve_require_path};

    let root = corpus_dir();
    let paths = require_paths(graph, &[root.clone()]);
    out.push((
        "require:paths".into(),
        paths.into_iter().collect::<BTreeSet<_>>().into_iter().collect::<Vec<_>>().join(","),
    ));
    for path in ["base", "parent", "child", "util", "refs", "kid", "nope"] {
        let resolved = resolve_require_path(graph, path, &[root.clone()])
            .map(|uri_id| {
                graph
                    .document(uri_id)
                    .map(|d| d.uri().to_string())
                    .unwrap_or_else(|| "<missing document>".into())
            })
            .unwrap_or_else(|| "None".into());
        out.push((format!("require:resolve:{path}"), resolved));
    }
}
```

- [ ] **Step 3: Add the document + references probe**

```rust
fn probe_documents(graph: &Graph, out: &mut Vec<(String, String)>) {
    use rubydex::model::ids::UriId;

    let mut uris: BTreeSet<u64> = graph.documents().keys().map(|id| id.get()).collect();
    for (id, _uri) in graph.store_document_uris() {
        uris.insert(id.get());
    }
    for raw in uris {
        let uri_id = UriId::new(raw);
        let Some(doc) = graph.document(uri_id) else {
            out.push((format!("doc:id:{raw}"), "<missing>".into()));
            continue;
        };
        let uri = doc.uri().to_string();
        let defs: Vec<String> = doc
            .definitions()
            .iter()
            .filter_map(|def_id| {
                let decl_id = graph.definition_id_to_declaration_id(*def_id)?;
                Some(declaration_fqn(graph, decl_id))
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        // Render refs by raw id: ids are deterministic content hashes of the referenced
        // name/offset, so equality of raw ids IS semantic equality.
        let const_refs: Vec<String> = doc
            .constant_references()
            .iter()
            .map(|ref_id| {
                let r = graph.constant_reference(*ref_id).expect("const ref present for document");
                format!("{}@{}", r.name_id().get(), r.offset().start())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let method_refs: Vec<String> = doc
            .method_references()
            .iter()
            .map(|ref_id| {
                let r = graph.method_reference(*ref_id).expect("method ref present for document");
                let receiver = r.receiver().map(|n| n.get()).unwrap_or(u64::MAX);
                format!("{}@{}|recv:{}", r.str().get(), r.offset().start(), receiver)
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        out.push((
            format!("doc:{uri}"),
            format!(
                "defs:{}|const_refs:{}|method_refs:{}",
                defs.join(","),
                const_refs.join(","),
                method_refs.join(","),
            ),
        ));
    }
}
```

- [ ] **Step 4: Wire into `probe_all` and run tests**

```rust
fn probe_all(graph: &Graph, name_ids: &[NameId], out: &mut Vec<(String, String)>) {
    probe_declarations(graph, out);
    probe_search(graph, out);
    probe_completion(graph, name_ids, out);
    probe_aliases(graph, out);
    probe_find_member(graph, out);
    probe_require(graph, out);
    probe_documents(graph, out);
}
```

Run: `cd rust && cargo test -p rubydex --features redb-store --test differential_store`
Expected: 2 passed.

- [ ] **Step 5: Run global verification, then commit**

```bash
cd rust && cargo test -p rubydex --features redb-store 2>&1 | grep -E "^test result"
cargo clippy -p rubydex --features redb-store --all-targets 2>&1 | grep -cE "^warning: |^error"
git add rust/rubydex/tests/differential_store.rs
git commit --no-gpg-sign -m "Differential harness: alias, member, require, document probes"
```

---

### Task E: Cypher probes + stdlib corpus test

**Files:**
- Modify: `rust/rubydex/tests/differential_store.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: `probe_cypher`, test `differential_on_stdlib` (ignored, env-gated).

- [ ] **Step 1: Add the cypher probes**

```rust
fn probe_cypher(graph: &Graph, out: &mut Vec<(String, String)>) {
    use rubydex::query::cypher::{run_query, OutputFormat};

    for q in [
        "MATCH (c:Class) RETURN c.name",
        "MATCH (c:Class) WHERE c.name = 'Child' RETURN c.name",
    ] {
        // Table output, sorted lines: iteration order over graph maps can differ
        // between the memory and store graphs, so only sorted content is comparable.
        let result = run_query(graph, q, OutputFormat::Table)
            .unwrap_or_else(|error| format!("Err({error})"));
        let lines: Vec<&str> = result.lines().collect();
        let body: Vec<&str> = if lines.last().is_some_and(|l| l.contains("row")) {
            lines[..lines.len() - 1].to_vec()
        } else {
            lines
        };
        let mut sorted = body;
        sorted.sort();
        out.push((format!("cypher:{q}"), sorted.join("\n")));
    }
}
```

Wire into `probe_all` (append `probe_cypher(graph, out);`).

- [ ] **Step 2: Add the stdlib corpus test (ignored by default)**

```rust
/// Heavy: differential on the Ruby stdlib corpus (the POC's 20k-file corpus).
/// Run: RUBYDEX_DIFF_CORPUS=$(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/.. \
///   cargo test -p rubydex --features redb-store --test differential_store -- --ignored
#[test]
#[ignore = "set RUBYDEX_DIFF_CORPUS to a Ruby source tree to run"]
fn differential_on_stdlib() {
    let root = std::env::var("RUBYDEX_DIFF_CORPUS")
        .expect("set RUBYDEX_DIFF_CORPUS to a Ruby source tree (e.g. $(ruby -e 'print RbConfig::CONFIG[\"rubylibdir\"]')/..)");
    let root = PathBuf::from(root);
    let dir = tempfile::tempdir().expect("tempdir");

    let memory = build_graph_from(&root);
    let store_graph = build_store_graph_from(&root, dir.path());
    // Sample the completion probe: 350k+ names on stdlib is too slow per-name.
    let mut name_ids: Vec<NameId> = memory.names().keys().copied().collect::<BTreeSet<_>>().into_iter().collect();
    name_ids.truncate(1000);

    let mut mem_probe = Vec::new();
    probe_all(&memory, &name_ids, &mut mem_probe);
    let mut store_probe = Vec::new();
    probe_all(&store_graph, &name_ids, &mut store_probe);
    normalize(&mut mem_probe);
    normalize(&mut store_probe);

    assert_eq!(mem_probe.len(), store_probe.len(), "probe case count differs");
    for (m, s) in mem_probe.iter().zip(store_probe.iter()) {
        assert_eq!(m.0, s.0, "probe case sets differ");
        assert_eq!(m.1, s.1, "divergence in case {}", m.0);
    }
}
```

- [ ] **Step 3: Run the full test file + the ignored stdlib test once (local verification only)**

Run:
```bash
cd rust && cargo test -p rubydex --features redb-store --test differential_store
RUBYDEX_DIFF_CORPUS="$(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/.." cargo test -p rubydex --features redb-store --test differential_store -- --ignored
```
Expected: 2 passed + 1 ignored in the first; the stdlib test passes (or reveals a real divergence — stop and fix via TDD).

- [ ] **Step 4: Run global verification, then commit**

```bash
cd rust
cargo test -p rubydex --features redb-store 2>&1 | grep -E "^test result"
cargo test -p rubydex 2>&1 | grep -E "^test result"   # default build still compiles the (cfg-excluded) test file
cargo clippy -p rubydex --features redb-store --all-targets 2>&1 | grep -cE "^warning: |^error"
cargo clippy -p rubydex --all-targets 2>&1 | grep -cE "^warning: |^error"
git add rust/rubydex/tests/differential_store.rs
git commit --no-gpg-sign -m "Differential harness: cypher probes and stdlib corpus test"
```

## Out of scope (separate tasks)

- CI wiring for the harness: roadmap Task 1.8.
- Any divergence the harness uncovers: new TDD bugfix tasks, filed as they appear.
- Fuzz-based store round-trip (roadmap Task 1.2) — different tool, same corpus directory can be reused.
