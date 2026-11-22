# frozen_string_literal: true

# valgrind: skip — spawns the manager and a store build

require "test_helper"
require "tmpdir"
require "fileutils"
require "json"
require "timeout"

# A workspace that opts into `[disk_index] manager` registers itself with the machine-wide
# manager when it indexes, so the manager can keep the store fresh between sessions.
class DiskIndexManagerSessionTest < Minitest::Test
  MANAGER_BIN = File.expand_path(
    "../lib/rubydex/#{Gem.win_platform? ? "rubydex-index-manager.exe" : "rubydex-index-manager"}",
    __dir__,
  )

  def setup
    super
    skip_unless_store_feature
  end

  def with_workspace(config)
    Dir.mktmpdir do |tmpdir|
      # The registry records the workspace as the graph resolved it (macOS `/private/var/...`), so
      # compare against the real path rather than `Dir.mktmpdir`'s `/var/...` alias.
      dir = File.realpath(tmpdir)
      File.write(File.join(dir, "rubydex.toml"), config)
      FileUtils.mkdir_p(File.join(dir, "lib"))
      File.write(File.join(dir, "lib", "app.rb"), "class App\nend\n")
      graph = Rubydex::Graph.configure_for_workspace(dir)
      graph.index_workspace
      begin
        yield(graph, dir)
      ensure
        # Wait for any detached rebuild and close open handles before `mktmpdir` removes `dir`,
        # since Windows cannot unlink an open file or a directory being written to.
        graph.instance_variable_get(:@rebuild_process)&.join(30)
        graph.detach_store
        Rubydex::IndexManager.release_all
      end
    end
  end

  # The manager decides "rebuild instead of surgical" by comparing a burst of filesystem events
  # against the number a session registers, so the registered count must be the manifest's file count
  # and not the count of indexed roots.
  def test_an_opted_in_session_gets_registered
    skip("the Rust manager does not list a live Ruby session on Windows yet") if Gem.win_platform?

    with_workspace("[disk_index]\nenabled = true\nmanager = true\n") do |graph, dir|
      registry = File.join(graph.send(:platform_cache_root), "rubydex", "manager", "sessions")
      sessions = JSON.parse(%x(#{MANAGER_BIN} --registry #{registry} --list))
      mine = sessions.select { |session| session["workspace"] == dir }

      assert_equal(1, mine.size, "the live session must be listed exactly once")
      assert_equal(1, mine.first["docs"], "lib/app.rb, not the roots the session walked")
    end
  end

  def test_a_workspace_that_stays_out_of_the_manager_leaves_no_session
    with_workspace("[disk_index]\nenabled = true\n") do |graph, dir|
      registry = File.join(graph.send(:platform_cache_root), "rubydex", "manager", "sessions")
      sessions = JSON.parse(%x(#{MANAGER_BIN} --registry #{registry} --list))

      assert_equal(
        [],
        sessions.select { |session| session["workspace"] == dir },
        "no opt-in means no session registered",
      )
    end
  end

  def test_the_manager_indexer_waits_for_its_rebuild_to_publish
    with_workspace("[disk_index]\nenabled = true\n") do |graph, dir|
      store = graph.send(:store_cache_path)
      previous = File.read("#{store}.hash")
      File.write(File.join(dir, "Gemfile.lock"), "new dependency set\n")

      begin
        assert(run_index_session(dir, store), "the indexer must succeed")
        refute_equal(previous, File.read("#{store}.hash"), "the store must be published before the indexer exits")
      ensure
        Timeout.timeout(45) { sleep(0.02) while Dir.exist?("#{store}.rebuild") }
      end
    end
  end

  def test_a_full_build_removes_the_lock_it_claimed
    Dir.mktmpdir do |dir|
      File.write(File.join(dir, "foo.rb"), "class Foo; end\n")
      store = File.join(dir, "tmp", "rubydex", "index.redb")
      lock = "#{store}.rebuild"

      assert(run_index_session(dir, store, "--full"), "the full build must succeed")
      refute(File.exist?(lock), "the builder must release the rebuild lock")
      manifest = JSON.parse(File.read("#{store}.files"))
      assert(File.exist?(File.join(File.dirname(store), manifest.fetch("store"))), "the store was built")
    end
  end

  def test_a_full_build_leaves_a_lock_it_did_not_claim_alone
    Dir.mktmpdir do |dir|
      File.write(File.join(dir, "foo.rb"), "class Foo; end\n")
      store = File.join(dir, "tmp", "rubydex", "index.redb")
      FileUtils.mkdir_p(File.dirname(store))
      lock = "#{store}.rebuild"
      Dir.mkdir(lock)

      assert(run_index_session(dir, store, "--full"), "losing the lock is not a failure")
      assert(File.exist?(lock), "another builder's lock must not be removed")
      assert_empty(Dir.glob(File.join(File.dirname(store), "index*.redb")), "no store is built without the lock")
    end
  end

  private

  def run_index_session(workspace, store, *flags)
    script = File.expand_path("../lib/rubydex/index_session.rb", __dir__)
    system(
      RbConfig.ruby,
      "-I#{File.expand_path("../lib", __dir__)}",
      script,
      workspace,
      store,
      *flags,
      out: File::NULL,
      err: File::NULL,
    )
  end
end
