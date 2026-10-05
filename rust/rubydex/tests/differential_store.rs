//! Differential tests: every query API must return identical results on the in-memory
//! graph and on the store-backed graph built from the same corpus. The in-memory graph
//! is the reference; the store path is only correct if it reproduces it exactly.

#![cfg(feature = "redb-store")]

use rubydex::{
    indexing::{IndexerBackend, index_files},
    listing::collect_file_paths,
    model::{
        declaration::Ancestor,
        graph::Graph,
        ids::{DeclarationId, NameId},
        store::RedbStore,
    },
    resolution::Resolver,
};
use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/diff_corpus")
}

/// Indexes + resolves every Ruby file under `root` into a fresh in-memory graph.
fn build_graph_from(root: &Path) -> Graph {
    let mut graph = Graph::new();
    let (files, _errors) = collect_file_paths(vec![root.to_string_lossy().into_owned()], &graph.excluded_patterns());
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
/// `(case, normalized_value)` pairs. `sample` caps full-corpus walks (0 = no cap): the
/// fixture corpus probes everything; large corpora sample a deterministic prefix.
fn probe_all(graph: &Graph, name_ids: &[NameId], sample: usize, out: &mut Vec<(String, String)>) {
    probe_declarations(graph, sample, out);
    probe_completion(graph, name_ids, sample, out);
    probe_search(graph, sample, out);
    probe_aliases(graph, sample, out);
    probe_find_member(graph, out);
    probe_require(graph, out);
    probe_documents(graph, sample, out);
    probe_cypher(graph, out);
}

/// Deterministic prefix of a sorted id set: `sample` entries, or all when `sample == 0`.
fn cap_sample<T: Ord>(set: BTreeSet<T>, sample: usize) -> Vec<T> {
    if sample == 0 {
        set.into_iter().collect()
    } else {
        set.into_iter().take(sample).collect()
    }
}

/// Heavy: differential on the Ruby stdlib corpus (the POC's 20k-file corpus).
/// Run: `RUBYDEX_DIFF_CORPUS=$(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/.. \
///   cargo test -p rubydex --features redb-store --test differential_store -- --ignored`
#[test]
#[ignore = "set RUBYDEX_DIFF_CORPUS to a Ruby source tree to run"]
fn differential_on_stdlib() {
    let root = std::env::var("RUBYDEX_DIFF_CORPUS").expect(
        "set RUBYDEX_DIFF_CORPUS to a Ruby source tree (e.g. $(ruby -e 'print RbConfig::CONFIG[\"rubylibdir\"]')/..)",
    );
    let root = PathBuf::from(root);
    let dir = tempfile::tempdir().expect("tempdir");

    let memory = build_graph_from(&root);
    let store_graph = build_store_graph_from(&root, dir.path());
    // Sample the completion probe: 350k+ names on stdlib is too slow per-name.
    let mut name_ids: Vec<NameId> = memory
        .names()
        .keys()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    name_ids.truncate(1000);

    let mut mem_probe = Vec::new();
    probe_all(&memory, &name_ids, 2000, &mut mem_probe);
    let mut store_probe = Vec::new();
    probe_all(&store_graph, &name_ids, 2000, &mut store_probe);
    normalize(&mut mem_probe);
    normalize(&mut store_probe);

    assert_eq!(mem_probe.len(), store_probe.len(), "probe case count differs");
    for (m, s) in mem_probe.iter().zip(store_probe.iter()) {
        assert_eq!(m.0, s.0, "probe case sets differ");
        assert_eq!(m.1, s.1, "divergence in case {}", m.0);
    }
}

/// All declaration ids a graph can see: in-memory map ∪ store-backed names.
fn all_declaration_ids(graph: &Graph) -> BTreeSet<u64> {
    let mut ids: BTreeSet<u64> = graph.declarations().keys().map(rubydex::model::id::Id::get).collect();
    for (id, _name) in graph.store_declaration_names() {
        ids.insert(id.get());
    }
    ids
}

fn declaration_fqn(graph: &Graph, id: DeclarationId) -> String {
    graph
        .declaration(id)
        .map_or_else(|| format!("<missing {}>", id.get()), |d| d.name().to_string())
}

/// Probes every declaration's full structure: name, kind, owner, members, ancestors,
/// descendants, singleton class, and each definition's kind/uri/offset.
fn probe_declarations(graph: &Graph, sample: usize, out: &mut Vec<(String, String)>) {
    for raw in cap_sample(all_declaration_ids(graph), sample) {
        let id = DeclarationId::new(raw);
        let Some(decl) = graph.declaration(id) else {
            out.push((format!("decl:id:{raw}"), "<missing>".into()));
            continue;
        };
        let fqn = decl.name().to_string();
        let owner = declaration_fqn(graph, *decl.owner_id());
        let (members, ancestors, descendants): (Vec<String>, Vec<String>, Vec<String>) = match decl.as_namespace() {
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
            .as_namespace()
            .and_then(|ns| ns.singleton_class())
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

fn candidate_names(graph: &Graph, candidates: &[rubydex::query::CompletionCandidate]) -> String {
    use rubydex::query::CompletionCandidate;
    candidates
        .iter()
        .map(|c| match c {
            CompletionCandidate::Declaration(id) => declaration_fqn(graph, *id),
            CompletionCandidate::KeywordArgument(str_id) => graph.string(*str_id).map_or_else(
                || format!("<missing string {}>", str_id.get()),
                |s| s.as_str().to_string(),
            ),
            CompletionCandidate::Keyword(keyword) => format!("kw:{}", keyword.name()),
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(",")
}

fn probe_completion(graph: &Graph, name_ids: &[NameId], sample: usize, out: &mut Vec<(String, String)>) {
    use rubydex::query::{CompletionContext, CompletionReceiver, completion_candidates};

    // Expression completion at every interned name (lexical scope = that name, self derived).
    // Name ids are taken from the in-memory graph: ids are deterministic content hashes,
    // so the same ids are valid keys in the store.
    for name_id in name_ids {
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
    let namespaces = cap_sample(
        all_declaration_ids(graph)
            .iter()
            .filter(|raw| {
                let id = DeclarationId::new(**raw);
                graph.declaration(id).is_some_and(|d| d.as_namespace().is_some())
            })
            .map(|raw| declaration_fqn(graph, DeclarationId::new(*raw)))
            .collect::<BTreeSet<_>>(),
        sample,
    );
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

fn probe_aliases(graph: &Graph, sample: usize, out: &mut Vec<(String, String)>) {
    use rubydex::query::follow_method_alias;

    // Every definition of every declaration: alias definitions resolve to a target,
    // non-alias definitions error deterministically. Both must match across graphs.
    for raw in cap_sample(all_declaration_ids(graph), sample) {
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
    use rubydex::model::ids::StringId;
    use rubydex::query::find_member_in_ancestors;

    let cases: &[(&str, &str)] = &[
        ("Child", "parent_method"), // inherited
        ("Child", "nickname"),      // alias
        ("Child", "state"),         // attr_accessor
        ("Child", "solo"),          // singleton method on Child
        ("Child", "zzz_missing"),   // expect Err(MemberNotFound)
        ("Parent", "base_method"),  // included module
        ("Util", "util_method"),    // def self.
    ];
    for (owner, member) in cases {
        let result = find_member_in_ancestors(graph, DeclarationId::from(*owner), StringId::from(*member), false);
        let rendered = match result {
            Ok(target) => format!("Ok({})", declaration_fqn(graph, target)),
            Err(error) => format!("Err({error:?})"),
        };
        out.push((format!("member:{owner}:{member}"), rendered));
    }
}

fn probe_require(graph: &Graph, out: &mut Vec<(String, String)>) {
    use rubydex::query::{require_paths, resolve_require_path};

    let root = corpus_dir();
    let root_slice = std::slice::from_ref(&root);
    let paths = require_paths(graph, root_slice);
    out.push((
        "require:paths".into(),
        paths
            .into_iter()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(","),
    ));
    for path in ["base", "parent", "child", "util", "refs", "kid", "nope"] {
        let resolved = resolve_require_path(graph, path, root_slice).map_or_else(
            || "None".into(),
            |uri_id| {
                graph
                    .document(uri_id)
                    .map_or_else(|| "<missing document>".into(), |d| d.uri().to_string())
            },
        );
        out.push((format!("require:resolve:{path}"), resolved));
    }
}

fn probe_documents(graph: &Graph, sample: usize, out: &mut Vec<(String, String)>) {
    use rubydex::model::ids::UriId;

    let mut uris: BTreeSet<u64> = graph.documents().keys().map(rubydex::model::id::Id::get).collect();
    for (id, _uri) in graph.store_document_uris() {
        uris.insert(id.get());
    }
    for raw in cap_sample(uris, sample) {
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
                let r = graph
                    .constant_reference(*ref_id)
                    .expect("const ref present for document");
                format!("{}@{}", r.name_id().get(), r.offset().start())
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let method_refs: Vec<String> = doc
            .method_references()
            .iter()
            .map(|ref_id| {
                let r = graph
                    .method_reference(*ref_id)
                    .expect("method ref present for document");
                let receiver = r.receiver().map_or(u64::MAX, |n| n.get());
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

fn probe_cypher(graph: &Graph, out: &mut Vec<(String, String)>) {
    use rubydex::query::cypher::{OutputFormat, run_query};

    for q in [
        "MATCH (c:Class) RETURN c.name",
        "MATCH (c:Class) WHERE c.name = 'Child' RETURN c.name",
        "MATCH (c:Class) RETURN c.name, c.definition_count, c.visibility",
        "MATCH (d:Definition) RETURN d.name",
        "MATCH (doc:Document) RETURN doc.uri",
        "MATCH (c:Class)-[:HasParent]->(p) RETURN c.name, p.name",
        "MATCH (c:Class)-[:Owns]->(m) RETURN c.name, m.name",
        "MATCH (c:Class)-[:HasAncestor]->(a) RETURN c.name, a.name",
    ] {
        // Table output, sorted lines: iteration order over graph maps can differ
        // between the memory and store graphs, so only sorted content is comparable.
        let result = run_query(graph, q, OutputFormat::Table).unwrap_or_else(|error| format!("Err({error})"));
        let lines: Vec<&str> = result.lines().collect();
        let body: Vec<&str> = if lines.last().is_some_and(|l| l.contains("row")) {
            lines[..lines.len() - 1].to_vec()
        } else {
            lines
        };
        let mut sorted = body;
        sorted.sort_unstable();
        out.push((format!("cypher:{q}"), sorted.join("\n")));
    }
}

fn probe_search(graph: &Graph, sample: usize, out: &mut Vec<(String, String)>) {
    use rubydex::query::{MatchMode, declaration_search};

    // Every declaration must be findable by exact-FQN search (search is substring-based,
    // so the result set may include other names — the invariant is an identical result
    // set on both graphs).
    let fqns: Vec<String> = cap_sample(all_declaration_ids(graph), sample)
        .into_iter()
        .filter_map(|raw| graph.declaration(DeclarationId::new(raw)).map(|d| d.name().to_string()))
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

    for (label, mode, q) in [
        ("exact", &MatchMode::Exact, "Ch"),
        ("fuzzy", &MatchMode::Fuzzy, "chd"),
        ("fuzzy", &MatchMode::Fuzzy, "base"),
    ] {
        let found = declaration_search(graph, &[q], mode);
        let names: Vec<String> = found
            .iter()
            .map(|id| declaration_fqn(graph, *id))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        out.push((format!("search:{label}:{q}"), names.join(",")));
    }
}

fn normalize(entries: &mut [(String, String)]) {
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
    std::fs::write(extended.join("extra.rb"), "class Extra\n  def extra_method; end\nend\n").expect("write extra");

    let memory = build_graph_from(&corpus_dir());
    let store_graph = build_store_graph_from(&extended, dir.path());
    let name_ids: Vec<NameId> = memory.names().keys().copied().collect();

    let mut mem_probe = Vec::new();
    probe_all(&memory, &name_ids, 0, &mut mem_probe);
    let mut store_probe = Vec::new();
    probe_all(&store_graph, &name_ids, 0, &mut store_probe);
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
    probe_all(&memory, &name_ids, 0, &mut mem_probe);
    let mut store_probe = Vec::new();
    probe_all(&store_graph, &name_ids, 0, &mut store_probe);
    normalize(&mut mem_probe);
    normalize(&mut store_probe);

    assert_eq!(mem_probe.len(), store_probe.len(), "probe case count differs");
    for (m, s) in mem_probe.iter().zip(store_probe.iter()) {
        assert_eq!(m.0, s.0, "probe case sets differ");
        assert_eq!(m.1, s.1, "divergence in case {}", m.0);
    }
}

/// Copies the fixture corpus into `dir/ws` so edits can be applied in place. URIs embed absolute
/// paths, so the store build and the fresh reference build must see the same paths.
fn copy_corpus(dir: &Path) -> PathBuf {
    let ws = dir.join("ws");
    std::fs::create_dir_all(&ws).expect("mkdir ws");
    for entry in std::fs::read_dir(corpus_dir()).expect("read corpus") {
        let entry = entry.expect("dir entry");
        std::fs::copy(entry.path(), ws.join(entry.file_name())).expect("copy fixture");
    }
    ws
}

/// Edit differential: apply the same edits incrementally to an in-memory graph AND to a store-backed
/// graph, then require identical probes. The contract is store-backed incremental == in-memory
/// incremental (what a session's overlay must reproduce); incremental != fresh-rebuild is a property
/// of the engine's own invalidation, already true of the in-memory path, and not this layer's job.
/// `Some(src)` writes/replaces a file, `None` deletes it.
fn edit_case(edits: &[(&str, Option<&str>)]) {
    let dir = tempfile::tempdir().expect("tempdir");
    let ws = copy_corpus(dir.path());
    let mut memory = build_graph_from(&ws);
    let mut store_graph = build_store_graph_from(&ws, dir.path());

    for (file, content) in edits {
        let path = ws.join(file);
        let uri = url::Url::from_file_path(&path).expect("file uri").to_string();
        let uri_memory = uri.clone();
        if let Some(source) = content {
            std::fs::write(&path, source).expect("write edit");
            rubydex::indexing::index_source(
                &mut store_graph,
                uri.into(),
                source,
                &rubydex::indexing::LanguageId::Ruby,
            );
            rubydex::indexing::index_source(
                &mut memory,
                uri_memory.into(),
                source,
                &rubydex::indexing::LanguageId::Ruby,
            );
        } else {
            std::fs::remove_file(&path).expect("delete file");
            store_graph.delete_document(&uri);
            memory.delete_document(&uri_memory);
        }
    }
    Resolver::new(&mut store_graph).resolve();
    Resolver::new(&mut memory).resolve();

    let name_ids: Vec<NameId> = memory.names().keys().copied().collect();
    let mut mem_probe = Vec::new();
    probe_all(&memory, &name_ids, 0, &mut mem_probe);
    let mut store_probe = Vec::new();
    probe_all(&store_graph, &name_ids, 0, &mut store_probe);
    normalize(&mut mem_probe);
    normalize(&mut store_probe);

    // Key-based comparison: a length mismatch must show up as missing/extra keys, not as a shifted zip.
    let mem_keys: HashSet<String> = mem_probe.iter().map(|(key, _)| key.clone()).collect();
    let store_keys: HashSet<String> = store_probe.iter().map(|(key, _)| key.clone()).collect();
    let missing: Vec<String> = mem_keys
        .iter()
        .filter(|key| !store_keys.contains(key.as_str()))
        .cloned()
        .collect();
    let extra: Vec<String> = store_keys
        .iter()
        .filter(|key| !mem_keys.contains(key.as_str()))
        .cloned()
        .collect();
    let diverged: Vec<String> = mem_probe
        .iter()
        .filter_map(|(key, value)| {
            store_probe
                .iter()
                .find(|(other, _)| other == key)
                .filter(|(_, other_value)| other_value != value)
                .map(|(_, other_value)| format!("{key}\n  memory: {value}\n  store:  {other_value}"))
        })
        .collect();
    let mut report: Vec<String> = diverged.iter().take(6).cloned().collect();
    report.extend(missing.iter().take(6).map(|key| format!("MISSING {key}")));
    report.extend(extra.iter().take(6).map(|key| format!("EXTRA {key}")));
    assert!(
        mem_probe.len() == store_probe.len() && diverged.is_empty(),
        "edit differential: {} vs {} probes, {} diverged, {} missing, {} extra:\n{}",
        mem_probe.len(),
        store_probe.len(),
        diverged.len(),
        missing.len(),
        extra.len(),
        report.join("\n"),
    );
}

const PARENT_WITH_UTIL: &str = "require \"base\"\n\nclass Parent\n  include Util\n\n  attr_accessor :state\n\n  def parent_method\n    state\n  end\nend\n";
const CHILD_NO_SUPERCLASS: &str = "class Child\n  def child_method; end\n\n  def self.solo\n    \"solo\"\n  end\nend\n";

#[test]
fn edit_add_file() {
    edit_case(&[("extra.rb", Some("class Extra < Child\n  def extra_method; end\nend\n"))]);
}

#[test]
fn edit_modify_method_body() {
    edit_case(&[(
        "util.rb",
        Some("module Util\n  UTIL_CONST = 7\n\n  def self.other_method\n    UTIL_CONST\n  end\nend\n"),
    )]);
}

#[test]
#[ignore = "red until Phase 1 Task 1.3 (store-node mutation routing)"]
fn edit_delete_leaf_file() {
    edit_case(&[("kid.rb", None)]);
}

#[test]
#[ignore = "red until Phase 1 Task 1.3 (store-node mutation routing)"]
fn edit_delete_superclass_file() {
    edit_case(&[("parent.rb", None)]);
}

#[test]
#[ignore = "red until Phase 1 Task 1.3 (store-node mutation routing)"]
fn edit_change_mixin() {
    edit_case(&[("parent.rb", Some(PARENT_WITH_UTIL))]);
}

#[test]
#[ignore = "red until Phase 1 Task 1.3 (store-node mutation routing)"]
fn edit_drop_superclass() {
    edit_case(&[("child.rb", Some(CHILD_NO_SUPERCLASS))]);
}

#[test]
#[ignore = "red until Phase 1 Task 1.3 (store-node mutation routing)"]
fn edit_rename_module() {
    edit_case(&[("base.rb", Some("module Base2\n  BASE_CONST = \"base\"\nend\n"))]);
}

#[test]
#[ignore = "red until Phase 1 Task 1.3 (store-node mutation routing)"]
fn edit_delete_then_restore() {
    let original = std::fs::read_to_string(corpus_dir().join("parent.rb")).expect("read parent.rb");
    edit_case(&[("parent.rb", None), ("parent.rb", Some(original.as_str()))]);
}

#[test]
#[ignore = "red until Phase 1 Task 1.3 (store-node mutation routing)"]
fn edit_multi_file_branch_switch() {
    edit_case(&[
        ("kid.rb", None),
        ("parent.rb", Some(PARENT_WITH_UTIL)),
        ("child.rb", Some(CHILD_NO_SUPERCLASS)),
        ("extra.rb", Some("class Extra < Parent; end\n")),
    ]);
}

/// Replaces only the first occurrence of `from` in `s` (renames one declaration per edit).
fn replace_first(s: &str, from: &str, to: &str) -> String {
    match s.find(from) {
        Some(idx) => {
            format!("{}{}{}", &s[..idx], to, &s[idx + from.len()..])
        }
        None => s.to_string(),
    }
}

/// Heavy: applies a deterministic edit script to a copy of a real corpus through the live-edit API
/// and requires sampled probes to match the same edits applied to an in-memory graph. Seed-fixed so
/// a failure reproduces exactly.
#[test]
#[ignore = "set RUBYDEX_DIFF_CORPUS to a Ruby source tree to run"]
fn edit_soak_on_corpus() {
    let corpus = PathBuf::from(std::env::var("RUBYDEX_DIFF_CORPUS").expect("RUBYDEX_DIFF_CORPUS"));
    let mut seed: u64 = std::env::var("RUBYDEX_SOAK_SEED").map_or(11, |s| s.parse().expect("seed"));
    let edits: usize = std::env::var("RUBYDEX_SOAK_EDITS").map_or(200, |s| s.parse().expect("edits"));
    let mut next = || -> usize {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        usize::try_from(seed >> 33).expect("fits")
    };

    let dir = tempfile::tempdir().expect("tempdir");
    let ws = dir.path().join("ws");
    let status = std::process::Command::new("cp")
        .arg("-r")
        .arg(&corpus)
        .arg(&ws)
        .status()
        .expect("cp");
    assert!(status.success(), "copy corpus");

    let mut memory = build_graph_from(&ws);
    let mut store_graph = build_store_graph_from(&ws, dir.path());
    let mut files: Vec<PathBuf> = {
        let (paths, _) = collect_file_paths(
            vec![ws.to_string_lossy().into_owned()],
            &store_graph.excluded_patterns(),
        );
        paths
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "rb"))
            .collect()
    };
    files.sort();

    for round in 0..edits {
        let path = files[next() % files.len()].clone();
        let uri = url::Url::from_file_path(&path).expect("uri").to_string();
        let uri_memory = uri.clone();
        match next() % 3 {
            0 if path.exists() => {
                std::fs::remove_file(&path).expect("delete");
                store_graph.delete_document(&uri);
                memory.delete_document(&uri_memory);
            }
            1 => {
                let mut src = std::fs::read_to_string(&path).unwrap_or_default();
                let snippet = format!("\nclass RdxSoak{round} < Object; def soak_{round}; end; end\n");
                src.push_str(&snippet);
                std::fs::write(&path, &src).expect("append");
                rubydex::indexing::index_source(
                    &mut store_graph,
                    uri.into(),
                    &src,
                    &rubydex::indexing::LanguageId::Ruby,
                );
                rubydex::indexing::index_source(
                    &mut memory,
                    uri_memory.into(),
                    &src,
                    &rubydex::indexing::LanguageId::Ruby,
                );
            }
            _ => {
                let src = std::fs::read_to_string(&path).unwrap_or_default();
                let src = replace_first(&src, "\nclass ", &format!("\nclass Soak{round}"));
                let src = replace_first(&src, "\nmodule ", &format!("\nmodule Soak{round}"));
                std::fs::write(&path, &src).expect("rewrite");
                rubydex::indexing::index_source(
                    &mut store_graph,
                    uri.into(),
                    &src,
                    &rubydex::indexing::LanguageId::Ruby,
                );
                rubydex::indexing::index_source(
                    &mut memory,
                    uri_memory.into(),
                    &src,
                    &rubydex::indexing::LanguageId::Ruby,
                );
            }
        }
    }
    Resolver::new(&mut store_graph).resolve();
    Resolver::new(&mut memory).resolve();

    let name_ids: Vec<NameId> = memory.names().keys().copied().collect();
    let mut mem_probe = Vec::new();
    probe_all(&memory, &name_ids, 2_000, &mut mem_probe);
    let mut store_probe = Vec::new();
    probe_all(&store_graph, &name_ids, 2_000, &mut store_probe);
    normalize(&mut mem_probe);
    normalize(&mut store_probe);

    let mem_keys: HashSet<String> = mem_probe.iter().map(|(key, _)| key.clone()).collect();
    let extra: usize = store_probe
        .iter()
        .filter(|(key, _)| !mem_keys.contains(key.as_str()))
        .count();
    assert_eq!(mem_probe.len(), store_probe.len() - extra, "probe case count differs");
    for (m, s) in mem_probe.iter().zip(store_probe.iter()) {
        assert_eq!(m, s, "soak divergence (seed/edits reproduce it)");
    }
}
