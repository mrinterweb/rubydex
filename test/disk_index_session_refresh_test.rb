# frozen_string_literal: true

require "test_helper"
require "tmpdir"
require "fileutils"

# Three sessions (two editors + an agent server) share one store per directory. A branch switch
# is a filesystem event no session owns, so the freshness marker is what notices: the first
# session to claim the rebuild lock spawns the one rebuild child, and every session re-attaches
# (redb pins a snapshot at open, so a session must reopen to see a new store).
class DiskIndexSessionRefreshTest < Minitest::Test
  def with_workspace
    Dir.mktmpdir do |dir|
      FileUtils.mkdir_p(File.join(dir, "tmp"))
      rb = File.join(dir, "foo.rb")
      File.write(rb, "class Foo\n  def bar; end\nend\n")
      ENV["RUBYDEX_DISK_INDEX"] = "1"
      begin
        yield(dir, rb)
      ensure
        ENV.delete("RUBYDEX_DISK_INDEX")
      end
    end
  end

  def session(dir)
    graph = Rubydex::Graph.configure_for_workspace(dir)
    graph.index_workspace
    graph.resolve
    graph
  end

  # `prepare_for_tool_call` is the whole pre-tool sequence, and it resolves exactly once itself —
  # that is what lets the caller skip its own resolve, which used to walk and sort every document
  # URI in the store on top of the refresh.
  def test_a_tool_call_refresh_leaves_the_graph_resolved
    with_workspace do |dir, rb|
      # Enough files that editing one stays inside REBUILD_DIFF_RATIO and takes the surgical branch
      # (with a single-file workspace any change is a rebuild, which resolves zero times).
      20.times { |i| File.write(File.join(dir, "k#{i}.rb"), "class K#{i}\nend\n") }
      graph = session(dir)
      resolves = 0
      graph.define_singleton_method(:resolve) do
        resolves += 1
        super()
      end

      File.write(rb, "class Foo\n  def quux; end\nend\n")
      assert(graph.prepare_for_tool_call)

      assert_equal(1, resolves, "the refresh resolves once, internally")
      assert_includes(graph["Foo"].members.map(&:name), "Foo#quux()", "and the graph answers")
    end
  end

  def test_a_session_reopens_when_the_store_is_rebuilt_elsewhere
    with_workspace do |dir, rb|
      first = session(dir)
      assert_includes(first["Foo"].members.map(&:name), "Foo#bar()")

      # The branch switch: content changes on disk, no session is told about it.
      File.write(rb, "class Foo\n  def baz; end\nend\n")
      second = session(dir) # a different session notices first and rebuilds
      assert_includes(second["Foo"].members.map(&:name), "Foo#baz()")

      assert(first.send(:refresh_if_stale), "the first session must be able to move to the new snapshot")
      first.resolve
      members = first["Foo"].members.map(&:name)
      assert_includes(members, "Foo#baz()")
      refute_includes(members, "Foo#bar()", "the reopened session must not still answer from the old snapshot")
    end
  end

  def test_refresh_does_nothing_when_the_store_is_already_fresh
    with_workspace do |dir, _rb|
      graph = session(dir)
      refute(graph.send(:refresh_if_stale), "nothing changed: no rebuild, no reopen")
      assert_includes(graph["Foo"].members.map(&:name), "Foo#bar()")
    end
  end

  def test_a_session_can_reopen_without_being_the_builder
    with_workspace do |dir, rb|
      first = session(dir)
      File.write(rb, "class Foo\n  def qux; end\nend\n")
      builder = session(dir) # claims the lock and rebuilds
      assert_includes(builder["Foo"].members.map(&:name), "Foo#qux()")

      # The second stale session finds the marker fresh and just re-attaches.
      assert(first.send(:refresh_if_stale))
      first.resolve
      assert_includes(first["Foo"].members.map(&:name), "Foo#qux()")
    end
  end

  def test_only_one_session_claims_the_rebuild
    with_workspace do |dir, _rb|
      graph = session(dir)
      cache = graph.send(:store_cache_path)

      assert_kind_of(String, graph.send(:claim_rebuild, cache), "the first session gets the lock")
      refute(graph.send(:claim_rebuild, cache), "the second session must not spawn a second rebuild")
    end
  end

  def test_a_stale_lock_does_not_block_a_rebuild
    with_workspace do |dir, _rb|
      graph = session(dir)
      cache = graph.send(:store_cache_path)
      lock = graph.send(:claim_rebuild, cache)
      # A builder that crashed leaves the lock behind; it must expire, not wedge the directory.
      File.utime(Time.now - 3600, Time.now - 3600, lock)

      assert_kind_of(String, graph.send(:claim_rebuild, cache), "an expired lock is reclaimed")
    end
  end
end
