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

    at_exit { release_all }

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
        File.expand_path(Gem.win_platform? ? "rubydex-index-manager.exe" : "rubydex-index-manager", __dir__)
      end

      # Starts the machine-wide manager when no live one holds its lock. Liveness is the same
      # primitive as sessions use: the manager holds an exclusive lock on `manager.lock`, so a
      # lock we can take means no manager is running. The manager re-checks the lock on startup,
      # so two sessions spawning at the same instant cost one wasted process, never two daemons.
      #: (String registry) -> nil
      def launch_if_absent(registry)
        handle = File.new(File.join(File.dirname(registry), "manager.lock"), "w")
        # Non-blocking is the point: a live manager holds this lock for its whole lifetime, so a
        # blocking `flock` waits for the manager to exit and the session never launches anything.
        taken = handle.flock(File::LOCK_EX | File::LOCK_NB)
        # Closed either way: our own open handle would keep Windows from replacing the lock file,
        # and the manager takes the lock for itself once it starts.
        handle.close
        return unless taken

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
        payload = JSON.generate({ workspace:, store:, builder:, docs: })

        if Gem.win_platform?
          # Windows cannot rename a file this process has open, so there is no temp to publish:
          # claim the final name exclusively, lock it, then write. A manager probing the gap sees
          # an unlocked file it cannot parse, and it cannot unlink one an open handle holds
          # either, so the session lists itself on the manager's next poll.
          handle = File.open(path, File::RDWR | File::CREAT | File::EXCL)
          handle.flock(File::LOCK_EX | File::LOCK_NB)
          handle.write(payload)
          handle.flush
        else
          # Create and lock under a non-`.json` name. The manager prunes a registry file whose lock
          # it can take, so publishing an unlocked file would let it unlink a session that is about
          # to lock it. Rename (with the lock already held) is the atomic publish.
          temp = "#{path}.tmp"
          handle = File.new(temp, "w")
          handle.flock(File::LOCK_EX | File::LOCK_NB)
          handle.write(payload)
          handle.flush
          File.rename(temp, path)
        end

        @handles[path] = handle
        handle
      end

      # Ends every registered session: closes the handle, which releases the OS lock, and removes
      # the registry file. `at_exit` calls this; so can a host or a test that has to release the
      # lock before the process exits.
      #: -> void
      def release_all
        @handles.each do |path, handle|
          handle.close
          File.delete(path) if File.exist?(path)
        rescue SystemCallError
          # Another process probing the registry holds the file open on Windows. The lock is
          # already released, which is what liveness depends on, so a failed unlink must not
          # turn shutdown into a crash.
        end
        @handles.clear
      end
    end
  end
end
