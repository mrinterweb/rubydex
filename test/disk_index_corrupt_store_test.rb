# frozen_string_literal: true

require "tmpdir"
require "fileutils"
require "test_helper"

# A store that reports read failures must not keep answering queries with holes: the graph reports
# the failure, and the Ruby layer quarantines the store and indexes in memory instead.
#
# Detection is covered in Rust (tests/store_corruption.rs), which can address a specific table.
# What this file pins down is the Ruby contract: a non-zero `store_errors` must quarantine the store
# and degrade the session.
class DiskIndexCorruptStoreTest < Minitest::Test
  def setup
    super
    @tmp = Dir.mktmpdir
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

  def test_store_errors_is_zero_on_a_healthy_store
    graph = Rubydex::Graph.configure_for_workspace(@tmp)
    graph.index_workspace

    assert_equal 0, graph.store_errors, "a freshly built store must report no read failures"
  end

  # Writes a store without attaching it. redb holds an exclusive lock per file, so the graph that
  # builds a store must not be the graph that reads it — that is how a real session works anyway.
  def write_store(files)
    files.each { |name, source| File.write(File.join(@tmp, name), source) }

    memory = Rubydex::Graph.new
    memory.index_all(files.keys.map { |name| File.join(@tmp, name) })
    memory.resolve

    graph = Rubydex::Graph.configure_for_workspace(@tmp)
    cache = graph.send(:store_cache_path)
    # build_store writes the file but does not create the cache directory; index_workspace does.
    FileUtils.mkdir_p(File.dirname(cache))
    memory.build_store(cache)
    [graph, cache]
  end

  def test_reported_errors_quarantine_the_store_and_fall_back
    files = 5.times.map { |i| ["klass#{i}.rb", "class Klass#{i}; end\n"] }.to_h
    graph, cache = write_store(files)
    assert File.exist?(cache), "baseline: a store was written"

    # Report the store as untrustworthy, as the Rust layer does after a failed read.
    graph.define_singleton_method(:store_errors) { 1 }
    graph.index_workspace

    assert File.exist?("#{cache}.corrupt"), "the untrustworthy store must be quarantined"
    refute File.exist?(cache), "the corrupt store must not stay on the active cache path"
    refute File.exist?("#{cache}.hash"), "its freshness marker must go with it, so the next run rebuilds"

    # The session still answers, from the in-memory index it fell back to.
    refute_nil graph["Klass0"], "the session must keep working after quarantining the store"
  end

  def test_a_healthy_store_is_not_quarantined
    graph, cache = write_store("foo.rb" => "class Foo; end\n")

    graph.index_workspace

    assert File.exist?(cache), "a healthy store must stay on the cache path"
    refute File.exist?("#{cache}.corrupt"), "a healthy store must not be quarantined"
  end
end
