# frozen_string_literal: true

require "rubydex"
require "rubydex/index_manager"
require "tmpdir"
require "fileutils"
require "minitest/autorun"
require "json"

# Liveness is an OS lock, not a PID: a session holds an exclusive lock on its
# registry file for its whole lifetime, and the manager prunes the file once the
# lock is free. These tests cover the killed-session case (SIGKILL, no at_exit)
# and the manager exiting when the registry empties.
class IndexManagerRegistryTest < Minitest::Test
  ROOT = File.expand_path("..", __dir__)
  MANAGER_BIN = File.join(ROOT, "rust", "target", "debug", "rubydex-index-manager")

  def manager(*args)
    unless File.exist?(MANAGER_BIN)
      system(
        "cargo",
        "build",
        "-p",
        "rubydex-index-manager",
        chdir: File.join(ROOT, "rust"),
      ) or raise "cargo build failed"
    end

    %x(#{MANAGER_BIN} #{args.join(" ")})
  end

  def test_it_lists_a_session_that_holds_its_lock
    Dir.mktmpdir("rdx-reg-") do |dir|
      registry = File.join(dir, "sessions")
      handle = Rubydex::IndexManager.register(
        workspace: "/ws",
        store: "/ws/tmp/index.redb",
        builder: ["ruby", "-r", "rubydex"],
        docs: 100,
        registry: registry,
      )
      assert(handle.flock(File::LOCK_EX), "the test process must still hold the lock")

      sessions = JSON.parse(manager("--registry", registry, "--list"))
      assert_equal(1, sessions.size, "a live session must be listed")
      assert_equal("/ws", sessions[0]["workspace"])
      handle.close
    end
  end

  def test_it_prunes_a_session_killed_without_cleanup
    Dir.mktmpdir("rdx-reg-") do |dir|
      registry = File.join(dir, "sessions")
      FileUtils.mkdir_p(registry)
      script = File.join(dir, "session.rb")
      File.write(script, <<~RUBY)
        require "rubydex"
        require "rubydex/index_manager"
        Rubydex::IndexManager.register(
          workspace: "/ws", store: "/ws/tmp/index.redb", builder: ["ruby"], docs: 100,
          registry: ARGV[0]
        )
        sleep 60
      RUBY

      child_pid = Process.spawn("ruby", "-I#{File.join(ROOT, "lib")}", script, registry)
      deadline = Time.now + 5
      until Dir.glob(File.join(registry, "*.json")).size == 1
        sleep(0.05)
        raise "the child never registered" if Time.now > deadline
      end

      Process.kill(:KILL, child_pid)
      Process.wait(child_pid)
      sleep(0.2)

      sessions = JSON.parse(manager("--registry", registry, "--list"))
      assert_equal([], sessions, "a killed session must be pruned")
      assert_equal(
        [],
        Dir.glob(File.join(registry, "*.json")),
        "the dead registry file must be gone",
      )
    end
  end

  def test_the_manager_exits_when_no_session_is_alive
    Dir.mktmpdir("rdx-reg-") do |dir|
      registry = File.join(dir, "sessions")
      FileUtils.mkdir_p(registry)
      started = Time.now
      manager("--registry", registry, "--run", "--debounce-ms", "100")
      assert_operator(
        Time.now - started,
        :<,
        0.5,
        "the manager must exit within two polls of the registry going empty",
      )
    end
  end
end
