# frozen_string_literal: true

# valgrind: skip — index_workspace builds the store in a child

require "tmpdir"
require "fileutils"
require "test_helper"
require "timeout"
require "rubydex/mcp_server"

# A store that reports read failures must not keep answering queries with holes: the graph reports
# the failure, and the Ruby layer quarantines the store and indexes in memory instead.
#
# Detection is covered in Rust (tests/store_corruption.rs), which can address a specific table.
# What this file pins down is the Ruby contract: a non-zero `store_errors` must quarantine the store
# and degrade the session.
class DiskIndexCorruptStoreTest < Minitest::Test
  def setup
    super
    skip_unless_store_feature
    @tmp = Dir.mktmpdir
    @graphs = []
    ENV["RUBYDEX_DISK_INDEX"] = "1"
    ENV["RUBYDEX_REFRESH_TTL"] = "0" # tests edit out of band and expect the next refresh to see them
    ENV["RUBYDEX_CACHE_DIR"] = @tmp
    skip("ruby subprocess unavailable; index_workspace can't build a store here") unless Process.respond_to?(:spawn)
  end

  def teardown
    # Wait for any detached rebuild spawned during the test before removing `@tmp`, so the child
    # cannot race `remove_entry` (noisy stderr on POSIX, EACCES/ENOTEMPTY on Windows).
    @graphs&.each do |graph|
      graph.instance_variable_get(:@rebuild_process)&.join(30)
      graph.detach_store
    end
    ENV.delete("RUBYDEX_DISK_INDEX")
    ENV.delete("RUBYDEX_REFRESH_TTL")
    ENV.delete("RUBYDEX_CACHE_DIR")
    FileUtils.remove_entry(@tmp) if @tmp && File.exist?(@tmp)
    super
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
    File.write("#{cache}.hash", graph.send(:store_signature))
    File.write(
      "#{cache}.files",
      JSON.generate({ "lockfile" => graph.send(:lockfile_hash), "files" => graph.send(:source_scan) }),
    )
    @graphs << graph
    [graph, cache]
  end

  def test_reported_errors_quarantine_the_store_and_fall_back
    files = 5.times.map { |i| ["klass#{i}.rb", "class Klass#{i}; end\n"] }.to_h
    graph, cache = write_store(files)
    assert(File.exist?(cache), "baseline: a store was written")

    # Report the store as untrustworthy, as the Rust layer does after a failed read.
    graph.define_singleton_method(:store_errors) { 1 }
    graph.index_workspace

    assert(File.exist?("#{cache}.corrupt"), "the untrustworthy store must be quarantined")
    refute(File.exist?(cache), "the corrupt store must not stay on the active cache path")
    refute(File.exist?("#{cache}.hash"), "its freshness marker must go with it, so the next run rebuilds")
    refute(File.exist?("#{cache}.files"), "its manifest must go with it, so the next run does not adopt it")

    # The session must have fallen back to an in-memory index. `index_all` is a no-op while a store
    # is attached (static store-backed mode), so a file indexed after the fallback can only be found
    # if the store was really detached — asserting on a constant the store already had would pass
    # either way, with the quarantined store answering from its open handle.
    File.write(File.join(@tmp, "late.rb"), "class LateToAdd; end\n")
    graph.index_all([File.join(@tmp, "late.rb")])
    graph.resolve # what MCPServer#spawn_indexer does after index_workspace
    refute_nil(graph["LateToAdd"], "the fallback must index in memory, not keep serving the store")
    refute_nil(graph["Klass0"], "the session must keep working after quarantining the store")
  end

  def test_mcp_retries_a_read_that_detects_store_corruption
    [false, true].each do |raise_on_read|
      graph, cache = write_store("foo.rb" => "class Foo; end\n")
      graph.index_workspace
      read_failed = false
      graph.define_singleton_method(:search) do |query, &block|
        unless read_failed
          read_failed = true
          raise "damaged record" if raise_on_read

          next [].to_enum
        end
        super(query, &block)
      end
      graph.define_singleton_method(:store_errors) { read_failed ? 1 : 0 }
      server = Rubydex::MCPServer::Server.new(root_path: @tmp)
      server.define_singleton_method(:graph_or_error) { graph }

      response = server.handle(
        jsonrpc: "2.0",
        id: 1,
        method: "tools/call",
        params: { name: "search_declarations", arguments: { query: "Foo", match_mode: "exact" } },
      )
      assert(response.key?(:result), "a detected read failure must retry, not return a tool error")
      result = response.fetch(:result)
      payload = JSON.parse(result.fetch(:content).fetch(0).fetch(:text))

      assert_includes(payload.fetch("results").map { |entry| entry.fetch("name") }, "Foo")
      assert(File.exist?("#{cache}.corrupt"), "the failed store is quarantined before retrying")
    end
  end

  def test_a_healthy_store_is_read_without_quarantine
    graph, cache = write_store("foo.rb" => "class Foo; end\n")

    graph.index_workspace

    # The read is what makes store_errors a real count rather than a trivially zero one.
    refute_nil(graph["Foo"], "the attached store must answer")
    assert_equal(0, graph.store_errors, "reading a healthy store must report no failures")
    assert(File.exist?(cache), "a healthy store must stay on the cache path")
    refute(File.exist?("#{cache}.corrupt"), "a healthy store must not be quarantined")
  end

  # A store whose manifest sidecar is unreadable has no provenance (and no baseline for a surgical
  # refresh), so it must not answer queries: the session falls back to an in-memory index.
  def test_a_corrupt_manifest_detaches_the_store_before_the_in_memory_fallback
    graph, cache = write_store("foo.rb" => "class Foo; end\n")
    File.write(File.join(@tmp, "late.rb"), "class LateToAdd; end\n")
    File.write("#{cache}.hash", graph.send(:store_signature))
    File.write("#{cache}.files", "{not json")

    graph.index_workspace
    graph.resolve

    refute_nil(graph["LateToAdd"], "the fallback must index in memory, not no-op on the attached store")
    refute_nil(graph["Foo"], "the session must keep working after the failed manifest")
  end

  # The sidecar lives in a directory the workspace may commit, so it is data, never code. This payload
  # raises from `_load` the instant anything calls `Marshal.load` on it.
  class MarshalPayload
    class << self
      def _load(_payload)
        raise "Marshal.load executed the manifest sidecar"
      end
    end

    def _dump(_level)
      "payload"
    end
  end

  def test_a_marshal_payload_in_the_manifest_is_never_loaded
    graph, cache = write_store("foo.rb" => "class Foo; end\n")
    File.write("#{cache}.hash", graph.send(:store_signature))
    File.binwrite("#{cache}.files", Marshal.dump(MarshalPayload.new))

    graph.index_workspace
    graph.resolve

    refute_nil(graph["Foo"], "a sidecar this version cannot parse is absent, not executable")
  end

  # A sidecar only a build can rewrite, so "/run the rebuild" is what makes an older release's store
  # migrate. Without it, every boot on that workspace would fall back to memory again.
  def test_a_sidecar_from_an_older_release_is_rewritten_by_a_background_rebuild
    graph, cache = write_store("foo.rb" => "class Foo; end\n")
    File.write("#{cache}.hash", graph.send(:store_signature))
    File.binwrite("#{cache}.files", Marshal.dump({ "lockfile" => "no-lockfile", "files" => {} }))

    graph.index_workspace

    Timeout.timeout(120) do
      sleep(0.05) until (manifest = File.read("#{cache}.files")).start_with?("{")
      assert_equal(["files", "lockfile", "store", "workspace"], JSON.parse(manifest).keys.sort)
    end
    assert(File.exist?("#{cache}.hash"), "the rebuild republished the marker too")
  end

  def test_quarantine_removes_manifest_when_move_fails_and_next_boot_rebuilds
    graph, cache = write_store("foo.rb" => "class Foo; end\n")
    graph.index_workspace
    active_store = graph.send(:active_store_path)

    original_mv = FileUtils.method(:mv)
    FileUtils.define_singleton_method(:mv) { |*| raise Errno::EACCES, "open handle on Windows" }
    begin
      graph.define_singleton_method(:store_errors) { 1 }
      assert(graph.quarantine_if_untrustworthy(cache))
    ensure
      FileUtils.define_singleton_method(:mv, original_mv)
    end

    assert(File.exist?(active_store), "the open store stayed in place when mv failed")
    refute(File.exist?("#{cache}.hash"), "quarantine must remove the freshness marker")
    refute(File.exist?("#{cache}.files"), "quarantine must remove the manifest sidecar")

    # Next boot must not trust the remaining store without its manifest and must trigger a rebuild.
    rebuilt = Rubydex::Graph.configure_for_workspace(@tmp)
    @graphs << rebuilt
    rebuilt.index_workspace
    rebuilt.resolve
    refute_nil(rebuilt["Foo"])
    Timeout.timeout(120) { sleep(0.05) until rebuilt.send(:store_fresh?, cache) }
    assert(File.exist?("#{cache}.files"), "rebuild republished the manifest")
  end

  # The marker is dropped by quarantine while a rebuild is in flight, so the source state has moved
  # too: a signature-equal refresh returns before this branch and never exercises it.
  def test_refresh_if_stale_does_not_take_surgical_path_without_marker
    graph, cache = write_store("foo.rb" => "class Foo; end\n")
    graph.index_workspace
    FileUtils.rm_f("#{cache}.hash")
    File.write(File.join(@tmp, "foo.rb"), "class Foo\n  def m; end\nend\n")

    surgical_called = false
    graph.define_singleton_method(:surgical_changes) do |*args|
      surgical_called = true
      super(*args)
    end
    rebuilds = 0
    graph.define_singleton_method(:launch_rebuild) do |_c, lock|
      rebuilds += 1
      @rebuild_process = Process.detach(Process.spawn(Gem.ruby, "-e", "exit 0"))
      Dir.rmdir(lock)
    end

    refute(graph.refresh_if_stale)
    refute(surgical_called, "missing marker must require a rebuild rather than trusting surgical diff")
    assert_equal(1, rebuilds)
  end
end
