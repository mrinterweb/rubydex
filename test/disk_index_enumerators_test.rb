# frozen_string_literal: true

# valgrind: skip — index_workspace builds the store in a child

require "test_helper"
require "tmpdir"
require "fileutils"
require "json"
require "rubydex/mcp_server"

# Store-backed graphs keep the bulk index off-heap. Enumeration APIs and the MCP tools built on
# them must still see it; a session that only walked the in-memory overlay reported zero files.
class DiskIndexEnumeratorsTest < Minitest::Test
  def setup
    super
    @tmp = Dir.mktmpdir
    @workspace = File.join(@tmp, "workspace")
    FileUtils.mkdir_p(@workspace)
    File.write(File.join(@workspace, "foo.rb"), "class Foo\n  CONST = Bar\n\n  def bar\n    Baz.new\n  end\nend\n")
    File.write(File.join(@workspace, "other.rb"), "class Bar\nend\n\nclass Baz\nend\n")
    ENV["RUBYDEX_DISK_INDEX"] = "1"
    ENV["RUBYDEX_CACHE_DIR"] = @tmp
    skip("ruby subprocess unavailable; index_workspace can't build a store here") unless Process.respond_to?(:spawn)
  end

  def teardown
    ENV.delete("RUBYDEX_DISK_INDEX")
    ENV.delete("RUBYDEX_CACHE_DIR")
    FileUtils.remove_entry(@tmp) if @tmp && File.exist?(@tmp)
    super
  end

  def build_memory_graph
    graph = Rubydex::Graph.configure_for_workspace(@workspace)
    graph.index_all(graph.workspace_paths)
    graph.resolve
    graph
  end

  def test_enumerators_include_store_backed_nodes
    disk = Rubydex::Graph.configure_for_workspace(@workspace)
    disk.index_workspace
    disk.resolve
    memory = build_memory_graph

    assert_equal(memory.documents.count, disk.documents.count, "documents")
    assert_equal(memory.declarations.count, disk.declarations.count, "declarations")
    assert_equal(memory.constant_references.count, disk.constant_references.count, "constant references")
    assert_equal(memory.method_references.count, disk.method_references.count, "method references")
    assert_equal(
      memory.dead_code_candidates.map(&:name).sort,
      disk.dead_code_candidates.map(&:name).sort,
      "dead code candidates",
    )
  end

  # `codebase_stats` needs lengths, not payloads: the enumerator size paths count store ids instead
  # of decoding every definition, constant reference and method reference to count them.
  def test_enumerator_sizes_match_the_enumerated_counts
    disk = Rubydex::Graph.configure_for_workspace(@workspace)
    disk.index_workspace
    disk.resolve
    memory = build_memory_graph

    assert_equal(memory.constant_references.count, disk.constant_references.size, "constant references")
    assert_equal(memory.method_references.count, disk.method_references.size, "method references")

    store_document = disk.documents.find { |document| document.uri.end_with?("foo.rb") }
    overlay_document = memory.documents.find { |document| document.uri.end_with?("foo.rb") }
    refute_nil(store_document)
    assert_equal(overlay_document.definitions.count, store_document.definitions.size, "definitions")
  ensure
    detach_if_store_backed(disk)
  end

  def test_get_file_declarations_finds_a_store_backed_document
    disk = Rubydex::Graph.configure_for_workspace(@workspace)
    disk.index_workspace
    disk.resolve

    tool = Rubydex::MCPServer::GetFileDeclarationsTool.new(disk)
    payload = JSON.parse(tool.call(file_path: "foo.rb").to_h.fetch(:content).first.fetch(:text))

    assert_equal("foo.rb", payload.fetch("file"))
    assert_includes(payload.fetch("declarations").map { |entry| entry.fetch("name") }, "Foo")
  ensure
    detach_if_store_backed(disk)
  end

  def test_method_reference_cached_names_match_store_and_invalidate_on_delete
    disk = Rubydex::Graph.configure_for_workspace(@workspace)
    disk.index_workspace
    disk.resolve

    # `Baz.new` is the only `new` call in the workspace. The URI comes from the reference itself
    # because macOS tmpdirs live behind the `/var` -> `/private/var` symlink and the store keys
    # documents by their real path, so `path_to_uri(@workspace/...)` need not match.
    ref = disk.method_references.find { |reference| reference.name == "new" }
    refute_nil(ref, "store-backed method reference must expose its cached name")
    assert_equal("new", ref.name, "the cached name must survive a second read")

    disk.delete_document(ref.document.uri)
    error = assert_raises(RuntimeError) { ref.name }
    assert_equal("Method reference must exist for a valid id", error.message)
  ensure
    detach_if_store_backed(disk)
  end

  private

  # A build without the redb-store feature answers `detach_store` with a RuntimeError rather than
  # no-op, and these tests run in that build too (they cover the in-memory fallback).
  def detach_if_store_backed(graph)
    graph&.detach_store if Rubydex::Graph.store_format_version.positive?
  end
end
