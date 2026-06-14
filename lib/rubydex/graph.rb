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
    # Disk-backed orchestration: build (or reuse) a redb store of the whole resolved graph in a
    # FORKED child so its peak indexing memory is reclaimed when the child exits, then attach the
    # store to this graph. The long-lived server therefore holds the bulk index off-heap and serves
    # reads from disk. Falls back to the in-memory path if the store can't be built.
    #: -> Array[String]
    def index_workspace
      cache = store_cache_path
      build_store_via_fork(cache) unless File.exist?(cache) && store_fresh?(cache)
      attach_store(cache)
      []
    rescue StandardError => e
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

    # Path of the on-disk store for this workspace, namespaced by workspace path.
    #: -> String
    def store_cache_path
      require "digest"
      key = Digest::SHA1.hexdigest(File.expand_path(@workspace_path))
      File.join(Dir.home, ".cache", "rubydex", key, "index.redb")
    end

    # SHA of the workspace Gemfile.lock, used to invalidate the store when dependencies change.
    #: -> String
    def lockfile_hash
      require "digest"
      lock = File.join(@workspace_path, "Gemfile.lock")
      File.exist?(lock) ? Digest::SHA1.hexdigest(File.read(lock)) : "no-lockfile"
    end

    #: (String) -> bool
    def store_fresh?(cache)
      marker = "#{cache}.hash"
      File.exist?(marker) && File.read(marker) == lockfile_hash
    end

    # Builds the store in a forked child (whose peak indexing memory is reclaimed on exit), then
    # atomically publishes it. The parent never holds the full in-memory index.
    #: (String) -> void
    def build_store_via_fork(cache)
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

      File.rename(tmp, cache)
      File.write("#{cache}.hash", lockfile_hash)
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
