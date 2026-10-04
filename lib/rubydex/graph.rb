# frozen_string_literal: true

require "fileutils"

module Rubydex
  # The global graph representing all declarations and their relationships for the workspace
  #
  # Note: this class is partially defined in C to integrate with the Rust backend
  class Graph
    INDEXABLE_EXTENSIONS = [".rb", ".rake", ".rbs", ".ru"].freeze

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
      build_store_via_fork(cache) unless File.exist?(cache) && store_fresh?(cache)
      attach_store(cache)
      return index_all(workspace_paths) if quarantine_untrustworthy_store(cache)

      []
    rescue StandardError, NotImplementedError => e
      # NotImplementedError (from `fork`) is < ScriptError, not < StandardError, so list it
      # explicitly — otherwise the gem raises on every call on Windows, where fork is unavailable.
      warn("rubydex: disk-backed index unavailable (#{e.class}: #{e.message}); falling back to in-memory")
      index_all(workspace_paths)
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
    def store_signature
      require "digest"
      # The layout version is part of the key so a store written by an incompatible layout is
      # invalidated instead of silently degrading against it. The gem version deliberately does not
      # participate: a release that leaves the layout untouched must not force a full re-index.
      Digest::SHA1.hexdigest(self.class.store_format_version.to_s + lockfile_hash + workspace_source_signature)
    end

    # SHA of the workspace Gemfile.lock, used to invalidate the store when dependencies change.
    #: -> String
    def lockfile_hash
      require "digest"
      lock = File.join(workspace_path, "Gemfile.lock")
      File.exist?(lock) ? Digest::SHA1.hexdigest(File.read(lock)) : "no-lockfile"
    end

    # Hash over the path/mtime/size of every indexable Ruby/RBS file under the workspace (excluding
    # ignored directories). Detects source changes between runs without reading file contents.
    #: -> String
    def workspace_source_signature
      require "digest"
      require "find"
      digest = Digest::SHA1.new
      root = workspace_path
      excluded = excluded_patterns
      # Excluded patterns are absolute globs anchored at the workspace root (e.g.
      # "/workspace/.git", "/workspace/**/fixtures"), matching the Rust listing's semantics.
      excluded_match = ->(path) { excluded.any? { |pattern| File.fnmatch?(pattern, path, File::FNM_PATHNAME) } }
      Find.find(root) do |path|
        if File.directory?(path)
          # Prune excluded directories (e.g. .git, node_modules) so Find doesn't descend into them.
          Find.prune if excluded_match.call(path)
          next
        end
        next unless INDEXABLE_EXTENSIONS.include?(File.extname(path))
        next if excluded_match.call(path)

        rel = path.delete_prefix(root + File::SEPARATOR)
        stat = File.stat(path)
        digest.update(rel)
        digest.update("\0")
        digest.update(stat.mtime.to_i.to_s)
        digest.update("\0")
        digest.update(stat.size.to_s)
        digest.update("\0")
      end
      digest.hexdigest
    rescue Errno::ENOENT
      # A file vanished mid-walk; treat the signature as unknown so the store is rebuilt.
      "unknown"
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
    def build_store_via_fork(cache)
      require "fileutils"
      require "rbconfig"
      FileUtils.mkdir_p(File.dirname(cache))
      ensure_store_ignored(File.dirname(cache))
      tmp = "#{cache}.#{Process.pid}.building"
      marker = "#{cache}.hash"
      marker_tmp = "#{marker}.#{Process.pid}.building"
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
        File.write(marker_tmp, store_signature)
        File.rename(tmp, cache)
        File.rename(marker_tmp, marker)
      ensure
        # A failed build (or a crash mid-publish) must not leave .building temps behind; after a
        # successful publish both were renamed, so these are no-ops.
        File.delete(tmp) if File.exist?(tmp)
        File.delete(marker_tmp) if File.exist?(marker_tmp)
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
