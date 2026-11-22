# frozen_string_literal: true

require "fileutils"
require "json"
require "open3"
require "securerandom"
require "rubydex/index_manager"

module Rubydex
  # The global graph representing all declarations and their relationships for the workspace
  #
  # Note: this class is partially defined in C to integrate with the Rust backend
  class Graph
    INDEXABLE_EXTENSIONS = [".rb", ".rake", ".rbs", ".ru"].freeze
    # Immutable per-build store filename inside the store directory. A rebuild writes a new
    # generation and points `index.redb.files` at it instead of replacing an open `.redb` file
    # in place, so Windows sessions can keep reading the old snapshot while a rebuild publishes.
    GENERATION_STORE_PATTERN = /\Aindex\.[0-9a-f]{16}\.redb\z/
    # A rebuild lock older than this is treated as abandoned (a builder that crashed) and
    # reclaimed, so a directory can never wedge itself out of rebuilding forever.
    REBUILD_LOCK_TIMEOUT = 30 * 60

    # How long the indexer waits for a claimed rebuild to finish before giving up. The lock directory is
    # removed by the detached builder's `ensure`, so a builder killed before it runs one leaves it
    # behind, and an unbounded wait on it would stall this indexer until the lock's mtime expiry. The
    # ceiling has to clear a real rebuild, though: a full build of a one-file workspace measures ~9 s on
    # an M-series laptop (mostly process start plus the store write), and a large workspace is a small
    # multiple of that, against the ~25 s documented for a surgical refresh's fallback.
    REBUILD_LOCK_WAIT = 2 * 60

    # Above this fraction of changed files a full rebuild (one child, a fresh store shared by every
    # session) is cheaper than growing each session's overlay. Measured: surgical 200
    # files = 71 ms; full rebuild = 24.6 s.
    REBUILD_DIFF_RATIO = 0.25

    # How stale the freshness walk may go, in seconds. The walk stats every indexable file (~105 ms
    # warm, ~0.9 s cold, on 6,441 files) and runs before every tool call, so it is reused
    # inside this window. Owner decision 2026-08-10: an edit made outside the editor (sed, a formatter,
    # a script) may be answered up to this stale. A moved HEAD bounds the case that matters (a branch
    # switch or a commit rewrites files in bulk) and the editor path is unaffected, because it indexes
    # its own saves. RUBYDEX_REFRESH_TTL overrides it.
    REFRESH_TTL = 2

    # Once the overlay would hold this fraction of the workspace's files, a rebuild is the cheaper
    # answer. A surgical apply leaves the edited files in the overlay and only a rebuild compacts it,
    # so a session that absorbs many small branch switches converges on holding the whole index in
    # memory: ~12 MB per 1,000 files measured, against ~5 MB for the store it opened. Same
    # shape as REBUILD_DIFF_RATIO, which bounds one refresh; this one bounds the session.
    OVERLAY_COMPACTION_RATIO = 0.25

    class << self
      # Creates a new graph with the loaded configuration. For use cases where the graph must be shared between
      # different tools, do not use this. Create and own a `Config` object instead.
      #
      #: (String) -> instance
      def configure_for_workspace(workspace_path)
        graph = new
        graph.load_config(Config.load(workspace_path))
        graph
      end

      # Layout version of the persisted store, owned by the Rust side: it is what makes an older
      # store unreadable, not the gem version. Cached: it never changes within a process.
      #
      #: -> Integer
      def store_format_version
        @store_format_version ||= rdx_store_format_version
      rescue NoMethodError
        # Built without the redb-store feature: nothing is ever persisted, so any value works.
        0
      end
    end

    # Index all files and dependencies of the workspace that exists in `workspace_path`.
    #
    # Disk-backed orchestration (runtime opt-in via RUBYDEX_DISK_INDEX=1; the store feature is
    # compiled in by default — see ext/rubydex/extconf.rb): build (or reuse) a redb store of
    # the whole resolved graph in a short-lived child process (Process.spawn) so its peak indexing
    # memory is reclaimed when the child exits, then attach the store to this graph. The long-lived
    # server therefore holds the bulk index off-heap and serves reads from disk. Falls back to the
    # in-memory path if the store can't be built.
    #
    # The disk-backed path now supports live edits via a materialize-on-write overlay: edits
    # flow through to the in-memory overlay, with store-backed nodes materialized before
    # mutation. It stays opt-in because the overlay is in-memory only (a fresh boot rebuilds
    # the store from scratch; persisting overlay writes back is a future task).
    #: -> Array[String]
    def index_workspace
      return index_all(workspace_paths) unless disk_index_enabled?

      cache = store_cache_path
      @head_sha = workspace_head
      # Parse the manifest once up front so warm boot knows which generation file to open without
      # reading `index.redb.files` twice. When the manifest is absent or unusable, fall back to the
      # legacy `index.redb` path so an older store can still be quarantined or rebuilt in the background.
      adopted = adopt_manifest(cache)
      store_file = @manifest_store_path || cache
      if File.exist?(store_file)
        begin
          attach_store_generation(store_file)
        rescue RuntimeError # no usable snapshot: wait for a new store instead of serving the old one
          build_store_in_child(cache)
          adopted = adopt_manifest(cache)
          attach_store_generation(@manifest_store_path || cache)
        end
      else
        build_store_in_child(cache)
        adopted = adopt_manifest(cache)
        attach_store_generation(@manifest_store_path || cache)
      end
      return index_all(workspace_paths) if quarantine_if_untrustworthy(cache)

      # The manifest is what the attached snapshot is vouched for by: it records what the store holds
      # and is the baseline a surgical refresh diffs against. A workspace can commit the store (and its
      # marker) without one, so an unprovenanced snapshot is never served — it gets the same answer a
      # corrupt store gets. A stale store *with* a manifest is fine: it answers while it refreshes.
      if adopted
        # One walk and one digest for the whole boot: the marker is exactly the signature this
        # session would publish, and `adopt_manifest` already parsed the sidecar that
        # `store_published?` would parse again.
        expected_marker = store_signature
        marker = "#{cache}.hash"
        if File.exist?(marker) && File.read(marker) == expected_marker
          # What this session is serving: the store's content plus the commit it read it at. HEAD
          # belongs here and deliberately not in `store_signature` — a commit rewrites no files, so
          # the shared store is still exactly right and only this session has to notice, while a
          # marker keyed on HEAD would throw away a good store after every commit.
          @session_signature = "#{@head_sha}#{expected_marker}"
        else
          refresh_if_stale
        end
      else
        warn("rubydex: disk-backed index has no usable provenance; indexing in memory")
        detach_attached_store
        index_all(workspace_paths)
        resolve
        # Only a build rewrites the sidecar, so leave one running: without it every boot on this
        # workspace takes this path again (a store written by an older release, whose Marshal sidecar
        # this version will not parse, would never migrate). The rebuild also replaces whatever
        # snapshot was committed, which is what makes serving it unnecessary in the first place.
        lock = claim_rebuild(cache)
        launch_rebuild(cache, lock) if lock
      end
      register_session(cache) if index_manager_enabled?

      []
    rescue StandardError, NotImplementedError => e
      # NotImplementedError is a ScriptError, not a StandardError; keep it here so a missing
      # platform primitive falls back instead of escaping into the editor's tool call.
      warn("rubydex: disk-backed index unavailable (#{e.class}: #{e.message}); falling back to in-memory")
      # `index_all` is a no-op while a store is attached, so a failure after `attach_store` (a
      # corrupt manifest sidecar, an unreadable scan) has to detach before the fallback is real.
      # No store feature compiled in means nothing was ever attached, and `detach_store` raises
      # rather than no-op there — which would replace the failure being recovered from.
      detach_attached_store if respond_to?(:detach_store) && self.class.store_format_version.positive?
      index_all(workspace_paths)
    end

    # Moves this session to the current source state, cheapest route first: a fresh marker means
    # nothing changed; else a small diff is applied into the overlay (no rebuild); else the store is
    # rebuilt once for everyone. Sessions never coordinate: the signature is what notices, and the
    # first session to claim the create-exclusive rebuild lock spawns the one rebuild child. redb pins
    # a snapshot at open, so every session must re-attach to see a new store; the rest find the marker
    # fresh and pay only the reopen (~71 ms measured, vs ~25 s for the rebuild).
    #
    # Leaves the graph resolved: a surgical apply resolves, a re-attached store is already resolved,
    # and a rebuilt store was built from a resolved graph. Callers must not resolve again.
    #
    # -> bool
    def refresh_if_stale
      cache = store_cache_path
      head = workspace_head
      # A moved HEAD means the tree changed in bulk, which is exactly what the walk measures, so the
      # memo is dropped rather than reused stale: stale stamps would be published as what the rebuild
      # had just indexed, and the next session would disagree and rebuild again.
      scan = source_scan(rewalk: @head_sha != head)
      @head_sha = head
      expected_marker = store_signature(scan)
      signature = "#{head}#{expected_marker}"
      return false if @session_signature == signature

      marker = "#{cache}.hash"
      if store_published?(marker, expected_marker, cache)
        attach_store_generation(@manifest_store_path || cache)
      elsif (changes = surgical_changes(scan))
        apply_changes(changes)
        @applied_files = scan
      else
        if @rebuild_process && !@rebuild_process.alive?
          status = @rebuild_process.value
          @rebuild_process = nil
          unless status.success?
            warn("rubydex: store rebuild failed; falling back to in-memory")
            detach_attached_store
            index_all(workspace_paths)
            resolve
            @session_signature = signature
            return true
          end
        end

        if (lock = claim_rebuild(cache))
          launch_rebuild(cache, lock)
        end

        deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + 0.1
        until store_published?(marker, expected_marker, cache)
          break if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline

          sleep(0.02)
        end
        return false unless store_published?(marker, expected_marker, cache)

        attach_store_generation(@manifest_store_path || cache)
      end
      @session_signature = signature
      true
    rescue StandardError => e
      # A session must never lose its index because a refresh failed; keeping the current
      # snapshot is strictly better than raising into the editor's tool call.
      warn("rubydex: session refresh failed (#{e.class}: #{e.message}); keeping the current snapshot")
      false
    end

    # Moves an untrustworthy store out of the cache path and detaches it, returning true when it
    # did. A store that failed to decode (corrupt, or written by an incompatible layout) must not
    # keep answering queries with holes: while one stays attached the layered accessors keep serving
    # it and `index_all` refuses to index, so detaching is what makes the in-memory fallback real.
    # Quarantining drops the freshness marker with it, so the next run rebuilds instead of re-reading
    # the same bad file. Also called before every MCP tool call, because the error counter only fills
    # up once something has read, and `attach_store` resets it.
    #
    #: (?String) -> bool
    def quarantine_if_untrustworthy(cache = store_cache_path)
      return false unless respond_to?(:store_errors) && store_errors.positive?

      warn("rubydex: disk-backed index returned errors; quarantining the store and indexing in memory")
      store_file = @attached_store_path || cache
      detach_attached_store
      begin
        FileUtils.mv(store_file, "#{cache}.corrupt", force: true) if File.exist?(store_file)
      rescue SystemCallError
        # Another session on Windows may still hold the corrupt file open; dropping the marker
        # below is still enough to make the next run rebuild cleanly.
        nil
      end
      FileUtils.rm_f("#{cache}.hash")
      true
    end

    # What runs before every tool call is served: quarantine a store that started reporting read
    # failures (the counter only fills once something has read, so the previous call is what notices)
    # and re-index in memory, then move to the current source state. Resolving is not part of it —
    # `refresh_if_stale` leaves the graph resolved, and resolving again per call is a full store scan
    # for nothing. Returns true when the session moved.
    #
    #: -> bool
    def prepare_for_tool_call
      return false unless disk_index_enabled?

      # Detaching empties the graph, and `refresh_if_stale` would then report "nothing changed" and
      # leave the session answering with nothing, so the fallback is the same one `index_workspace`
      # takes at boot.
      if quarantine_if_untrustworthy
        index_all(workspace_paths)
        resolve
        return true
      end

      refresh_if_stale
    end

    # Makes this session visible to the machine-wide manager and starts the manager when none is
    # live. The registry handle stays referenced for the process lifetime, which is what keeps the
    # session's lock held and the session listed. Sessions stay correct without the manager: this
    # is an optimization, never a dependency.
    #
    # A manager that cannot start (binary missing, registry not writable, no locks available) must
    # cost the pre-warm it provides and nothing else. Letting that failure reach `index_workspace`'s
    # rescue threw away the store that had just been built and re-indexed the workspace in memory —
    # the footprint the store exists to avoid, caused by the optional part.
    #: (String cache) -> nil
    def register_session(cache)
      registry = File.join(platform_cache_root, "rubydex", "manager", "sessions")
      @session_handle = Rubydex::IndexManager.register(
        workspace: workspace_path,
        store: cache,
        builder: Rubydex::IndexManager.builder_argv(workspace_path, cache),
        # The manager compares a burst of filesystem events against this to decide when a rebuild beats
        # a surgical pass, so it has to be the manifest's file count. `workspace_paths` counts indexed
        # *roots*, which trips that threshold about 100x
        # too eagerly — and costs a Bundler + rbs scan per session to compute.
        docs: @applied_files.to_h.size,
        registry: registry,
      )
      Rubydex::IndexManager.launch_if_absent(registry)
    rescue StandardError => e
      warn("rubydex: index manager unavailable (#{e.class}: #{e.message}); continuing without background refresh")
      nil
    end

    # Files to re-index/delete to move this session from @applied_files to `scan`, or nil when a full
    # rebuild is required (no manifest, lockfile changed, or the diff exceeds REBUILD_DIFF_RATIO).
    #: (Hash[String, String]) -> Hash[Symbol, Array[String]]?
    def surgical_changes(scan)
      return unless @applied_files && @applied_lockfile == lockfile_hash

      changed = scan.filter_map { |rel, stamp| rel if @applied_files[rel] != stamp }
      removed = @applied_files.each_key.reject { |rel| scan.key?(rel) }
      return if (changed.size + removed.size) > (scan.size * REBUILD_DIFF_RATIO)
      return if overlay_divergence(scan) > (scan.size * OVERLAY_COMPACTION_RATIO)

      { changed: changed, removed: removed }
    end

    # What the overlay would hold after applying `scan`: every file whose stamp moved away from what
    # the attached store was built from, plus what the store holds that no longer exists. Only a
    # rebuild (or a re-attach) resets it.
    #: (Hash[String, String]) -> Integer
    def overlay_divergence(scan)
      scan.count { |rel, stamp| @manifest_files[rel] != stamp } +
        @manifest_files.each_key.count { |rel| !scan.key?(rel) }
    end

    # Re-indexes the changed files and deletes the removed ones, then resolves. The store is never
    # rewritten: these mutations land in the session's overlay on top of the store.
    #: (Hash[Symbol, Array[String]]) -> void
    def apply_changes(changes)
      root = workspace_path
      to_uri = ->(rel) { path_to_uri(File.join(root, rel)) }
      changes[:removed].each { |rel| delete_document(to_uri.call(rel)) }
      changes[:changed].each do |rel|
        language = File.extname(rel) == ".rbs" ? "rbs" : "ruby"
        index_source(to_uri.call(rel), File.read(File.join(root, rel)), language)
      end
      resolve
    end

    # What the store at `cache` was built from: the manifest sidecar written next to the marker.
    #
    # The sidecar sits in a directory a workspace may commit (`[disk_index] location = "tmp"`), so it is
    # data, never code: JSON, parsed and shape-checked before a single field is used. An older release's
    # sidecar (Marshal) or any unreadable one fails the parse and is treated as absent — which costs one
    # rebuild and, unlike `Marshal.load`, never instantiates what the workspace put there.
    #
    # Returns whether a manifest was adopted: a store whose sidecar is missing or unusable has no
    # baseline to diff against and no record of what it holds, so callers rebuild instead of serving it.
    #: (String) -> bool
    def adopt_manifest(cache)
      files, lockfile, store_file = read_manifest("#{cache}.files")
      return false unless files && store_file && File.exist?(store_file)

      # What the attached store holds, kept apart from `@applied_files` (what this session is serving)
      # so the overlay can be measured against the store it sits on.
      @manifest_files = files
      @applied_files = files
      @applied_lockfile = lockfile
      @manifest_store_path = store_file
      true
    end

    # Parses a manifest sidecar, returning `[files, lockfile, store_file]`, or `[nil, nil, nil]`
    # when it is absent or not the shape this version writes.
    #: (String) -> [Hash[String, String]?, String?, String?]
    def read_manifest(path)
      manifest = JSON.parse(File.read(path))
      files = manifest["files"] if manifest.is_a?(Hash)
      return [nil, nil, nil] unless files.is_a?(Hash) && files.all? { |rel, stamp| rel.is_a?(String) && stamp.is_a?(String) }

      lockfile = manifest["lockfile"]
      return [nil, nil, nil] unless lockfile.nil? || lockfile.is_a?(String)

      store_file = if manifest.key?("store")
        store_name = manifest["store"]
        return [nil, nil, nil] unless store_name.is_a?(String) && store_name.match?(GENERATION_STORE_PATTERN)

        File.join(File.dirname(path), store_name)
      else
        path.delete_suffix(".files")
      end

      [files, lockfile, store_file]
    rescue SystemCallError, JSON::ParserError, EncodingError
      # Unreadable, not JSON at all, or JSON with an encoding a `String` key cannot hold: all of them
      # mean "no usable manifest", never "raise into the editor's tool call".
      [nil, nil, nil]
    end

    #: (String cache, String lock) -> void
    def launch_rebuild(cache, lock)
      pid = Process.spawn(*Rubydex::IndexManager.builder_argv(workspace_path, cache), "--full", "--claimed")
      @rebuild_process = Process.detach(pid)
    rescue StandardError
      release_rebuild_lock(lock)
      raise
    end

    # Create-exclusive rebuild lock: `Dir.mkdir` raises `Errno::EEXIST` atomically, so N
    # stale sessions (two editors plus an agent server on one directory) pay one rebuild. A lock
    # left by a crashed builder expires on mtime instead of wedging the directory forever.
    #: (String cache) -> (String | false)
    def claim_rebuild(cache)
      lock = "#{cache}.rebuild"
      FileUtils.mkdir_p(File.dirname(lock))
      return lock if make_lock_dir(lock)

      # Something other than our directory holds the lock path. A *file* or symlink there (a committed
      # store can ship one — including a dangling symlink, where `File.exist?` is false but `Dir.mkdir`
      # still raises EEXIST) never expires the way an abandoned directory does, so leaving it would
      # suppress every rebuild of that store and the committed snapshot would stay served as ground truth.
      if File.symlink?(lock) || (File.exist?(lock) && !File.directory?(lock))
        FileUtils.rm_f(lock)
        return lock if make_lock_dir(lock)
      end

      return false unless File.directory?(lock) && (Time.now - File.mtime(lock)) > REBUILD_LOCK_TIMEOUT

      # Reclaiming is racy: a competing session may have reclaimed and re-created the lock
      # between the check and here, in which case this session lost and someone else is
      # building. A build legitimately slower than REBUILD_LOCK_TIMEOUT is reclaimed too, so
      # two builders can run on one store; the publish is atomic and the marker decides which
      # store a session reads.
      begin
        Dir.rmdir(lock)
        Dir.mkdir(lock)
      rescue Errno::ENOENT, Errno::EEXIST, Errno::ENOTDIR
        return false
      end
      lock
    end

    #: (String lock) -> bool
    def make_lock_dir(lock)
      Dir.mkdir(lock)
      true
    rescue Errno::EEXIST
      false
    end

    # Removes a rebuild lock this process holds. Only a directory is ours, so a file left at the path
    # is not touched; `Dir.rmdir` raises ENOTDIR on one, and that must not mask the failure that is
    # already propagating.
    #: (String) -> void
    def release_rebuild_lock(lock)
      Dir.rmdir(lock) if !File.symlink?(lock) && File.directory?(lock)
    rescue SystemCallError
      nil
    end

    # Waits for another session's claimed rebuild to finish, returning false when it outlived
    # `timeout`. The wait exists so an indexer leaves a store settled; the bound exists so a builder
    # that died without removing its lock directory (SIGKILL, OOM — the build is a child precisely to
    # bound memory) degrades to "not fresh" instead of spinning until the lock's mtime expiry.
    #: (String store, ?Float timeout) -> bool
    def wait_for_rebuild(store, timeout = REBUILD_LOCK_WAIT)
      lock = "#{store}.rebuild"
      deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + timeout
      while Dir.exist?(lock)
        return false if Process.clock_gettime(Process::CLOCK_MONOTONIC) >= deadline

        sleep(0.02)
      end
      true
    end

    # Returns all workspace paths that should be indexed
    #
    #: -> Array[String]
    def workspace_paths
      paths = []
      root = workspace_path

      Dir.each_child(root) do |entry|
        full_path = File.join(root, entry)

        if File.directory?(full_path) || INDEXABLE_EXTENSIONS.include?(File.extname(entry))
          paths << full_path
        end
      end

      add_workspace_dependency_paths(paths)
      add_core_rbs_definition_paths(paths)
      paths.uniq!
      paths
    end

    private

    # Whether the disk-backed (low-resident-memory) index is enabled for this process. The env
    # var wins when set — so CI and one-off runs can flip it without editing the committed
    # configuration — and otherwise the workspace's `[disk_index] enabled` decides.
    #: -> bool
    def disk_index_enabled?
      env = ENV["RUBYDEX_DISK_INDEX"]
      return env == "1" || env == "true" if env

      disk_index_enabled
    end

    # Whether this workspace wants the machine-wide background manager that keeps its store fresh
    # between sessions. The env var wins so CI and one-off runs can flip it without editing the
    # committed configuration.
    #: -> bool
    def index_manager_enabled?
      env = ENV["RUBYDEX_INDEX_MANAGER"]
      return env == "1" || env == "true" if env

      disk_index_manager
    end

    # Path of the on-disk store for this workspace. Location precedence:
    # RUBYDEX_CACHE_DIR (ops override, so CI can point anywhere without touching the repo) >
    # the workspace's committed `[disk_index] location` ("tmp", "global", or an absolute dir) >
    # the workspace's own tmp/ when one exists (the Rails convention; and the right home for a
    # worktree workflow, where each checkout is a different workspace and the store belongs to
    # the one you are in) > the platform cache directory. Stores outside the workspace are named
    # `<workspace-name>-<hash8>` so `du` on the cache root reads as the projects they belong to.
    #: -> String
    def store_cache_path
      File.join(store_cache_dir, "index.redb")
    end

    # Directory holding this workspace's store.
    #: -> String
    def store_cache_dir
      return File.join(ENV["RUBYDEX_CACHE_DIR"], "rubydex", global_store_key) if ENV["RUBYDEX_CACHE_DIR"] && !ENV["RUBYDEX_CACHE_DIR"].empty?

      case disk_index_location
      when "tmp" then workspace_store_dir
      when "global" then platform_store_dir
      when "" then Dir.exist?(File.join(workspace_path, "tmp")) ? workspace_store_dir : platform_store_dir
      else configured_store_dir(disk_index_location)
      end
    end

    # An absolute `location` from the workspace's committed config. The config is workspace-controlled,
    # so the directory is honored only where a store cannot overwrite what another workspace or app owns:
    # inside this workspace, or inside this workspace's own cache directory (`platform_store_dir`). A
    # path anywhere else (including a sibling project's store under `platform_cache_root` or a shared
    # directory like `/tmp`) falls back to the default instead. Users who want an arbitrary machine-local
    # directory can set `RUBYDEX_CACHE_DIR` in the environment.
    #: (String) -> String
    def configured_store_dir(location)
      # `Config` only accepts an absolute path here (a relative one is a config error), so expanding it
      # cannot depend on the process's working directory.
      dir = File.expand_path(location)
      return dir if within?(dir, workspace_path) || within?(dir, platform_store_dir)

      warn("rubydex: ignoring disk_index.location #{location.inspect}: outside the workspace and its cache directory")
      Dir.exist?(File.join(workspace_path, "tmp")) ? workspace_store_dir : platform_store_dir
    end

    # Whether `path` is `root` or inside it, with both expanded so ".." and a trailing separator cannot
    # pass the check. Deliberately not `realpath`: the directory is usually created by the caller.
    #: (String, String) -> bool
    def within?(path, root)
      path = File.expand_path(path)
      root = File.expand_path(root)
      path == root || path.start_with?("#{root}#{File::SEPARATOR}")
    end

    # The workspace's own `tmp/`, which Rails generates and gitignores. rubydex never indexes it
    # (`tmp` is a built-in exclusion), so a store there cannot feed back into the index.
    #: -> String
    def workspace_store_dir
      File.join(workspace_path, "tmp", "rubydex")
    end

    # Name for a store kept outside the workspace: the workspace name leads so `du -sh` on the
    # cache root is readable, the path hash keeps distinct workspaces (and worktrees) apart.
    #: -> String
    def global_store_key
      require "digest"
      "#{File.basename(workspace_path)}-#{Digest::SHA1.hexdigest(File.expand_path(workspace_path))[0, 8]}"
    end

    #: -> String
    def platform_store_dir
      File.join(platform_cache_root, "rubydex", global_store_key)
    end

    # Root for stores of workspaces that don't keep one in their own tmp/: XDG on Linux,
    # ~/Library/Caches on macOS, %USERPROFILE%/.cache elsewhere (Windows' %LOCALAPPDATA% is a
    # registry value, not something the process env reliably carries).
    #: -> String
    def platform_cache_root
      return ENV["XDG_CACHE_HOME"] if ENV["XDG_CACHE_HOME"] && !ENV["XDG_CACHE_HOME"].empty?
      return File.join(Dir.home, "Library", "Caches") if RUBY_PLATFORM.include?("darwin")

      File.join(Dir.home, ".cache")
    end

    # A store inside the workspace would otherwise show up in `git status`, and a `git add -A`
    # would commit a gigabyte of it. Rails' tmp/ is already gitignored, so the entry is only
    # written when nothing ignores the store dir yet; it goes to `.git/info/exclude`, which is
    # untracked, rather than editing the project's `.gitignore`.
    #: (String dir) -> void
    def ensure_store_ignored(dir)
      exclude = File.join(workspace_path, ".git", "info", "exclude")
      return unless File.exist?(exclude)

      root = "#{File.expand_path(workspace_path)}/"
      dir = File.expand_path(dir)
      # A store outside the workspace is not ours to ignore: `git check-ignore` rejects an absolute
      # path outside the repository, and appending it only grew `.git/info/exclude` on every build.
      return unless dir.start_with?(root)

      entry = "#{dir.delete_prefix(root)}/"
      return if system("git", "-C", workspace_path, "check-ignore", "-q", entry, err: File::NULL)

      contents = File.read(exclude)
      File.write(exclude, "#{contents}\n#{entry}") unless contents.lines.map(&:chomp).include?(entry)
    end

    # Signature of everything that can change the index for this workspace: the Gemfile.lock hash
    # (dependency changes) plus a hash of every indexable source file's path/mtime/size under the
    # workspace. The file signature catches source changes for workspaces without a Gemfile.lock too
    # (which would otherwise be treated as fresh forever, since lockfile_hash returns the constant
    # "no-lockfile"). mtime+size is the standard cache heuristic — cheap, no content reads.
    def store_signature(scan = source_scan, lockfile = lockfile_hash)
      require "digest"
      # The layout version is part of the key so a store written by an incompatible layout is
      # invalidated instead of silently degrading against it. The gem version deliberately does not
      # participate: a release that leaves the layout untouched must not force a full re-index.
      Digest::SHA1.hexdigest(
        self.class.store_format_version.to_s + lockfile + workspace_source_signature(scan),
      )
    rescue Errno::ENOENT
      # A file vanished mid-walk; treat the signature as unknown so the store is rebuilt.
      "unknown"
    end

    # The commit this workspace is checked out to, or nil outside a repository. One `git rev-parse`
    # (~1 ms) per freshness check, and the reason the walk can afford a TTL: a commit or branch switch
    # moves HEAD whether or not any file's stamp moved. Deliberately not `git status`: that stats every
    # tracked file, which is the very cost the TTL exists to avoid.
    #
    # -> String?
    def workspace_head
      # stderr is discarded: outside a repository `git rev-parse` prints "fatal: not a git repository"
      # and `capture2` only captures stdout, so that landed in the session's stderr once per freshness
      # check.
      sha, status = Open3.capture2("git", "-C", workspace_path, "rev-parse", "HEAD", err: File::NULL)
      return unless status.success?

      sha = sha.strip
      sha.empty? ? nil : sha
    rescue StandardError
      nil
    end

    # SHA of the workspace Gemfile.lock, used to invalidate the store when dependencies change.
    #: -> String
    def lockfile_hash
      require "digest"
      lock = File.join(workspace_path, "Gemfile.lock")
      File.exist?(lock) ? Digest::SHA1.hexdigest(File.read(lock)) : "no-lockfile"
    end

    # One walk over every indexable Ruby/RBS file under the workspace (excluded directories pruned):
    # workspace-relative path => "mtime_micros:size". Microseconds, not whole seconds: a same-size
    # edit that lands inside the same second is otherwise invisible to the freshness check forever.
    # Feeds both the freshness signature and the surgical diff, so the walk happens once per refresh
    # instead of once per signature.
    #
    # Reused for REFRESH_TTL seconds because it is the expensive part of a tool call; `rewalk:` forces
    # a fresh one, which is what a moved HEAD asks for. RUBYDEX_REFRESH_TTL overrides the window (the
    # tests edit files out of band and expect the next refresh to see them).
    #
    #: (?rewalk: bool) -> Hash[String, String]
    def source_scan(rewalk: false)
      fresh = @scan && (Time.now - @scan_at) < ENV.fetch("RUBYDEX_REFRESH_TTL", REFRESH_TTL).to_f
      return @scan if fresh && !rewalk

      @scan = walk_sources
      @scan_at = Time.now
      @scan
    end

    # The walk itself: workspace-relative path => "mtime_micros:size", excluded directories pruned
    # without descending into them.
    #
    # Measured on 6,441 files: ~105 ms warm, ~0.8 s cold. Settling the wildcard-free
    # built-in exclusions with a Set instead of `File.fnmatch?` per path only buys it down to ~82 ms,
    # which is not worth a second matching rule while the TTL already limits this to one walk per
    # window.
    #
    #: -> Hash[String, String]
    def walk_sources
      require "find"
      root = workspace_path
      excluded = excluded_patterns
      # Excluded patterns are absolute globs anchored at the workspace root (e.g.
      # "/workspace/.git", "/workspace/**/fixtures"), matching the Rust listing's semantics.
      excluded_match = ->(path) { excluded.any? { |pattern| File.fnmatch?(pattern, path, File::FNM_PATHNAME) } }
      scan = {}
      Find.find(root) do |path|
        if File.directory?(path)
          # Prune excluded directories (e.g. .git, node_modules) so Find doesn't descend into them.
          Find.prune if excluded_match.call(path)
          next
        end
        next unless INDEXABLE_EXTENSIONS.include?(File.extname(path))
        next if excluded_match.call(path)

        stat = File.stat(path)
        scan[path.delete_prefix(root + File::SEPARATOR)] = "#{stat.mtime.tv_sec * 1_000_000 + stat.mtime.tv_nsec / 1000}:#{stat.size}"
      end
      scan
    end

    # Digest over the scan. Field order and separators are stable by design; the mtime field gained
    # sub-second resolution, which makes every pre-existing marker mismatch exactly once (one
    # rebuild per workspace) and then hold again.
    #: (Hash[String, String]) -> String
    def workspace_source_signature(scan)
      require "digest"
      digest = Digest::SHA1.new
      scan.each do |rel, stamp|
        mtime, size = stamp.split(":", 2)
        digest.update(rel)
        digest.update("\0")
        digest.update(mtime)
        digest.update("\0")
        digest.update(size)
        digest.update("\0")
      end
      digest.hexdigest
    end

    #: (String) -> bool
    def store_fresh?(cache)
      marker = "#{cache}.hash"
      store_published?(marker, store_signature, cache)
    end

    # Whether `cache` holds the store this session asked for: the marker says it was built from this
    # source, *and* its manifest is readable. A workspace can commit the store, the marker and the
    # sidecar (`[disk_index] location = "tmp"`), so the manifest is also the provenance check — a
    # snapshot without one is rebuilt rather than trusted. Adopting it here is what a caller wants
    # next anyway: the manifest is the baseline a surgical refresh diffs against.
    #: (String marker, String expected_marker, String cache) -> bool
    def store_published?(marker, expected_marker, cache)
      return false unless File.exist?(marker)

      File.read(marker) == expected_marker && adopt_manifest(cache)
    rescue SystemCallError
      # A Windows builder renaming the marker at this instant can fail the read transiently; the
      # caller's poll loop retries on the next tick.
      false
    end

    # Path of the actual `.redb` data file this session is attached to (or that the manifest names),
    # falling back to the logical `index.redb` path for legacy stores.
    #: (?String) -> String
    def active_store_path(cache = store_cache_path)
      @attached_store_path || @manifest_store_path || cache
    end

    # Attaches a store generation and releases our handle on any previous generation, then deletes
    # that previous generation if nothing else holds it open.
    #: (String) -> void
    def attach_store_generation(path)
      previous = @attached_store_path
      attach_store(path)
      @attached_store_path = path
      return unless previous && previous != path && File.basename(previous).match?(GENERATION_STORE_PATTERN)

      begin
        File.delete(previous)
      rescue SystemCallError
        nil
      end
    end

    #: -> void
    def detach_attached_store
      detach_store
      @attached_store_path = nil
    end

    # Renames `src` onto `dst`, retrying briefly on Windows where a reader or AV scanner can hold
    # the short-lived sidecar open for a few milliseconds.
    #: (String, String) -> void
    def rename_with_retry(src, dst)
      attempts = 0
      begin
        File.rename(src, dst)
      rescue SystemCallError
        if Gem.win_platform? && (attempts += 1) < 5
          sleep(0.05 * attempts)
          retry
        end
        raise
      end
    end

    # Best-effort sweep of older generation files (and any legacy `index.redb`) after a new
    # generation is published. Files still held open by another Windows session fail with EACCES
    # and are skipped until that session moves on or the manager's prune pass reaps them.
    #: (String, String) -> void
    def cleanup_stale_generations(cache, keep_basename)
      dir = File.dirname(cache)
      legacy = File.basename(cache)
      Dir.each_child(dir) do |entry|
        next if entry == keep_basename
        next unless entry == legacy || entry.match?(GENERATION_STORE_PATTERN)

        begin
          File.delete(File.join(dir, entry))
        rescue SystemCallError
          nil
        end
      end
    rescue SystemCallError
      nil
    end

    # Builds the store in a short-lived child process (whose peak indexing memory is reclaimed
    # when it exits), then atomically publishes it. The parent never holds the full in-memory index.
    # Uses spawn rather than fork: the parent is a long-lived Ruby process with a warm jemalloc
    # heap and live threads, and a forked child that then allocates heavily corrupts its allocator
    # (SIGSEGV in `tcache_bin_flush` during `index_all`). A fresh process starts with a clean heap.
    #: (String, ?Hash[String, String]) -> void
    def build_store_in_child(cache, scan = source_scan)
      require "fileutils"
      require "rbconfig"
      FileUtils.mkdir_p(File.dirname(cache))
      ensure_store_ignored(File.dirname(cache))
      gen_name = "index.#{SecureRandom.hex(8)}.redb"
      gen_path = File.join(File.dirname(cache), gen_name)
      tmp = "#{cache}.#{Process.pid}.building"
      marker = "#{cache}.hash"
      marker_tmp = "#{marker}.#{Process.pid}.building"
      manifest = "#{cache}.files"
      manifest_tmp = "#{manifest}.#{Process.pid}.building"
      builder = File.expand_path("store_builder.rb", __dir__)
      lockfile = lockfile_hash
      manifest_published = false

      begin
        pid = Process.spawn(RbConfig.ruby, "-I#{File.expand_path("..", __dir__)}", builder, tmp, workspace_path)
        _, status = Process.wait2(pid)
        raise "store build subprocess failed (#{status&.exitstatus})" unless status&.success?
        raise "Gemfile.lock changed while building the store" unless lockfile_hash == lockfile

        # Publish order matters for concurrent correctness: publish the immutable generation file
        # first, then the manifest naming it, then the marker. Because each build writes a fresh
        # `index.<gen>.redb` filename, no reader ever has the destination file open — even on
        # Windows, where replacing an open file fails with EACCES. A reader landing between the
        # manifest and marker renames sees the new manifest with the old marker, mismatches, and
        # refreshes; it never sees a dangling store path or stale content marked fresh.
        # JSON, not Marshal: this file is read back from a directory a workspace can commit, so it must
        # be data a hostile repository cannot turn into an object (see `Graph#adopt_manifest`).
        # `workspace` records which checkout this store belongs to, so the index manager can evict
        # the stores of deleted workspaces (see `rubydex-index-manager`'s prune). Readers ignore
        # unknown keys, so older stores stay readable.
        File.write(
          manifest_tmp,
          JSON.generate({
            "store" => gen_name,
            "workspace" => workspace_path,
            "lockfile" => lockfile,
            "files" => scan,
          }),
        )
        File.write(marker_tmp, store_signature(scan, lockfile))
        rename_with_retry(tmp, gen_path)
        rename_with_retry(manifest_tmp, manifest)
        manifest_published = true
        rename_with_retry(marker_tmp, marker)
        cleanup_stale_generations(cache, gen_name)
      ensure
        # A failed build (or a crash mid-publish) must not leave .building temps or an unreferenced
        # generation behind; after a successful publish all temps were renamed, so these are no-ops.
        File.delete(tmp) if File.exist?(tmp)
        File.delete(marker_tmp) if File.exist?(marker_tmp)
        File.delete(manifest_tmp) if File.exist?(manifest_tmp)
        File.delete(gen_path) if !manifest_published && File.exist?(gen_path)
      end
    end

    # Gathers the paths we have to index for all workspace dependencies
    #: (Array[String]) -> void
    def add_workspace_dependency_paths(paths)
      specs = Bundler.locked_gems&.specs
      return unless specs

      specs.each do |lazy_spec|
        spec = Gem::Specification.find_by_name(lazy_spec.name)
        spec.require_paths.each do |path|
          # For native extensions, RubyGems inserts an absolute require path pointing to
          # `gems/some-gem-1.0.0/extensions`. Those paths don't actually include any Ruby files inside, so we can skip
          # descending them
          next if File.absolute_path?(path)

          paths << File.join(spec.full_gem_path, path)
        end
      rescue Gem::MissingSpecError
        nil
      end
    end

    # Searches for the latest installation of the `rbs` gem and adds the paths for the core and stdlib RBS definitions
    # to the list of paths. This method does not require `rbs` to be a part of the bundle. It searches for whatever
    # latest installation of `rbs` exists in the system and fails silently if we can't find one
    #
    #: (Array[String]) -> void
    def add_core_rbs_definition_paths(paths)
      rbs_gem_path = Gem.path
        .flat_map { |path| Dir.glob(File.join(path, "gems", "rbs-[0-9]*/")) }
        .max_by { |path| Gem::Version.new(File.basename(path).delete_prefix("rbs-")) }

      return unless rbs_gem_path

      paths << File.join(rbs_gem_path, "core")
      paths << File.join(rbs_gem_path, "stdlib")
    end
  end
end
