# frozen_string_literal: true

require "fileutils"
require "open3"
require "rubydex/index_manager"

module Rubydex
  # The global graph representing all declarations and their relationships for the workspace
  #
  # Note: this class is partially defined in C to integrate with the Rust backend
  class Graph
    INDEXABLE_EXTENSIONS = [".rb", ".rake", ".rbs", ".ru"].freeze
    # A rebuild lock older than this is treated as abandoned (a builder that crashed) and
    # reclaimed, so a directory can never wedge itself out of rebuilding forever.
    REBUILD_LOCK_TIMEOUT = 30 * 60

    # Above this fraction of changed files a full rebuild (one child, a fresh store shared by every
    # session) is cheaper than growing each session's overlay. Measured on reserv-api: surgical 200
    # files = 71 ms; full rebuild = 24.6 s.
    REBUILD_DIFF_RATIO = 0.25

    # How stale the freshness walk may go, in seconds. The walk stats every indexable file (~105 ms
    # warm, ~0.9 s cold, on reserv's 6,441 files) and runs before every tool call, so it is reused
    # inside this window. Owner decision 2026-08-10: an edit made outside the editor (sed, a formatter,
    # a script) may be answered up to this stale. A moved HEAD bounds the case that matters (a branch
    # switch or a commit rewrites files in bulk) and the editor path is unaffected, because it indexes
    # its own saves. RUBYDEX_REFRESH_TTL overrides it.
    REFRESH_TTL = 2

    # Once the overlay would hold this fraction of the workspace's files, a rebuild is the cheaper
    # answer. A surgical apply leaves the edited files in the overlay and only a rebuild compacts it,
    # so a session that absorbs many small branch switches converges on holding the whole index in
    # memory: ~12 MB per 1,000 files measured on reserv, against ~5 MB for the store it opened. Same
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
    # the whole resolved graph in a FORKED child so its peak indexing memory is reclaimed when the
    # child exits, then attach the store to this graph. The long-lived server therefore holds the
    # bulk index off-heap and serves reads from disk. Falls back to the in-memory path if the store
    # can't be built.
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
      build_store_in_child(cache) unless File.exist?(cache) && store_fresh?(cache)
      attach_store(cache)
      return index_all(workspace_paths) if quarantine_if_untrustworthy(cache)

      adopt_manifest(cache)
      @session_signature = session_state(@head_sha, source_scan)
      register_session(cache) if index_manager_enabled?

      []
    rescue StandardError, NotImplementedError => e
      # NotImplementedError (from `fork`) is < ScriptError, not < StandardError, so list it
      # explicitly — otherwise the gem raises on every call on Windows, where fork is unavailable.
      warn("rubydex: disk-backed index unavailable (#{e.class}: #{e.message}); falling back to in-memory")
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
      signature = session_state(head, scan)
      return false if @session_signature == signature

      marker = "#{cache}.hash"
      if File.exist?(marker) && File.read(marker) == signature
        attach_store(cache)
        adopt_manifest(cache)
      elsif (changes = surgical_changes(scan))
        apply_changes(changes)
        @applied_files = scan
      else
        lock = claim_rebuild(cache)
        return false unless lock

        begin
          build_store_in_child(cache, scan)
        ensure
          Dir.rmdir(lock) if File.exist?(lock)
        end
        attach_store(cache)
        adopt_manifest(cache)
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
      detach_store
      FileUtils.mv(cache, "#{cache}.corrupt", force: true)
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
      # Detaching empties the graph, and `refresh_if_stale` would then report "nothing changed" and
      # leave the session answering with nothing, so the fallback is the same one `index_workspace`
      # takes at boot.
      if quarantine_if_untrustworthy
        index_all(workspace_paths)
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
        # *roots* (53 on reserv, whose manifest is 6,441 files), which trips that threshold about 100x
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
      removed = @applied_files.keys - scan.keys
      return if (changed.size + removed.size) > (scan.size * REBUILD_DIFF_RATIO)
      return if overlay_divergence(scan) > (scan.size * OVERLAY_COMPACTION_RATIO)

      { changed: changed, removed: removed }
    end

    # What the overlay would hold after applying `scan`: every file whose stamp moved away from what
    # the attached store was built from, plus what the store holds that no longer exists. Only a
    # rebuild (or a re-attach) resets it.
    #: (Hash[String, String]) -> Integer
    def overlay_divergence(scan)
      scan.count { |rel, stamp| @manifest_files[rel] != stamp } + (@manifest_files.keys - scan.keys).size
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
    # A store predating manifests has no sidecar, so the first stale refresh rebuilds and writes one.
    #: (String) -> void
    def adopt_manifest(cache)
      manifest = Marshal.load(File.binread("#{cache}.files"))
      # What the attached store holds, kept apart from `@applied_files` (what this session is serving)
      # so the overlay can be measured against the store it sits on.
      @manifest_files = manifest["files"]
      @applied_files = @manifest_files
      @applied_lockfile = manifest["lockfile"]
    rescue Errno::ENOENT
      @manifest_files = nil
      @applied_files = nil
    end

    # Create-exclusive rebuild lock: `Dir.mkdir` raises `Errno::EEXIST` atomically, so N
    # stale sessions (two editors plus an agent server on one directory) pay one rebuild. A lock
    # left by a crashed builder expires on mtime instead of wedging the directory forever.
    #: (String cache) -> (String | false)
    def claim_rebuild(cache)
      lock = "#{cache}.rebuild"
      begin
        Dir.mkdir(lock)
      rescue Errno::EEXIST
        return false unless File.exist?(lock) && (Time.now - File.mtime(lock)) > REBUILD_LOCK_TIMEOUT

        # Reclaiming is racy: a competing session may have reclaimed and re-created the lock
        # between the check and here, in which case this session lost and someone else is
        # building. A build legitimately slower than REBUILD_LOCK_TIMEOUT is reclaimed too, so
        # two builders can run on one store; the publish is atomic and the marker decides which
        # store a session reads.
        begin
          Dir.rmdir(lock)
          Dir.mkdir(lock)
        rescue Errno::ENOENT, Errno::EEXIST
          return false
        end
      end
      lock
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
      else disk_index_location
      end
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

      entry = "#{dir.delete_prefix("#{File.expand_path(workspace_path)}/")}/"
      return if system("git", "-C", workspace_path, "check-ignore", "-q", entry)

      File.write(exclude, File.read(exclude) + "\n" + entry)
    end

    # Signature of everything that can change the index for this workspace: the Gemfile.lock hash
    # (dependency changes) plus a hash of every indexable source file's path/mtime/size under the
    # workspace. The file signature catches source changes for workspaces without a Gemfile.lock too
    # (which would otherwise be treated as fresh forever, since lockfile_hash returns the constant
    # "no-lockfile"). mtime+size is the standard cache heuristic — cheap, no content reads.
    def store_signature(scan = source_scan)
      require "digest"
      # The layout version is part of the key so a store written by an incompatible layout is
      # invalidated instead of silently degrading against it. The gem version deliberately does not
      # participate: a release that leaves the layout untouched must not force a full re-index.
      Digest::SHA1.hexdigest(
        self.class.store_format_version.to_s + lockfile_hash + workspace_source_signature(scan),
      )
    rescue Errno::ENOENT
      # A file vanished mid-walk; treat the signature as unknown so the store is rebuilt.
      "unknown"
    end

    # What a session is serving: the store's content plus the commit it read it at. HEAD belongs here
    # and deliberately not in `store_signature` — a commit rewrites no files, so the shared store is
    # still exactly right and only this session has to notice, while a marker keyed on HEAD would throw
    # away a good store after every commit.
    #
    #: (String?, Hash[String, String]) -> String
    def session_state(head, scan)
      "#{head}#{store_signature(scan)}"
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
    # Measured on reserv (6,441 files): ~105 ms warm, ~0.8 s cold. Settling the wildcard-free
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
      File.exist?(marker) && File.read(marker) == store_signature
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
      tmp = "#{cache}.#{Process.pid}.building"
      marker = "#{cache}.hash"
      marker_tmp = "#{marker}.#{Process.pid}.building"
      manifest = "#{cache}.files"
      manifest_tmp = "#{manifest}.#{Process.pid}.building"
      builder = File.expand_path("store_builder.rb", __dir__)

      begin
        pid = Process.spawn(RbConfig.ruby, "-I#{File.expand_path("..", __dir__)}", builder, tmp, workspace_path)
        _, status = Process.wait2(pid)
        raise "store build subprocess failed (#{status&.exitstatus})" unless status&.success?

        # Publish order matters for concurrent correctness: rename the store first, then the marker.
        # Both renames are atomic on POSIX, so a concurrent reader (attach_store) never sees a
        # partially-written file. A reader that lands between the two renames sees a NEW store with an
        # OLD marker, so store_fresh? compares the old marker to the new signature, mismatches, and
        # rebuilds — a wasted rebuild, never staleness (a stale store can only be served when the
        # marker says fresh, which requires the new marker, which is written last). Concurrent builders
        # use per-pid temps, so they never clobber each other's build; the second rename simply wins.
        # The scan is captured BEFORE the child runs, so a file edited during the build is marked
        # stale and re-diffed by the next refresh rather than wrongly served as fresh.
        File.binwrite(manifest_tmp, Marshal.dump({ "lockfile" => lockfile_hash, "files" => scan }))
        File.write(marker_tmp, store_signature(scan))
        File.rename(tmp, cache)
        File.rename(manifest_tmp, manifest)
        File.rename(marker_tmp, marker)
      ensure
        # A failed build (or a crash mid-publish) must not leave .building temps behind; after a
        # successful publish all three were renamed, so these are no-ops.
        File.delete(tmp) if File.exist?(tmp)
        File.delete(marker_tmp) if File.exist?(marker_tmp)
        File.delete(manifest_tmp) if File.exist?(manifest_tmp)
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
