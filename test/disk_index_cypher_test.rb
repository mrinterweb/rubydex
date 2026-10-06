# frozen_string_literal: true

require "tmpdir"
require "fileutils"
require "test_helper"

class DiskIndexCypherTest < Minitest::Test
  def setup
    super
    @tmp = Dir.mktmpdir
    ENV["RUBYDEX_DISK_INDEX"] = "1"
    ENV["RUBYDEX_CACHE_DIR"] = @tmp
  end

  def teardown
    ENV.delete("RUBYDEX_DISK_INDEX")
    ENV.delete("RUBYDEX_CACHE_DIR")
    FileUtils.remove_entry(@tmp) if @tmp && File.exist?(@tmp)
    super
  end

  def test_cypher_node_cells_resolve_store_backed_declarations
    # `index_workspace` builds the store in a short-lived child process; without spawn there is
    # no store-backed graph to exercise (the in-memory path is covered in graph_test.rb).
    skip("ruby subprocess unavailable; index_workspace can't build a store here") unless Process.respond_to?(:spawn)

    File.write(File.join(@tmp, "foo.rb"), "class Foo\nend\n")

    graph = Rubydex::Graph.configure_for_workspace(@tmp)
    graph.index_workspace
    refute_nil(graph["Foo"], "store-backed reads work")

    rows = Rubydex::Query.parse("MATCH (c:Class {name: 'Foo'}) RETURN c").run(graph).rows
    assert_equal(1, rows.length, "the node cell must resolve, not be dropped as stale")
    node = rows.first["c"]
    assert_kind_of(Rubydex::Declaration, node)
    assert_equal("Foo", node.name)
  end
end
