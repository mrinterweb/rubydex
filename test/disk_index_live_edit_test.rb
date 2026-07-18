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

    # Live edit: change the method name. Store documents are keyed by `file://` URIs (from
    # index_workspace), so the edit must use the same URI to replace the store-backed document.
    graph.index_source("file://#{rb}", "class Foo\n  def baz; end\nend\n", "ruby")
    graph.resolve

    # The overlay document shadows the store's, and resolution surfaces the edited members.
    foo = graph["Foo"]
    refute_nil(foo, "graph[\"Foo\"] still readable after live edit")
    member_names = foo.members.map(&:name)
    assert_includes(member_names, "Foo#baz()")
    refute_includes(member_names, "Foo#bar()")
  end
end
