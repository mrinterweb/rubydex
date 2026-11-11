# frozen_string_literal: true

require "rubydex"
require "rubydex/index_manager"
require "tmpdir"
require "fileutils"
require "minitest/autorun"
require "json"

# Liveness is an OS lock, not a PID: a session holds an exclusive lock on its
# registry file for its whole lifetime, and the manager prunes the file once the
# lock is free. These tests cover the died-without-cleanup case (`exit!`, which skips at_exit the
# way a kill does, so valgrind still sees a well-formed child report) and the manager exiting when
# the registry empties.
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

    IO.popen([MANAGER_BIN, *args], "r", &:read)
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

  def test_register_publishes_the_registry_file_only_after_locking_it
    Dir.mktmpdir("rdx-reg-") do |dir|
      registry = File.join(dir, "sessions")
      handle = Rubydex::IndexManager.register(
        workspace: "/ws",
        store: "/ws/tmp/index.redb",
        builder: ["ruby"],
        docs: 1,
        registry: registry,
      )

      assert_empty(Dir.glob(File.join(registry, "*.tmp")), "the unlocked temp file must not remain")
      assert_equal(1, Dir.glob(File.join(registry, "*.json")).size, "the locked file is published")
      assert(handle.flock(File::LOCK_EX), "the published file is already locked by the session")
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
        exit!
      RUBY

      child_pid = Process.spawn("ruby", "-I#{File.join(ROOT, "lib")}", script, registry)
      # A child valgrind traces boots ~1000x slower, so its boot time is not a 5 s budget: the
      # memleak job failed on this deadline, not on anything the registry did wrong.
      deadline = Time.now + (ENV["RUBY_MEMCHECK_RUNNING"] ? 180 : 5)
      until Dir.glob(File.join(registry, "*.json")).size == 1
        sleep(0.05)
        raise "the child never registered" if Time.now > deadline
      end
      Process.wait(child_pid)

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
      # A `--list` pays the same process startup, so the bound scales with the machine (and with
      # valgrind, which traces the manager as a child) instead of assuming real-world speed.
      started = Time.now
      manager("--registry", registry, "--list")
      baseline = Time.now - started
      started = Time.now
      manager("--registry", registry, "--run", "--debounce-ms", "100")
      assert_operator(
        Time.now - started,
        :<,
        baseline * 2 + 0.2,
        "the manager must exit within two polls of the registry going empty",
      )
    end
  end

  # A live manager holds `manager.lock` for its whole lifetime, so taking it has to be a try: a
  # blocking `flock` waits for the manager to exit, and the session that just registered blocks with
  # it. That is what the Windows suite hung on until GitHub's six-hour limit.
  def test_launch_returns_instead_of_waiting_for_a_live_manager
    Dir.mktmpdir("rdx-reg-") do |dir|
      registry = File.join(dir, "sessions")
      FileUtils.mkdir_p(registry)
      lock = File.join(dir, "manager.lock")
      holder = Process.spawn(
        RbConfig.ruby,
        "-e",
        "File.new(ARGV[0], 'w').flock(File::LOCK_EX); puts 'ready'; $stdout.flush; sleep 60",
        lock,
        out: File::NULL,
      )
      begin
        deadline = Time.now + 10
        sleep(0.05) until lock_taken?(lock) || Time.now > deadline
        assert(lock_taken?(lock), "the holder never took the lock")

        launched = Thread.new { Rubydex::IndexManager.launch_if_absent(registry) }
        assert(launched.join(10), "launch_if_absent must not wait for a running manager")
      ensure
        kill_and_reap(holder)
      end
    end
  end

  def kill_and_reap(pid)
    Process.kill("KILL", pid)
    Process.wait(pid)
  rescue Errno::ESRCH, Errno::ECHILD
    nil
  end

  def lock_taken?(path)
    File.open(path, "a") { |file| !file.flock(File::LOCK_EX | File::LOCK_NB) }
  end
end
