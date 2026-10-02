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

  def test_failed_store_build_cleans_up_temp_files
    skip("fork unavailable; build_store_via_fork can't run on this platform") unless Process.respond_to?(:fork)

    ws = File.join(@tmp, "ws")
    FileUtils.mkdir_p(ws)
    File.write(File.join(ws, "a.rb"), "class A; end\n")

    graph = Rubydex::Graph.new(workspace_path: ws)
    cache_dir = File.join(@tmp, "cache", "key")
    cache = File.join(cache_dir, "index.redb")
    original_build_store = nil

    # `build_store` is a C method; `define_method` replaces its table entry, so capture the
    # original and rebind it afterwards (remove_method would delete the C method for good).
    original_build_store = Rubydex::Graph.instance_method(:build_store)
    Rubydex::Graph.send(:define_method, :build_store) do |path|
      File.binwrite(path, "partial")
      exit!(1) # simulate a child that dies after writing a partial store
    end

    assert_raises(RuntimeError) { graph.send(:build_store_via_fork, cache) }
    assert_empty(
      Dir.glob(File.join(cache_dir, "*.building")),
      "temp files leaked after a failed store build",
    )
  ensure
    Rubydex::Graph.send(:define_method, :build_store, original_build_store) if original_build_store
  end
end
