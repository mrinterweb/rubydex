# frozen_string_literal: true

require "tmpdir"
require "fileutils"
require "test_helper"

class DiskIndexLiveEditTest < Minitest::Test
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

  def test_index_source_applies_to_store_backed_graph
    # `index_workspace` builds the store via `build_store_via_fork`, which requires
    # `Process#fork` (unavailable on Windows). Without it, `index_workspace` silently falls back
    # to the in-memory path, which is covered by other tests — there's nothing store-backed to
    # verify here. Mirrors the same check `build_store_via_fork` itself makes.
    skip("fork unavailable; index_workspace can't build a store on this platform") unless Process.respond_to?(:fork)

    rb = File.join(@tmp, "foo.rb")
    File.write(rb, "class Foo\n  def bar; end\nend\n")

    graph = Rubydex::Graph.new(workspace_path: @tmp)
    graph.index_workspace # builds + attaches the store

    # The store-backed graph can still read declarations from disk.
    refute_nil(graph["Foo"], "graph[\"Foo\"] should read from the store")

    # Live edit: change the method name. Before this task, rdx_index_source short-circuited
    # and silently dropped the edit. Now it must flow through to the overlay without panicking.
    graph.index_source(rb, "class Foo\n  def baz; end\nend\n", "ruby")

    # The layered accessor still reads "Foo" from the store (resolution of the new "baz"
    # member requires Graph#resolve, which is a separate concern).
    refute_nil(graph["Foo"], "graph[\"Foo\"] still readable after live edit")
  end
end
