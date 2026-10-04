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
    # `index_workspace` builds the store in a short-lived child process, which requires spawning a
    # Ruby process with rubydex loadable. Without it, `index_workspace` silently falls back to the
    # in-memory path (covered by other tests) — there's nothing store-backed to verify here.
    skip("ruby subprocess unavailable; index_workspace can't build a store here") unless Process.respond_to?(:spawn)

    rb = File.join(@tmp, "foo.rb")
    File.write(rb, "class Foo\n  def bar; end\nend\n")

    graph = Rubydex::Graph.configure_for_workspace(@tmp)
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
    ws = File.join(@tmp, "ws")
    FileUtils.mkdir_p(ws)
    File.write(File.join(ws, "a.rb"), "class A; end\n")

    graph = Rubydex::Graph.configure_for_workspace(ws)
    cache_dir = File.join(@tmp, "cache", "key")
    cache = File.join(cache_dir, "index.redb")

    # The builder runs in a fresh process, so the failure has to be simulated at the process
    # boundary: `false` exits non-zero without writing anything, like a builder that dies.
    original_spawn = Process.method(:spawn)
    Process.define_singleton_method(:spawn) { |*| original_spawn.call("false") }

    assert_raises(RuntimeError) { graph.send(:build_store_via_fork, cache) }
    assert_empty(
      Dir.glob(File.join(cache_dir, "*.building")),
      "temp files leaked after a failed store build",
    )
  ensure
    Process.define_singleton_method(:spawn, original_spawn) if original_spawn
  end
end
