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
    ENV["RUBYDEX_REFRESH_TTL"] = "0" # tests edit out of band and expect the next refresh to see them
    ENV["RUBYDEX_CACHE_DIR"] = File.join(@tmp, "cache")
    20.times { |i| write("c#{i}.rb", "class C#{i}\n  def m#{i}; end\nend\n") }
    write("parent.rb", "class Parent\nend\n")
    write("child.rb", "class Child < Parent\nend\n")
  end

  def teardown
    ENV.delete("RUBYDEX_DISK_INDEX")
    ENV.delete("RUBYDEX_REFRESH_TTL")
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
    graph.define_singleton_method(:build_store_in_child) do |*args|
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
    graph.define_singleton_method(:build_store_in_child) do |*args|
      rebuilt = true
      super(*args)
    end
    graph.refresh_if_stale
    assert(rebuilt, "a lockfile change must rebuild (the gem closure changed)")
  end

  # Each round edits two files — well under REBUILD_DIFF_RATIO — but a surgical apply leaves them in
  # the overlay and only a rebuild compacts it. Without a ceiling, a session that absorbs many small
  # branch switches converges on holding the whole index in memory (~12 MB per 1,000 files).
  def test_enough_surgical_edits_trigger_a_rebuild
    graph = booted_graph
    rebuilt = false
    graph.define_singleton_method(:build_store_in_child) do |*args|
      rebuilt = true
      super(*args)
    end

    rounds = 0
    until rebuilt
      rounds += 1
      refute(rounds > 11, "no rebuild in #{rounds} rounds: the overlay ceiling never fires")

      write("c#{rounds * 2}.rb", "class C#{rounds * 2}\n  def r#{rounds}; end\nend\n")
      write("c#{rounds * 2 + 1}.rb", "class C#{rounds * 2 + 1}\n  def r#{rounds}; end\nend\n")
      graph.refresh_if_stale
    end

    assert_equal(3, rounds, "25% of 22 files is 5, so the round that applies the 6th file rebuilds")
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

  # The stamp a freshness check records used to be whole-second mtime + size, so an edit that
  # landed in the same second and did not change the byte count was invisible forever: the session
  # kept answering with the deleted constant and had no way to learn otherwise. Agents rewriting a
  # file with sed and then querying immediately hit exactly this.
  def test_a_same_size_edit_inside_the_same_second_is_noticed
    graph = booted_graph
    path = File.join(@ws, "c0.rb")
    second = Time.now.to_i + 1_000
    File.utime(Time.at(second + 0.1), Time.at(second + 0.1), path)
    graph.refresh_if_stale

    before = File.size(path)
    source = "class C0Renamed\nend\n"
    File.write(path, source + ("#" * (before - source.bytesize)))
    assert_equal(before, File.size(path), "the edit must not change the file size")
    File.utime(Time.at(second + 0.2), Time.at(second + 0.2), path)

    assert(graph.refresh_if_stale, "a same-size edit inside the same second must be noticed")
    refute_nil(graph["C0Renamed"])
    assert_nil(graph["C0"])
  end

  private

  def write(rel, src)
    path = File.join(@ws, rel)
    File.write(path, src)
  end

  def booted_graph
    skip("ruby subprocess unavailable") unless Process.respond_to?(:spawn)
    graph = Rubydex::Graph.configure_for_workspace(@ws)
    graph.index_workspace
    graph
  end

  def refute_rebuild(graph, &block)
    graph.define_singleton_method(:build_store_in_child) { |*| raise "unexpected rebuild" }
    block.call
  end
end
