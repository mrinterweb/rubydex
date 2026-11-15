# RubyDex MCP Code-Graph Host Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the existing RubyDex MCP server a low-memory, honest code-graph entry point for agents, without claiming to know business concepts or resolved method-call targets it does not know.

**Architecture:** Keep the MCP transport and agent-facing tools in RubyDex. Default `rdx mcp` to the existing disk-index path, reuse `Graph` and the query schema, and expose a name-based method-call search with explicit limits on what it means. Serialize tool calls against a single graph when refresh can replace its store. Do not build a second graph or a separate gem.

**Tech Stack:** Ruby 4, existing C/Rust extension, redb store (optional build feature), MCP JSON-RPC over stdio, Minitest. No new dependencies.

**Spec:** [Design basis and findings](#design-basis-and-findings) below in this same file. This plan covers the first, independently shippable MCP increment; the remaining items in the assessment are separate projects, not implied implementation tasks.

## Global Constraints

- Keep `rdx mcp [PATH]`, its seven existing tool schemas, and `RUBYDEX_DISK_INDEX=0` behavior compatible. Only the MCP command defaults to disk; Ruby LSP and `rdx query` retain their existing defaults. Because env overrides `rubydex.toml`, a workspace that explicitly disables the disk index needs `RUBYDEX_DISK_INDEX=0` when launching MCP.
- Never start `rubydex-index-manager` implicitly: keep `RUBYDEX_INDEX_MANAGER` opt-in.
- When redb is not compiled in, or store attach/build fails, preserve `Graph#index_workspace`'s existing in-memory fallback; do not fail MCP startup.
- Do not describe `MethodReference#receiver` as a resolved target method. It can identify a constant receiver; calls with dynamic receivers remain ambiguous.
- Do not expose unrestricted Cypher in MCP until execution can be bounded *before* intermediate results are materialized; `LIMIT` and output truncation do not suffice with `cypher-parser 0.8.1`.
- Preserve Linux, macOS and Windows behavior. Tests must pass with `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null`; do not add long fixed sleeps to probes.

## Review Focus

- An inherited `RUBYDEX_DISK_INDEX=0` must still give an in-memory MCP server (Task 1 test).
- A build without `redb-store` must still answer MCP queries (Task 1 existing e2e test, run in the no-store CI job).
- Two concurrent tool requests must never use one graph while another request detaches/re-attaches its store (Task 2 test).
- Calls of the same name on different receiver types must not be reported as calls to one specific declaration (Task 4 test).
- A warm server must see a source change on the next tool call without presenting stale results as fresh (Task 2 regression test; allow the existing freshness TTL without fixed waits).

---

## Scope and file map

This plan ships four reviewable changes: MCP-only disk default; safe graph access during refresh; static schema discovery; exact-name, **unresolved-target** method-call search. All work stays within the existing Ruby MCP module; no new general-purpose interface or store-format change.

| File | Responsibility |
|---|---|
| `lib/rubydex/cli/command/mcp.rb` | MCP-only disk default and explicit opt-out |
| `lib/rubydex/mcp_server.rb` | Serialize refresh + tool evaluation against one graph |
| `lib/rubydex/mcp_server/tools/describe_schema_tool.rb` | Read-only `Rubydex::Query.schema("json")` projection |
| `lib/rubydex/mcp_server/tools/find_method_references_tool.rb` | Exact unqualified-name search, bounded output, no target claim |
| `test/mcp_command_test.rb` | Default/override contract without building a store |
| `test/mcp_server_test.rb` | Tool dispatch and refresh/read serialization |
| `test/mcp_server_tools_test.rb` | Schema and name-based search semantics |
| `test/integration/mcp_server_test.rb` | Existing stdio e2e; MCP commands really use the feature they advertise |
| `test/disk_index_enumerators_test.rb` | Method search against the attached store |

## Task 1: Disk-backed default for `rdx mcp` only

**Files:** Modify `lib/rubydex/cli/command/mcp.rb`; create `test/mcp_command_test.rb`; modify `test/integration/mcp_server_test.rb`.

**Interfaces:** Consumes existing `Graph#index_workspace`, `RUBYDEX_DISK_INDEX` override and `MCPServer.run(path)`. Produces `Command::Mcp#run` with an MCP-only default of `RUBYDEX_DISK_INDEX=1` when the environment variable is absent; an explicit `0` wins. No manager default and no new command flags.

- [ ] **Step 1: Write failing tests.** In `test/mcp_command_test.rb`, test `Mcp.new([path]).run` with `MCPServer.run` stubbed: absent env is `"1"` inside the call, explicit `"0"` remains `"0"`, and neither case changes `RUBYDEX_INDEX_MANAGER`. Restore env in `ensure`. In the existing stdio e2e test, pass `"RUBYDEX_DISK_INDEX" => nil`, `"RUBYDEX_INDEX_MANAGER" => "0"`, and an isolated `RUBYDEX_CACHE_DIR` to the subprocess; when `Graph.store_format_version.positive?`, assert the store exists after readiness, and keep the existing `Dog` search assertion (which must work in the no-store build too).
- [ ] **Step 2: Run the red tests.** `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest test/mcp_command_test.rb` and `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest test/integration/mcp_server_test.rb -n test_mcp_server_e2e`; expect the absent-env assertion to fail and, on a redb build, the store assertion to fail.
- [ ] **Step 3: Implement `Rubydex::CLI::Command::Mcp#run`.** Set `ENV["RUBYDEX_DISK_INDEX"]` to `"1"` only when absent, after option validation and before `MCPServer.run(path)`. Preserve the old fallback in `Graph#index_workspace` rather than adding a second path.
- [ ] **Step 4: Verify both tests pass.** Re-run both Step 2 commands; verify the `Dog` query also succeeds in the no-store CI build. Do not compile a second feature set locally just for the unit test.
- [ ] **Step 5: Commit.** Stage only these three files; message `Default MCP to the disk-backed index`.

## Task 2: Coherent MCP tool calls across store refresh

**Files:** Modify `lib/rubydex/mcp_server.rb`; modify `test/mcp_server_test.rb`.

**Interfaces:** Consumes `Graph#prepare_for_tool_call` and `Tool#call(**arguments)`. Produces `Server#call_tool(params)` with refresh and tool evaluation serialized by a server-owned `Mutex`; ping, initialization and `tools/list` do not take this lock.

- [ ] **Step 1: Write failing tests.** Stub `Server#graph_or_error` with one graph and the chosen tool's `#call`; use queues to block in `prepare_for_tool_call` on one request and signal when a second request begins. Assert the second cannot enter `prepare_for_tool_call` until the first tool returns; release both queues in `ensure` so a failed assertion cannot hang the suite. Add a source-change regression using a tiny disk-backed workspace: after the server becomes ready, change an indexed file and assert a subsequent tool call reports its new declaration (`RUBYDEX_REFRESH_TTL=0`, no fixed sleep; skip when redb is not compiled in).
- [ ] **Step 2: Run the red tests.** `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest test/mcp_server_test.rb`; expect the overlapping-requests assertion to fail.
- [ ] **Step 3: Implement `Server#call_tool(params)` locking.** Create one `Mutex` in `Server#initialize`; lock from `graph.prepare_for_tool_call` through `tool.new(graph).call(**arguments.transform_keys(&:to_sym))`. Leave argument validation and non-graph methods outside the lock. Do not copy graph state between requests.
- [ ] **Step 4: Run tests.** The same test file and `test/disk_index_session_refresh_test.rb` must pass; check that the existing concurrent `handle` dispatch test still passes.
- [ ] **Step 5: Commit.** Stage the server and test file; message `Serialize MCP reads with store refresh`.

## Task 3: Discover the existing query schema through MCP

**Files:** Create `lib/rubydex/mcp_server/tools/describe_schema_tool.rb`; modify `test/mcp_server_tools_test.rb` and `test/integration/mcp_server_test.rb`.

**Interfaces:** Consumes `Rubydex::Query.schema("json")`, `BaseTool#response`, and automatic `*_tool.rb` registration. Produces tool `describe_schema` with no arguments; JSON payload contains `node_labels`, `relationships`, and `properties`, parsed from the existing schema source. Do not duplicate labels or relationships in Ruby.

- [ ] **Step 1: Write failing tests.** Assert `describe_schema` appears in `tools/list`, the tool result has `node_labels` containing `Class`, `relationships` containing `DEFINES`, and `properties` containing `name`; reject unknown arguments using existing server validation. Assert a successful stdio `tools/call` has `isError: false`.
- [ ] **Step 2: Run the red tests.** `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest test/mcp_server_tools_test.rb` and the integration file; expect unknown-tool/registration failures.
- [ ] **Step 3: Implement `Rubydex::MCPServer::DescribeSchemaTool#call() -> Tool::Response`.** Use `JSON.parse(Rubydex::Query.schema("json"))` and the existing `response` helper; make it a `BaseTool` so it follows the existing discovery and dispatch contract.
- [ ] **Step 4: Run the two test files.** Expect the schema assertions to pass in either redb feature set.
- [ ] **Step 5: Commit.** Stage the tool and tests; message `Expose graph schema to MCP agents`.

## Task 4: Find method-call sites without inventing target resolution

**Files:** Create `lib/rubydex/mcp_server/tools/find_method_references_tool.rb`; modify `test/mcp_server_tools_test.rb`, `test/disk_index_enumerators_test.rb`, and `test/integration/mcp_server_test.rb`.

**Interfaces:** Consumes `Graph#method_references`, `MethodReference#name`, `#location`, `BaseTool#paginate`, `#format_path`. Produces `find_method_references(name:, limit: nil, offset: nil) -> Tool::Response`: exact **unqualified** call name, `{ name:, references: [{path:, line:, column:}], total:, target_resolution: "unknown" }`, default page 50/max 100. Sort by path/line/column before pagination; no claim that results belong to any particular method declaration.

- [ ] **Step 1: Write failing tests.** Use two classes that both define `rdx_ontology_probe` and a third file that calls each: `name: "rdx_ontology_probe"` returns both locations, `total: 2`, `target_resolution: "unknown"`; `limit: 1, offset: 1` returns only the second; an unknown name returns an empty page. Add the call assertions to an *existing* store-backed test in `test/disk_index_enumerators_test.rb` so no extra store build is required; keep the original test running on no-store builds, and skip only the new store-specific assertions when redb is absent. In the stdio test, assert the new tool is listed and callable.
- [ ] **Step 2: Run the red tests.** `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec ruby -Itest test/mcp_server_tools_test.rb` and `test/disk_index_enumerators_test.rb`; expect an unregistered-tool failure.
- [ ] **Step 3: Implement `Rubydex::MCPServer::FindMethodReferencesTool#call(name:, limit: nil, offset: nil) -> Tool::Response`.** Select `ref.name == name`, sort by displayed location, paginate via `BaseTool#paginate(..., 100)`, and use `BaseTool#response`. Do not use the currently unproven `MethodDeclaration#references` as a target mapping. Keep the tool description explicit about dynamic dispatch and false positives.
- [ ] **Step 4: Run tests.** Run the two files plus `test/integration/mcp_server_test.rb`; expect both in-memory and attached-store cases to pass.
- [ ] **Step 5: Commit.** Stage the tool and tests; message `Expose name-based method call sites over MCP`.

## Final verification and next project gates

- [ ] Run `GIT_CONFIG_GLOBAL=/dev/null GIT_CONFIG_SYSTEM=/dev/null bundle exec rake ruby_test` and `bundle exec rake lint`; record failures without silently changing the scope.
- [ ] Manually compare three agent questions on a real repository against `rg`: definition, constant references, and same-name method calls. Record answer correctness, tool response size, warm RSS and startup time for one and several concurrent MCP sessions. The 67 MB single-process figure is a baseline, not a multi-agent guarantee.
- [ ] **Separate plan, not part of these commits:** bounded `query_graph`. `cypher-parser 0.8.1` applies `LIMIT` after building intermediate rows (`executor.rs`), so an MCP output cap is not an execution budget. Add an engine-level work/memory budget with deterministic failure tests before exposing raw queries to agents; do not concatenate `LIMIT` to arbitrary input as a substitute.
- [ ] **Separate plan after actual examples of business concepts:** curated annotations in a workspace sidecar with provenance and inferred/reviewed status, joining to names and source locations. Define what happens on rename, branches, and stale annotations before choosing JSONL/SQLite. Do not bake Rails/domain labels into the redb code index by default.
- [ ] **Separate measurement-driven decision:** a shared local MCP endpoint (with access control) if multiple stdio agents exhaust memory; a Rust MCP binary only if a measured deployment still needs less RAM. Neither changes this plan's tool semantics.
- [ ] **Separate correctness project:** investigate whether method references can be resolved to method declarations, including dynamic calls and the existing `graph.rs:1503` refresh panic. A `CALLS`/method-target Cypher edge requires this, not just projecting stored `MethodRef` records.

## Design basis and findings

Assessment originally captured 2026-11-14; corrected for the seven-tool count, 11 relationships,
and the distinction between recorded calls and resolved method targets.

Assessment of using RubyDex as the code index behind agent tooling, and as a host for a
code-graph ontology. All memory numbers are measured on `~/code/reserv/reserv-api`
(6,423 `.rb` files, 21,505 files total, 538,972,160 B prebuilt store) with release builds.

## Summary

RubyDex already ships an MCP server (`rdx mcp`) and a Cypher query engine (`rdx query`).
The immediate gaps are MCP schema discovery and a low-memory default; Cypher cannot safely
be opened to arbitrary agent queries until its execution has resource bounds, and method call
sites are not the same as resolved call-target edges.

Measured per-process cost with the store attached is **67 MB** (Ruby VM included). The
complaint that motivated this — ruby-lsp-mcp costing hundreds of MB per agent — is a
property of booting Ruby LSP (Ruby VM + Bundler + LSP + Rails add-on), not of the graph.

| Process | Peak RSS |
|---------|---------|
| `rdx mcp <workspace>`, store attached | **67 MB** |
| `ruby-lsp` session, store attached | 119–133 MB |
| `ruby_lsp_rails/serve` (Rails add-on, one per session) | 420–438 MB |
| Raw Rust read path (`rubydex_cli --open-store ... --search`) | **24 MB**, 57 ms |
| Full in-memory index of the same workspace (Rust CLI, not MCP) | 192 MB |

Reproduce:

```bash
RUBYDEX_DISK_INDEX=1 RUBYDEX_INDEX_MANAGER=0 \
  bundle exec ruby -Ilib exe/rdx mcp "$HOME/code/reserv/reserv-api"
```

The 24 MB / 57 ms figure is `rust/target/release/rubydex_cli --open-store
<store> --search Service`, sampling `VmHWM` while it runs. Concurrent readers are fine: that
command attached the same store while two live `ruby-lsp` sessions held it open. The store is
opened read-only with an 8 MiB page cache (`rust/rubydex/src/model/store.rs`, `Store::open`),
which is what keeps a long-lived server lean.

## What already exists

- `exe/rdx mcp [PATH]` — JSON-RPC over stdio, thread pool (`WORKER_COUNT = 4`), indexer on a
  background thread, paginated tool responses.
- `lib/rubydex/mcp_server.rb` (249 lines), `mcp_server/protocol.rb` (156), seven tools
  (~380 lines total): `search_declarations`, `get_declaration`, `get_file_declarations`,
  `get_descendants`, `find_constant_references`, `find_dead_code_candidates`,
  `codebase_stats`.
- `exe/rdx query <CYPHER>` and `Rubydex::Query.parse(...).run(graph)`, with
  `Rubydex::Query.schema` for introspection (`rust/rubydex-sys/src/cypher_api.rs`,
  `ext/rubydex/query.c`).

The tool descriptions are already written for agents (fuzzy vs exact match modes, page size
defaulting to 50 and capped at 100, structured errors with a `suggestion`). That design is
pointing the right way; it just stops short of the most useful surface.

## The schema is already a structural code graph

`rdx query --schema` prints 13 node labels and 11 relationship types:

- **Labels**: `Document`, `Definition`, `Declaration`, `Namespace`, `Class`, `Module`,
  `SingletonClass`, `Method`, `Constant`, `ConstantAlias`, `GlobalVariable`,
  `InstanceVariable`, `ClassVariable`.
- **Relationships**: `DEFINES` (Document → Definition), `DECLARES` (Definition →
  Declaration), `CONTAINS` (lexical nesting), `HAS_PARENT` (superclass, `*` for the chain),
  `INCLUDES` / `PREPENDS` / `EXTENDS` (mixins), `OWNS` (namespace → member),
  `HAS_ANCESTOR` / `HAS_DESCENDANT` (linearized chains), `REFERENCES` (Document →
  constant Declaration).
- **Properties**: `name`, `unqualified_name`, `visibility`, `definition_count`, `file`,
  `line`, `uri`, `path`.

That is a traversable structural code graph with an introspectable schema, a better starting
point than building a second model beside the index. It does not yet express domain semantics.

## Gaps, in priority order

1. **Cypher is not exposed over MCP.** Seven fixed questions instead of arbitrary ones. Expose
   `describe_schema` now; raw `query_graph` needs an engine-level execution budget first:
   `cypher-parser 0.8.1` materializes intermediate rows before applying `LIMIT`.
2. **No resolved method-call edges.** `REFERENCES` covers constants only. Method call sites
   are stored and can be searched by name, but the code examined does not establish a reliable
   call → method declaration mapping. Name matches are not proof of a target; investigate
   resolution before offering "who calls this particular method" or a `CALLS` edge.
3. **In-memory indexing by default.** `rdx mcp` calls `index_workspace` without enabling disk mode.
   The Rust CLI's full-index peak was 192 MB; MCP's in-memory peak was not measured here. Store
   attach/build should be the MCP command's default, with an explicit environment opt-out.
4. **No trustworthy freshness metadata per response.** The server instructions mention a
   staleness marker, but a store mtime or HEAD alone does not prove a refresh succeeded. Design
   an `as_of`/status result only after refresh failure is distinguishable from no change.
5. **Missing high-value queries**: tests for a declaration, diagnostics, symbol at a position.
6. **Unbounded Cypher results.** `--search Service` returns 42,757 rows. Acceptable on a CLI,
   fatal for a context window. Row caps and paging belong in the MCP projection.

## Ontology: host it, do not own it

RubyDex should supply declaration names, source locations, structural traversal, and positions.
Do not assume internal IDs survive schema changes or a rename. A specific domain model — Rails concepts, architectural layers,
business concepts — should attach as **annotations**: concept nodes plus edges keyed by fully
qualified name, not as new logic in the indexer.

Two implementations:

| | In the store | Sidecar overlay |
|---|---|---|
| Change | new redb table, format version bump | separate sqlite/jsonl keyed by FQN |
| Cost | format/version and cross-platform publishing considerations | separate persistence and join logic |
| Reindex needed | depends on how annotations are stored | no code reindex |
| Ontology churn | inside the indexer's correctness envelope | outside it |
| Multiple vocabularies | one | one per consumer |

Prefer the overlay. It keeps the indexer focused on Ruby semantics and lets a consumer evolve
its concept model without touching the store.

Static analysis under-approximates: `send`, `public_send`, `method_missing`, `define_method`
and Rails' dynamic surfaces will not appear as edges. The overlay is also where non-Ruby facts
(routes, `schema.rb`, docs) should enter, so the model is completed rather than inferred from
one source. Do not present a graph-only ontology as complete.

## Where it should live

The relevant axis is not "inside RubyDex or a separate gem" — a separate gem is still a Ruby
process at 67 MB plus a resolvable dependency, so it solves nothing. The axis is **Ruby process
versus Rust binary**:

- A Rust `rubydex-mcp` binary might approach 25–40 MB and ~60 ms startup, based on the bare
  CLI read path, but those are **estimates**, not measurements of an MCP server. It could avoid
  the Ruby/Bundler runtime and reuse the `rubydex-index-manager` packaging pattern.
- Prototyping in-tree is right, and is already the current state. Keep the MCP layer a thin
  projection of `Graph` and `Query` with no analysis logic of its own, so moving transport and
  tool dispatch into Rust later is mechanical and tool names and schemas stay byte-identical
  for existing agent configurations.

## Suggested next steps

1. Ship store-backed MCP by default, serialize refresh with reads, and expose `describe_schema`.
2. Expose exact-name method-call sites without claiming method target resolution.
3. Budget Cypher execution before adding `query_graph`; then cap output and paginate results.
4. Investigate method-target resolution before adding `CALLS`, `tests_for`, or ontology facts.
5. Revisit per-response freshness only when refresh failures can be distinguished from unchanged sessions; a store mtime does not certify that the current sources were indexed.

## Caveats

- Tool description quality and output token shape drive agent success more than engine speed.
  Expect to iterate on the MCP surface independently of core RubyDex work.
- The open `graph.rs:1503` `unresolve_reference` panic can abort an MCP session during a
  surgical refresh. Fix it before promising uninterrupted multi-agent availability; an MCP tool
  cannot catch a Rust panic across the FFI boundary.
