//! Differential tests: every query API must return identical results on the in-memory
//! graph and on the store-backed graph built from the same corpus. The in-memory graph
//! is the reference; the store path is only correct if it reproduces it exactly.

#![cfg(feature = "redb-store")]

use rubydex::{
    indexing::{index_files, IndexerBackend},
    listing::collect_file_paths,
    model::{
        declaration::Ancestor,
        graph::Graph,
        ids::{DeclarationId, NameId},
        store::RedbStore,
    },
    resolution::Resolver,
};
use std::collections::BTreeSet;
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
    probe_declarations(graph, out);
    probe_completion(graph, name_ids, out);
    probe_search(graph, out);
    probe_aliases(graph, out);
    probe_find_member(graph, out);
    probe_require(graph, out);
    probe_documents(graph, out);
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
            CompletionCandidate::KeywordArgument(str_id) => graph
                .string(*str_id)
                .map_or_else(|| format!("<missing string {}>", str_id.get()), |s| s.as_str().to_string()),
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
    let namespaces: Vec<String> = all_declaration_ids(graph)
        .iter()
        .filter(|raw| {
            let id = DeclarationId::new(**raw);
            graph
                .declaration(id)
                .is_some_and(|d| d.as_namespace().is_some())
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
        ("Child", "solo"),          // singleton method on Child
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

fn probe_documents(graph: &Graph, out: &mut Vec<(String, String)>) {
    use rubydex::model::ids::UriId;

    let mut uris: BTreeSet<u64> = graph.documents().keys().map(rubydex::model::id::Id::get).collect();
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

fn probe_search(graph: &Graph, out: &mut Vec<(String, String)>) {
    use rubydex::query::{declaration_search, MatchMode};

    // Every declaration must be findable by exact-FQN search (search is substring-based,
    // so the result set may include other names — the invariant is an identical result
    // set on both graphs).
    let fqns: Vec<String> = all_declaration_ids(graph)
        .iter()
        .filter_map(|raw| {
            graph
                .declaration(DeclarationId::new(*raw))
                .map(|d| d.name().to_string())
        })
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

    for (label, mode, q) in [("exact", &MatchMode::Exact, "Ch"), ("fuzzy", &MatchMode::Fuzzy, "chd"), ("fuzzy", &MatchMode::Fuzzy, "base")] {
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
