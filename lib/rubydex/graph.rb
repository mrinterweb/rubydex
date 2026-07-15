# frozen_string_literal: true

module Rubydex
  # The global graph representing all declarations and their relationships for the workspace
  #
  # Note: this class is partially defined in C to integrate with the Rust backend
  class Graph
    INDEXABLE_EXTENSIONS = [".rb", ".rake", ".rbs", ".ru"].freeze

    #: (?workspace_path: String?) -> void
    def initialize(workspace_path: nil)
      self.workspace_path = workspace_path if workspace_path
    end

    # Index all files and dependencies of the workspace that exists in `workspace_path`.
    #
    # Disk-backed orchestration (opt-in via RUBYDEX_DISK_INDEX=1): build (or reuse) a redb store of
    # the whole resolved graph in a FORKED child so its peak indexing memory is reclaimed when the
    # child exits, then attach the store to this graph. The long-lived server therefore holds the
    # bulk index off-heap and serves reads from disk. Falls back to the in-memory path if the store
    # can't be built.
    #
    # The default is the in-memory path: the disk-backed path is read-only today (the FFI
    # short-circuits drop live `index_source` edits against a store-backed graph), so it must stay
    # opt-in until live-edit integration (Stage 4) lands.
    #: -> Array[String]
    def index_workspace
      return index_all(workspace_paths) unless disk_index_enabled?

      cache = store_cache_path
      build_store_via_fork(cache) unless File.exist?(cache) && store_fresh?(cache)
      attach_store(cache)
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

    # Whether the disk-backed (low-resident-memory) index is enabled for this process. Opt-in via
    # RUBYDEX_DISK_INDEX=1; the default keeps the in-memory path so live edits keep working (the
    # store-backed path is read-only until Stage 4 lands live-edit integration).
    #: -> bool
    def disk_index_enabled?
      ENV["RUBYDEX_DISK_INDEX"] == "1" || ENV["RUBYDEX_DISK_INDEX"] == "true"
    end

    # Path of the on-disk store for this workspace, namespaced by workspace path. Honors
    # XDG_CACHE_HOME (and a RUBYDEX_CACHE_DIR override) instead of hardcoding ~/.cache, and falls
    # back to ~/.cache only when neither is set and a home directory exists.
    #: -> String
    def store_cache_path
      require "digest"
      key = Digest::SHA1.hexdigest(File.expand_path(@workspace_path))
      File.join(cache_root, "rubydex", key, "index.redb")
    end

    # Root directory for on-disk stores. RUBYDEX_CACHE_DIR > XDG_CACHE_HOME > ~/.cache.
    #: -> String
    def cache_root
      return ENV["RUBYDEX_CACHE_DIR"] if ENV["RUBYDEX_CACHE_DIR"] && !ENV["RUBYDEX_CACHE_DIR"].empty?
      return ENV["XDG_CACHE_HOME"] if ENV["XDG_CACHE_HOME"] && !ENV["XDG_CACHE_HOME"].empty?
      File.join(Dir.home, ".cache")
    end

    # Signature of everything that can change the index for this workspace: the Gemfile.lock hash
    # (dependency changes) plus a hash of every indexable source file's path/mtime/size under the
    # workspace. The file signature catches source changes for workspaces without a Gemfile.lock too
    # (which would otherwise be treated as fresh forever, since lockfile_hash returns the constant
    # "no-lockfile"). mtime+size is the standard cache heuristic — cheap, no content reads.
    #: -> String
    def store_signature
      require "digest"
      Digest::SHA1.hexdigest(lockfile_hash + workspace_source_signature)
    end

    # SHA of the workspace Gemfile.lock, used to invalidate the store when dependencies change.
    #: -> String
    def lockfile_hash
      require "digest"
      lock = File.join(@workspace_path, "Gemfile.lock")
      File.exist?(lock) ? Digest::SHA1.hexdigest(File.read(lock)) : "no-lockfile"
    end

    # Hash over the path/mtime/size of every indexable Ruby/RBS file under the workspace (excluding
    # ignored directories). Detects source changes between runs without reading file contents.
    #: -> String
    def workspace_source_signature
      require "digest"
      require "find"
      digest = Digest::SHA1.new
      Find.find(@workspace_path) do |path|
        next if File.directory?(path)
        next unless INDEXABLE_EXTENSIONS.include?(File.extname(path))
        # Skip ignored directories anywhere in the tree.
        rel = path.delete_prefix(@workspace_path + File::SEPARATOR)
        next if rel.split(File::SEPARATOR).any? { |seg| IGNORED_DIRECTORIES.include?(seg) }
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

    # Builds the store in a forked child (whose peak indexing memory is reclaimed on exit), then
    # atomically publishes it. The parent never holds the full in-memory index.
    #: (String) -> void
    def build_store_via_fork(cache)
      raise NotImplementedError, "fork is unavailable on this platform" unless Process.respond_to?(:fork)

      require "fileutils"
      FileUtils.mkdir_p(File.dirname(cache))
      tmp = "#{cache}.#{Process.pid}.building"

      pid = fork do
        builder = Rubydex::Graph.new(workspace_path: @workspace_path)
        builder.index_all(builder.workspace_paths)
        builder.resolve
        builder.build_store(tmp)
        exit!(0)
      end
      _, status = Process.wait2(pid)
      raise "store build subprocess failed (#{status&.exitstatus})" unless status&.success?

      # Publish order matters for concurrent correctness: rename the store first, then the marker.
      # Both renames are atomic on POSIX, so a concurrent reader (attach_store) never sees a
      # partially-written file. A reader that lands between the two renames sees a NEW store with an
      # OLD marker, so store_fresh? compares the old marker to the new signature, mismatches, and
      # rebuilds — a wasted rebuild, never staleness (a stale store can only be served when the
      # marker says fresh, which requires the new marker, which is written last). Concurrent builders
      # use per-pid temps, so they never clobber each other's build; the second rename simply wins.
      marker = "#{cache}.hash"
      marker_tmp = "#{marker}.#{Process.pid}.building"
      File.write(marker_tmp, store_signature)
      File.rename(tmp, cache)
      File.rename(marker_tmp, marker)
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
