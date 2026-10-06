# frozen_string_literal: true

require "tmpdir"
require "fileutils"
require "test_helper"

# A store-backed session moves to a new source state by re-indexing only the files that changed,
# instead of rebuilding the whole store. A diff too large (or a lockfile change, or no manifest)
# still falls back to the rebuild, and the freshness signature must stay byte-identical so stores
# written by older releases keep their markers.
class DiskIndexSurgicalRefreshTest < Minitest::Test
  def setup
    super
    @tmp = Dir.mktmpdir
    @ws = File.join(@tmp, "ws")
    FileUtils.mkdir_p(@ws)
    ENV["RUBYDEX_DISK_INDEX"] = "1"
    ENV["RUBYDEX_CACHE_DIR"] = File.join(@tmp, "cache")
    20.times { |i| write("c#{i}.rb", "class C#{i}\n  def m#{i}; end\nend\n") }
    write("parent.rb", "class Parent\nend\n")
    write("child.rb", "class Child < Parent\nend\n")
  end

  def teardown
    ENV.delete("RUBYDEX_DISK_INDEX")
    ENV.delete("RUBYDEX_CACHE_DIR")
    FileUtils.remove_entry(@tmp) if @tmp && File.exist?(@tmp)
    super
  end

  def test_small_diff_is_applied_without_a_rebuild
    graph = booted_graph
    write("c0.rb", "class C0\n  def renamed; end\nend\n")
    write("added.rb", "class Added\nend\n")
    File.delete(File.join(@ws, "c1.rb"))

    refute_rebuild(graph) { assert(graph.refresh_if_stale) }
    assert_includes(graph["C0"].members.map(&:name), "C0#renamed()")
    refute_nil(graph["Added"])
    assert_nil(graph["C1"])
  end

  def test_second_refresh_with_no_new_changes_is_a_noop
    graph = booted_graph
    write("c0.rb", "class C0\nend\n")
    assert(graph.refresh_if_stale)
    refute(graph.refresh_if_stale)
  end

  def test_superclass_change_is_reflected_in_ancestors
    graph = booted_graph
    write("child.rb", "class Child\nend\n")
    refute_rebuild(graph) { graph.refresh_if_stale }
    refute_includes(graph["Child"].ancestors.map(&:name), "Parent")
  end

  def test_large_diff_falls_back_to_a_rebuild
    graph = booted_graph
    12.times { |i| write("c#{i}.rb", "class C#{i}\n  def big#{i}; end\nend\n") } # 12/22 > 0.25
    rebuilt = false
    graph.define_singleton_method(:build_store_via_fork) do |*args|
      rebuilt = true
      super(*args)
    end
    assert(graph.refresh_if_stale)
    assert(rebuilt, "a diff over REBUILD_DIFF_RATIO must rebuild")
  end

  def test_lockfile_change_falls_back_to_a_rebuild
    graph = booted_graph
    write("Gemfile.lock", "GEM\n  specs:\n")
    rebuilt = false
    graph.define_singleton_method(:build_store_via_fork) do |*args|
      rebuilt = true
      super(*args)
    end
    graph.refresh_if_stale
    assert(rebuilt, "a lockfile change must rebuild (the gem closure changed)")
  end

  def test_store_signature_is_unchanged_by_the_scan_refactor
    graph = Rubydex::Graph.configure_for_workspace(@ws)
    assert_equal(graph.send(:store_signature), graph.send(:store_signature, graph.send(:source_scan)))
  end

  def test_reindexed_paths_match_the_rust_uri_form
    graph = booted_graph
    write("with space.rb", "class WithSpace\nend\n")
    refute_rebuild(graph) { graph.refresh_if_stale }
    refute_nil(graph["WithSpace"], "a path with a space must re-index, not silently miss the URI")
  end

  private

  def write(rel, src)
    path = File.join(@ws, rel)
    File.write(path, src)
    # mtime has 1 s resolution in the signature; force a visible change.
    t = Time.now + rand(10..1000)
    File.utime(t, t, path)
  end

  def booted_graph
    skip("ruby subprocess unavailable") unless Process.respond_to?(:spawn)
    graph = Rubydex::Graph.configure_for_workspace(@ws)
    graph.index_workspace
    graph
  end

  def refute_rebuild(graph, &block)
    graph.define_singleton_method(:build_store_via_fork) { |*| raise "unexpected rebuild" }
    block.call
  end
end
