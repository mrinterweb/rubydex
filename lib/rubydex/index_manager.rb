# frozen_string_literal: true

# The machine-wide index manager. A session (ruby-lsp, `rdx mcp`) registers its
# workspace and store here and keeps an exclusive OS lock on its registry file for
# its whole lifetime. The manager binary (`rubydex-index-manager`) is the only
# process that subscribes to file-system events; it learns a session died by taking
# over that lock, which the OS released when the process went away. No PIDs are
# consulted, so there is no PID-reuse race, and the mechanism is `flock(2)` on Unix
# and `LockFileEx` on Windows.
#
# Sessions never depend on the manager: with it absent, killed, or never started,
# they still refresh through the git fast path and the stat walk.

require "fileutils"
require "json"
require "securerandom"

module Rubydex
  class IndexManager
    # Registry files stay open (and locked) for the lifetime of the session that
    # created them; `at_exit` releases and deletes them. A session killed with
    # SIGKILL never runs `at_exit`, which is exactly why the manager has to detect
    # death through the lock rather than through cleanup hooks.
    @handles = {}

    at_exit do
      @handles.values.each do |handle|
        path = handle.path
        handle.close
        File.delete(path) if File.exist?(path)
      end
      @handles.clear
    end

    # A burst of file-system events inside this window collapses into one indexer.
    DEBOUNCE_MS = 200

    class << self
      # The indexer the manager spawns for a session: a Ruby process that refreshes the store and
      # exits. The manager appends `--full` when a burst covers a quarter of the manifest.
      #: (String workspace, String store) -> Array<String>
      def builder_argv(workspace, store)
        [Gem.ruby, "-r", "rubydex", File.expand_path("index_session.rb", __dir__), workspace, store]
      end

      # The manager binary, shipped next to the extension in the gem's lib directory.
      #: -> String
      def manager_binary
        File.expand_path("rubydex-index-manager", __dir__)
      end

      # Starts the machine-wide manager when no live one holds its lock. Liveness is the same
      # primitive as sessions use: the manager holds an exclusive lock on `manager.lock`, so a
      # lock we can take means no manager is running. The manager re-checks the lock on startup,
      # so two sessions spawning at the same instant cost one wasted process, never two daemons.
      #: (String registry) -> nil
      def launch_if_absent(registry)
        handle = File.new(File.join(File.dirname(registry), "manager.lock"), "w")
        return unless handle.flock(File::LOCK_EX)

        handle.close
        pid = Process.spawn(manager_binary, "--registry", registry, "--run")
        Process.detach(pid)
        nil
      end

      # Registers a session and returns the locked handle, which the caller must keep
      # referenced for the whole session (the lock is per file descriptor).
      #: (String workspace, String store, Array<String> builder, Integer docs, String registry) -> File
      def register(workspace:, store:, builder:, docs:, registry:)
        FileUtils.mkdir_p(registry)
        path = File.join(registry, "#{Process.pid}-#{SecureRandom.hex(4)}.json")
        handle = File.new(path, "w")
        handle.flock(File::LOCK_EX)
        handle.write(JSON.generate({ workspace:, store:, builder:, docs: }))
        handle.flush
        @handles[path] = handle
        handle
      end
    end
  end
end
