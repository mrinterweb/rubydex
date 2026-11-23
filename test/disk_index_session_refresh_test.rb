# frozen_string_literal: true

# valgrind: skip — spawns git and store builds

require "test_helper"
require "tmpdir"
require "fileutils"
require "timeout"

# Three sessions (two editors + an agent server) share one store per directory. A branch switch
# is a filesystem event no session owns, so the freshness marker is what notices: the first
# session to claim the rebuild lock spawns the one rebuild child, and every session re-attaches
# (redb pins a snapshot at open, so a session must reopen to see a new store).
class DiskIndexSessionRefreshTest < Minitest::Test
  def setup
    super
    skip_unless_store_feature
    @sessions = []
  end

  def with_workspace
    Dir.mktmpdir do |dir|
      FileUtils.mkdir_p(File.join(dir, "tmp"))
      rb = File.join(dir, "foo.rb")
      File.write(rb, "class Foo\n  def bar; end\nend\n")
      ENV["RUBYDEX_DISK_INDEX"] = "1"
      # Tests edit files out of band and expect the next refresh to see them; the TTL exists to
      # amortize real tool calls.
      ENV["RUBYDEX_REFRESH_TTL"] = "0"
      begin
        yield(dir, rb)
      ensure
        join_rebuilds
        # Windows cannot unlink a file this process still holds open, and an opted-in session keeps
        # its registry file locked until released, so the tmpdir would fail to clean up.
        Rubydex::IndexManager.release_all
        ENV.delete("RUBYDEX_DISK_INDEX")
        ENV.delete("RUBYDEX_REFRESH_TTL")
      end
    end
  end

  def session(dir)
    graph = Rubydex::Graph.configure_for_workspace(dir)
    @sessions << graph
    graph.index_workspace
    graph.resolve
    graph
  end

  def join_rebuilds(*extra)
    (@sessions + extra).each do |graph|
      graph.instance_variable_get(:@rebuild_process)&.join(30)
      graph.detach_store
    end
    @sessions.clear
  end

  def git(dir, *args)
    # A developer's git config may sign commits (and a signing agent may not answer in CI).
    args = ["-c", "commit.gpgsign=false", *args] if args.first == "commit"
    # `init.defaultBranch` is a developer setting; a runner without it still creates `master`.
    args = ["-c", "init.defaultBranch=main", *args] if args.first == "init"
    identity = {
      "GIT_AUTHOR_NAME" => "test",
      "GIT_AUTHOR_EMAIL" => "test@example.com",
      "GIT_COMMITTER_NAME" => "test",
      "GIT_COMMITTER_EMAIL" => "test@example.com",
    }
    Dir.chdir(dir) do
      system(identity, "git", *args, out: File::NULL, err: File::NULL) || raise("git #{args.join(" ")} failed")
    end
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

  # The freshness walk is the expensive signal, so it is only redone every REFRESH_TTL seconds. A
  # commit or branch switch is the change that must not wait for it: HEAD moves for free.
  def test_the_freshness_walk_is_reused_inside_its_ttl
    with_workspace do |dir, _rb|
      graph = session(dir)
      walks = 0
      graph.define_singleton_method(:walk_sources) do
        walks += 1
        super()
      end

      ENV["RUBYDEX_REFRESH_TTL"] = "60" # the suite runs with the window closed
      graph.refresh_if_stale
      graph.refresh_if_stale
      assert_equal(0, walks, "inside the window the boot walk is reused")

      ENV["RUBYDEX_REFRESH_TTL"] = "0"
      graph.refresh_if_stale
      assert_equal(1, walks, "once the window passes the walk runs again")
    end
  end

  # A branch switch rewrites files in bulk, which is exactly what the walk measures — so the walk is
  # forced, and the switch is visible even a millisecond after the boot walk.
  def test_a_branch_switch_is_noticed_inside_the_walk_ttl
    Dir.mktmpdir do |dir|
      FileUtils.mkdir_p(File.join(dir, "tmp"))
      rb = File.join(dir, "foo.rb")
      File.write(rb, "class Foo\n  def bar; end\nend\n")
      git(dir, "init", "-q")
      git(dir, "add", "foo.rb")
      git(dir, "commit", "-q", "-m", "main")
      git(dir, "checkout", "-q", "-b", "other")
      File.write(rb, "class Foo\n  def baz; end\nend\n")
      git(dir, "add", "foo.rb")
      git(dir, "commit", "-q", "-m", "other")
      git(dir, "checkout", "-q", "main")

      ENV["RUBYDEX_DISK_INDEX"] = "1"
      ENV["RUBYDEX_REFRESH_TTL"] = "60" # the walk must stay suppressed: only HEAD may notice
      begin
        graph = Rubydex::Graph.configure_for_workspace(dir)
        graph.index_workspace
        graph.resolve
        assert_includes(graph["Foo"].members.map(&:name), "Foo#bar()")

        git(dir, "checkout", "-q", "other")
        moved = graph.refresh_if_stale
        refute(moved, "the old snapshot is available during a branch-switch rebuild")
        Timeout.timeout(45) { sleep(0.02) until graph.send(:store_fresh?, graph.send(:store_cache_path)) }
        graph.refresh_if_stale
        members = graph["Foo"].members.map(&:name)
        assert_includes(members, "Foo#baz()")
        refute_includes(members, "Foo#bar()", "must not still answer from the previous branch")
      ensure
        join_rebuilds(graph) if graph
        ENV.delete("RUBYDEX_DISK_INDEX")
      end
    end
  end

  # A commit rewrites no files, so the store still describes the workspace exactly: HEAD belongs to
  # what a session is serving, not to what the shared store was built from, or every commit would
  # throw the store away (a 24.6 s rebuild on large codebase).
  def test_a_commit_that_changes_no_files_leaves_the_store_fresh
    Dir.mktmpdir do |dir|
      FileUtils.mkdir_p(File.join(dir, "tmp"))
      File.write(File.join(dir, "foo.rb"), "class Foo\n  def bar; end\nend\n")
      git(dir, "init", "-q")
      git(dir, "add", "foo.rb")
      git(dir, "commit", "-q", "-m", "first")

      ENV["RUBYDEX_DISK_INDEX"] = "1"
      begin
        graph = Rubydex::Graph.configure_for_workspace(dir)
        graph.index_workspace
        cache = graph.send(:store_cache_path)

        git(dir, "commit", "-q", "--allow-empty", "-m", "nothing changed")
        assert(graph.send(:store_fresh?, cache), "the store still describes the workspace")
        assert(graph.refresh_if_stale, "but the session knows it moved onto a new commit")
      ensure
        ENV.delete("RUBYDEX_DISK_INDEX")
      end
    end
  end

  # A manager that cannot start must cost the pre-warm it provides and nothing else. Letting that
  # failure reach `index_workspace`'s rescue threw away the store it had just built and re-indexed the
  # workspace in memory — the footprint the store exists to avoid, caused by the optional part.
  def test_a_manager_that_cannot_start_does_not_disable_the_disk_index
    with_workspace do |dir, _rb|
      ENV["RUBYDEX_INDEX_MANAGER"] = "1"
      ENV["XDG_CACHE_HOME"] = dir # keep the registry inside the tmpdir
      graph = Rubydex::Graph.configure_for_workspace(dir)
      fell_back = false
      graph.define_singleton_method(:index_all) do |*args|
        fell_back = true
        super(*args)
      end
      original = Rubydex::IndexManager.singleton_class.instance_method(:manager_binary)
      Rubydex::IndexManager.singleton_class.define_method(:manager_binary) { File.join(dir, "no-such-manager") }
      begin
        graph.index_workspace

        refute(fell_back, "a manager that cannot start must not fall back to the in-memory index")
        assert_includes(graph["Foo"].members.map(&:name), "Foo#bar()", "the store-backed graph still answers")
      ensure
        Rubydex::IndexManager.singleton_class.define_method(:manager_binary, original)
        ENV.delete("RUBYDEX_INDEX_MANAGER")
        ENV.delete("XDG_CACHE_HOME")
      end
    end
  end

  # Outside a repository `git rev-parse HEAD` prints "fatal: not a git repository", and `capture2`
  # only captures stdout, so that landed in the session's stderr once per freshness check.
  def test_a_workspace_outside_git_does_not_leak_git_noise
    Dir.mktmpdir do |dir|
      graph = Rubydex::Graph.configure_for_workspace(dir)
      noise = File.join(dir, "stderr")
      original = $stderr.dup
      File.open(noise, "w") { |log| $stderr.reopen(log) }
      begin
        assert_nil(graph.send(:workspace_head), "a non-repository has no commit")
      ensure
        $stderr.reopen(original)
      end

      assert_empty(File.read(noise), "git's stderr must not reach the session's own stderr")
    end
  end

  # A session that was not the builder must re-attach to the store the builder just published rather
  # than rebuild it again, and must stop answering from the snapshot it had pinned: redb pins one at
  # open, so a session only sees a new store by reopening it. The marker is keyed on what the store
  # holds (source scan + lockfile), not on HEAD: HEAD belongs to what the session is serving, and
  # mixing it into the marker made this branch dead in a git repo.
  def test_a_session_moves_to_the_store_another_session_rebuilt
    Dir.mktmpdir do |dir|
      FileUtils.mkdir_p(File.join(dir, "tmp"))
      8.times { |i| File.write(File.join(dir, "k#{i}.rb"), "class K#{i}\n  def old; end\nend\n") }
      git(dir, "init", "-q")
      git(dir, "add", ".")
      git(dir, "commit", "-q", "-m", "main")

      ENV["RUBYDEX_DISK_INDEX"] = "1"
      ENV["RUBYDEX_REFRESH_TTL"] = "0"
      begin
        first = session(dir)
        assert_includes(first["K0"].members.map(&:name), "K0#old()")
        builds = 0
        first.define_singleton_method(:build_store_in_child) do |*args, **kwargs|
          builds += 1
          super(*args, **kwargs)
        end

        8.times { |i| File.write(File.join(dir, "k#{i}.rb"), "class K#{i}\n  def new; end\nend\n") }
        git(dir, "add", ".")
        git(dir, "commit", "-q", "-m", "switch")

        second = session(dir) # starts the large-diff rebuild
        Timeout.timeout(45) { sleep(0.02) until second.send(:store_fresh?, second.send(:store_cache_path)) }
        second.refresh_if_stale
        assert_includes(second["K0"].members.map(&:name), "K0#new()")

        assert(first.send(:refresh_if_stale))
        assert_equal(0, builds, "a session must re-attach to the store another session just built")
        first.resolve
        members = first["K0"].members.map(&:name)
        assert_includes(members, "K0#new()")
        refute_includes(members, "K0#old()", "the reopened session must not still answer from the old snapshot")
      ensure
        join_rebuilds
        ENV.delete("RUBYDEX_DISK_INDEX")
        ENV.delete("RUBYDEX_REFRESH_TTL")
      end
    end
  end

  def test_a_refresh_that_finds_the_store_fresh_does_nothing
    with_workspace do |dir, _rb|
      graph = session(dir)
      refute(graph.send(:refresh_if_stale), "nothing changed: no rebuild, no reopen")
      assert_includes(graph["Foo"].members.map(&:name), "Foo#bar()")
    end
  end

  def test_boot_uses_an_existing_stale_store_while_it_refreshes
    with_workspace do |dir, rb|
      first = session(dir)
      cache = first.send(:store_cache_path)
      File.write(rb, "class Foo\n  def baz; end\nend\n")
      second = Rubydex::Graph.configure_for_workspace(dir)
      @sessions << second

      started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      second.index_workspace
      assert_operator(Process.clock_gettime(Process::CLOCK_MONOTONIC) - started, :<, 0.5)
      assert_includes(second["Foo"].members.map(&:name), "Foo#bar()")
      Timeout.timeout(45) { sleep(0.02) until second.send(:store_fresh?, cache) }
      assert(second.refresh_if_stale)
      assert_includes(second["Foo"].members.map(&:name), "Foo#baz()")
    end
  end

  def test_a_session_waits_briefly_for_another_rebuild_before_serving_its_snapshot
    with_workspace do |dir, rb|
      graph = session(dir)
      cache = graph.send(:store_cache_path)
      File.write(rb, "class Foo\n  def baz; end\nend\n")
      lock = graph.send(:claim_rebuild, cache)
      begin
        started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
        refute(graph.refresh_if_stale, "the old snapshot stays available while another builder holds the lock")
        elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - started
        assert_operator(elapsed, :>=, 0.08)
        assert_operator(elapsed, :<, 0.5)
        assert_includes(graph["Foo"].members.map(&:name), "Foo#bar()")
      ensure
        Dir.rmdir(lock)
      end
    end
  end

  def test_a_failed_background_rebuild_falls_back_to_a_fresh_in_memory_index
    with_workspace do |dir, rb|
      graph = session(dir)
      File.write(rb, "class Foo\n  def baz; end\nend\n")
      graph.define_singleton_method(:launch_rebuild) do |_cache, lock|
        @rebuild_process = Process.detach(Process.spawn(Gem.ruby, "-e", "exit 1"))
        Dir.rmdir(lock)
      end

      graph.refresh_if_stale
      Timeout.timeout(5) { sleep(0.02) while graph.instance_variable_get(:@rebuild_process).alive? }
      assert(graph.refresh_if_stale)
      assert_includes(graph["Foo"].members.map(&:name), "Foo#baz()")
    end
  end

  def test_a_session_serves_its_snapshot_while_its_own_rebuild_runs
    with_workspace do |dir, rb|
      graph = session(dir)
      cache = graph.send(:store_cache_path)
      File.write(rb, "class Foo\n  def baz; end\nend\n")

      started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
      refute(graph.refresh_if_stale, "the existing index answers while the rebuild runs")
      assert_operator(Process.clock_gettime(Process::CLOCK_MONOTONIC) - started, :<, 0.5)
      assert_includes(graph["Foo"].members.map(&:name), "Foo#bar()")

      Timeout.timeout(45) { sleep(0.02) until graph.send(:store_fresh?, cache) }
      assert(graph.refresh_if_stale)
      assert_includes(graph["Foo"].members.map(&:name), "Foo#baz()")
    end
  end

  def test_only_one_session_claims_the_rebuild
    with_workspace do |dir, _rb|
      graph = session(dir)
      cache = graph.send(:store_cache_path)

      lock = graph.send(:claim_rebuild, cache)
      assert_kind_of(String, lock, "the first session gets the lock")
      refute(graph.send(:claim_rebuild, cache), "the second session must not spawn a second rebuild")

      # A builder that crashed leaves the lock behind; it must expire, not wedge the directory.
      File.utime(Time.now - 3600, Time.now - 3600, lock)
      assert_kind_of(String, graph.send(:claim_rebuild, cache), "an expired lock is reclaimed")
    end
  end

  # A committed store can ship a *file* at the lock path. It never expires the way an abandoned
  # directory does (`Dir.mkdir` raises EEXIST forever, `Dir.rmdir` raises ENOTDIR), so leaving it alone
  # would suppress every rebuild of that store and keep serving whatever snapshot was committed.
  def test_a_file_at_the_rebuild_lock_path_does_not_suppress_rebuilds
    with_workspace do |dir, _rb|
      graph = session(dir)
      cache = graph.send(:store_cache_path)
      lock = "#{cache}.rebuild"
      File.write(lock, "not a lock directory")

      claimed = graph.send(:claim_rebuild, cache)

      assert_equal(lock, claimed, "a non-directory at the lock path is removed, not waited on")
      assert(File.directory?(lock), "the claim replaced it with the real lock")
      refute(graph.send(:claim_rebuild, cache), "and the lock is exclusive afterwards")
    end
  end

  def test_a_symlink_at_the_rebuild_lock_path_does_not_suppress_rebuilds
    skip("symlinks require elevated privileges on Windows") if Gem.win_platform?

    with_workspace do |dir, _rb|
      graph = session(dir)
      cache = graph.send(:store_cache_path)
      lock = "#{cache}.rebuild"

      # Dangling symlink: File.exist? is false, yet Dir.mkdir raises EEXIST.
      File.symlink(File.join(dir, "does-not-exist"), lock)
      claimed = graph.send(:claim_rebuild, cache)
      assert_equal(lock, claimed, "a dangling symlink at the lock path must be cleared and claimed")
      assert(File.directory?(lock) && !File.symlink?(lock))
      graph.send(:release_rebuild_lock, lock)

      # Symlink pointing to an existing directory: File.directory? is true, yet Dir.rmdir raises ENOTDIR.
      target_dir = File.join(dir, "real-dir")
      Dir.mkdir(target_dir)
      File.symlink(target_dir, lock)
      claimed = graph.send(:claim_rebuild, cache)
      assert_equal(lock, claimed, "a directory symlink at the lock path must be unlinked, not waited on")
      assert(File.directory?(lock) && !File.symlink?(lock))
      assert(Dir.exist?(target_dir), "unlinking the symlink must not touch its target directory")
    end
  end

  # The lock directory is removed by the detached builder's `ensure`; a builder killed before it runs
  # one must degrade the indexer to "not fresh" instead of spinning until the lock's mtime expiry.
  def test_waiting_for_a_lost_builder_is_bounded
    with_workspace do |dir, _rb|
      graph = session(dir)
      cache = graph.send(:store_cache_path)
      lock = "#{cache}.rebuild"
      Dir.mkdir(lock)
      begin
        started = Process.clock_gettime(Process::CLOCK_MONOTONIC)
        refute(graph.send(:wait_for_rebuild, cache, 0.1), "a lock that outlives the timeout is abandoned")
        elapsed = Process.clock_gettime(Process::CLOCK_MONOTONIC) - started
        assert_operator(elapsed, :>=, 0.09)
        assert_operator(elapsed, :<, 5, "the wait is bounded, it does not run to the lock's mtime expiry")
      ensure
        Dir.rmdir(lock)
      end

      assert(graph.send(:wait_for_rebuild, cache, 0.1), "no lock means nothing to wait for")
    end
  end

  # The freshness check is the whole cost of a warm boot on a large workspace: the walk (~105 ms on
  # 6,441 files) and the digest over it. Re-reading the sidecar and re-signing the same scan doubled
  # that for nothing, and a sidecar is parsed data, so the second parse was pure spend.
  def test_a_warm_boot_parses_the_manifest_and_signs_the_scan_once
    with_workspace do |dir, _rb|
      session(dir) # a fresh store, marker and sidecar

      warm = Rubydex::Graph.configure_for_workspace(dir)
      counts = Hash.new(0)
      warm.singleton_class.prepend(Module.new do
        define_method(:read_manifest) do |*args|
          counts[:manifest] += 1
          super(*args)
        end
        define_method(:store_signature) do |*args|
          counts[:signature] += 1
          super(*args)
        end
      end)
      warm.index_workspace

      assert_equal(1, counts[:manifest], "a warm boot must parse the manifest sidecar once")
      assert_equal(1, counts[:signature], "a warm boot must sign the source scan once")

      # `store_fresh?` must still reject a store whose manifest sidecar is missing, even with a matching marker.
      cache = warm.send(:store_cache_path)
      manifest_backup = File.read("#{cache}.files")
      FileUtils.rm_f("#{cache}.files")
      refute(warm.send(:store_fresh?, cache), "a matching marker without a manifest is not fresh")
      File.write("#{cache}.files", manifest_backup)

      # A store whose marker is gone is attached but unproven, so it still has to refresh.
      FileUtils.rm_f("#{cache}.hash")
      refreshed = false
      stale = Rubydex::Graph.configure_for_workspace(dir)
      stale.singleton_class.prepend(Module.new do
        define_method(:refresh_if_stale) do |*args|
          refreshed = true
          super(*args)
        end
      end)
      stale.index_workspace

      assert(refreshed, "a missing marker must refresh instead of trusting the attached store")
    end
  end

  def test_cold_boot_and_corrupt_recovery_hold_the_rebuild_lock
    with_workspace do |dir, _rb|
      graph = Rubydex::Graph.configure_for_workspace(dir)
      @sessions << graph
      cache = graph.send(:store_cache_path)
      lock = "#{cache}.rebuild"
      lock_held_during_build = []
      graph.define_singleton_method(:build_store_in_child) do |*args|
        lock_held_during_build << Dir.exist?(lock)
        super(*args)
      end

      graph.index_workspace
      assert_equal([true], lock_held_during_build, "cold boot must hold the rebuild lock while building")
      refute(Dir.exist?(lock), "cold boot must release the rebuild lock afterward")

      # Simulate an unopenable active generation file on next boot.
      active_store = graph.send(:active_store_path)
      graph.detach_store
      File.write(active_store, "not a redb store")

      recovery = Rubydex::Graph.configure_for_workspace(dir)
      @sessions << recovery
      recovery_locks = []
      recovery.define_singleton_method(:build_store_in_child) do |*args|
        recovery_locks << Dir.exist?(lock)
        super(*args)
      end

      recovery.index_workspace
      assert_equal([true], recovery_locks, "corrupt recovery must hold the rebuild lock while rebuilding")
      refute(Dir.exist?(lock), "corrupt recovery must release the rebuild lock afterward")
      assert_includes(recovery["Foo"].members.map(&:name), "Foo#bar()")
    end
  end

  def test_contended_cold_boot_waits_for_rebuild_lock_and_adopts_published_store
    with_workspace do |dir, _rb|
      builder = Rubydex::Graph.configure_for_workspace(dir)
      @sessions << builder
      cache = builder.send(:store_cache_path)
      lock = builder.send(:claim_rebuild, cache)

      waiter = Rubydex::Graph.configure_for_workspace(dir)
      @sessions << waiter
      builds = 0
      waiter.define_singleton_method(:build_store_in_child) do |*|
        builds += 1
        raise "contended cold boot should adopt the holder's store instead of rebuilding"
      end

      rebuilder = Thread.new do
        sleep(0.05)
        builder.send(:build_store_in_child, cache)
      ensure
        builder.send(:release_rebuild_lock, lock)
      end

      waiter.index_workspace
      rebuilder.join(30)

      assert_equal(0, builds)
      assert_includes(waiter["Foo"].members.map(&:name), "Foo#bar()")
    end
  end

  def test_cleanup_stale_generations_respects_grace_window_for_unreferenced_generations
    with_workspace do |dir, _rb|
      graph = session(dir)
      cache = graph.send(:store_cache_path)
      store_dir = File.dirname(cache)
      keep = File.basename(graph.send(:active_store_path))

      recent_racing = File.join(store_dir, "index.1111111111111111.redb")
      expired_orphan = File.join(store_dir, "index.2222222222222222.redb")
      previous_gen = File.join(store_dir, "index.3333333333333333.redb")
      File.write(recent_racing, "racing")
      File.write(expired_orphan, "orphan")
      File.write(previous_gen, "previous")
      old_time = Time.now - (Rubydex::Graph::GENERATION_CLEANUP_GRACE_SECONDS + 10)
      File.utime(old_time, old_time, expired_orphan)

      graph.send(:cleanup_stale_generations, cache, keep, File.basename(previous_gen))

      assert(File.exist?(recent_racing), "recent unreferenced generation must survive the grace window")
      refute(File.exist?(expired_orphan), "expired unreferenced generation must be swept")
      refute(File.exist?(previous_gen), "explicitly superseded generation must be swept immediately")
    end
  end
end
