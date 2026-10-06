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
      build_store_in_child(cache) unless File.exist?(cache) && store_fresh?(cache)
      attach_store(cache)
      return index_all(workspace_paths) if quarantine_untrustworthy_store(cache)

      adopt_manifest(cache)
      @session_signature = store_signature
      register_session(cache) if index_manager_enabled?

      []
    rescue StandardError, NotImplementedError => e
      # NotImplementedError (from `fork`) is < ScriptError, not < StandardError, so list it
      # explicitly — otherwise the gem raises on every call on Windows, where fork is unavailable.
      warn("rubydex: disk-backed index unavailable (#{e.class}: #{e.message}); falling back to in-memory")
      index_all(workspace_paths)
    end

    # Refreshes this session onto the newest snapshot for its directory. Sessions never
    # coordinate: the freshness marker is compared against what this session attached, and the
    # first session to claim the create-exclusive rebuild lock spawns the one rebuild child — a
    # branch switch is a filesystem event no session owns, so the signature is what notices. redb
    # pins a snapshot at open, so every session must re-attach to see a new store; the rest find
    # the marker fresh and pay only the reopen (~71 ms measured, vs ~25 s for the rebuild).
    #: -> bool
    # Git fast path for "what changed since the store was built". Far cheaper than
    # walking the tree (a scoped `git diff` is ~10ms vs ~70ms to stat the tree), but
    # it is only a fast path: returns nil when git cannot answer completely, i.e.
    # when the workspace is not a git checkout, when the checkout is not clean
    # (untracked/edited files are invisible to `git diff`), or when `from_sha` is
    # not an ancestor of HEAD. Paths are returned workspace-relative.
    def git_changed_files(workspace, from_sha)
      git_dir, status = Open3.capture2("git", "-C", workspace, "rev-parse", "--git-dir")
      git_dir = git_dir.strip
      return unless status.success? && !git_dir.empty?

      root = File.expand_path(git_dir, File.expand_path(workspace))
      root = root.sub(%r{/\.git\z}, "")
      clean, status = Open3.capture2("git", "-C", root, "status", "--porcelain")
      return unless status.success? && clean.strip.empty?

      head, status = Open3.capture2("git", "-C", root, "rev-parse", "HEAD")
      head = head.strip
      return unless status.success?

      return if head == from_sha

      # A scoped diff keeps a workspace inside a larger repo honest; git reports
      # paths relative to the repo root, so translate them back.
      root_path = root
      ws_path = File.expand_path(workspace)
      scope = if ws_path == root_path
        "."
      elsif ws_path.start_with?("#{root_path}/")
        ws_path[root_path.length + 1..]
      else
        return
      end
      listing, status = Open3.capture2("git", "-C", root, "diff", "--name-only", "#{from_sha}..#{head}", "--", scope)
      return unless status.success?

      prefix = scope == "." ? "" : "#{scope}/"
      listing.lines.map(&:strip).reject(&:empty?).map do |path|
        return nil unless prefix.empty? || path.start_with?(prefix)

        path[prefix.length..]
      end
    rescue StandardError
      nil
    end

    # Moves this session to the current source state, cheapest route first: a fresh marker means
    # nothing changed; else a small diff is applied into the overlay (no rebuild); else the store is
    # rebuilt once for everyone. Returns true when the session moved.
    def refresh_if_stale
      cache = store_cache_path
      scan = source_scan
      signature = store_signature(scan)
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

    # Makes this session visible to the machine-wide manager and starts the manager when none is
    # live. The registry handle stays referenced for the process lifetime, which is what keeps the
    # session's lock held and the session listed. Sessions stay correct without the manager: this
    # is an optimization, never a dependency.
    #: (String cache) -> nil
    def register_session(cache)
      registry = File.join(platform_cache_root, "rubydex", "manager", "sessions")
      @session_handle = Rubydex::IndexManager.register(
        workspace: workspace_path,
        store: cache,
        builder: Rubydex::IndexManager.builder_argv(workspace_path, cache),
        docs: workspace_paths.size,
        registry: registry,
      )
      Rubydex::IndexManager.launch_if_absent(registry)
    end

    # Files to re-index/delete to move this session from @applied_files to `scan`, or nil when a full
    # rebuild is required (no manifest, lockfile changed, or the diff exceeds REBUILD_DIFF_RATIO).
    #: (Hash[String, String]) -> Hash[Symbol, Array[String]]?
    def surgical_changes(scan)
      return unless @applied_files && @applied_lockfile == lockfile_hash

      changed = scan.filter_map { |rel, stamp| rel if @applied_files[rel] != stamp }
      removed = @applied_files.keys - scan.keys
      return if (changed.size + removed.size) > (scan.size * REBUILD_DIFF_RATIO)

      { changed: changed, removed: removed }
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
      @applied_files = manifest["files"]
      @applied_lockfile = manifest["lockfile"]
    rescue Errno::ENOENT
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
      Digest::SHA1.hexdigest(self.class.store_format_version.to_s + lockfile_hash + workspace_source_signature(scan))
    rescue Errno::ENOENT
      # A file vanished mid-walk; treat the signature as unknown so the store is rebuilt.
      "unknown"
    end

    # SHA of the workspace Gemfile.lock, used to invalidate the store when dependencies change.
    #: -> String
    def lockfile_hash
      require "digest"
      lock = File.join(workspace_path, "Gemfile.lock")
      File.exist?(lock) ? Digest::SHA1.hexdigest(File.read(lock)) : "no-lockfile"
    end

    # One walk over every indexable Ruby/RBS file under the workspace (excluded directories pruned):
    # workspace-relative path => "mtime:size". Feeds both the freshness signature and the surgical
    # diff, so the walk happens once per refresh instead of once per signature.
    #: -> Hash[String, String]
    def source_scan
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
        scan[path.delete_prefix(root + File::SEPARATOR)] = "#{stat.mtime.to_i}:#{stat.size}"
      end
      scan
    end

    # Digest over the scan, field order and separators unchanged from the pre-scan walk so markers
    # written by older releases stay valid.
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

    # Moves an untrustworthy store out of the cache path; returns true when it did. A store that
    # failed to decode (corrupt, or written by an incompatible layout) must not keep answering
    # queries with holes. Quarantining drops the freshness marker with it, so the next run rebuilds
    # instead of re-reading the same bad file.
    #: (String cache) -> bool
    def quarantine_untrustworthy_store(cache)
      return false unless respond_to?(:store_errors) && store_errors.positive?

      warn("rubydex: disk-backed index returned errors; quarantining the store and indexing in memory")
      FileUtils.mv(cache, "#{cache}.corrupt", force: true)
      FileUtils.rm_f("#{cache}.hash")
      true
    end

    # Builds the store in a short-lived child process (whose peak indexing memory is reclaimed
    # when it exits), then atomically publishes it. The parent never holds the full in-memory index.
    # Uses spawn rather than fork: the parent is a long-lived Ruby process with a warm jemalloc
    # heap and live threads, and a forked child that then allocates heavily corrupts its allocator
    # (SIGSEGV in `tcache_bin_flush` during `index_all`). A fresh process starts with a clean heap.
    #: (String) -> void
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
