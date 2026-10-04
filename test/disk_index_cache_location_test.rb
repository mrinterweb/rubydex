# frozen_string_literal: true

require "test_helper"
require "tmpdir"
require "fileutils"

# Where the store lives is decided by: RUBYDEX_CACHE_DIR > the workspace's committed
# `[disk_index] location` > the workspace's own tmp/ when one exists (the Rails convention) >
# the platform cache directory. The store dir is named so `du` on the global root is readable.
class DiskIndexCacheLocationTest < Minitest::Test
  def with_workspace(config: nil, tmp: false)
    Dir.mktmpdir do |dir|
      File.write(File.join(dir, "rubydex.toml"), config) if config
      FileUtils.mkdir_p(File.join(dir, "tmp")) if tmp
      graph = Rubydex::Graph.configure_for_workspace(dir)
      yield(graph, dir)
    end
  end

  def test_tmp_location_puts_the_store_in_the_workspace
    with_workspace(config: "[disk_index]\nlocation = \"tmp\"\n") do |graph, dir|
      assert_equal(File.join(dir, "tmp", "rubydex", "index.redb"), graph.send(:store_cache_path))
    end
  end

  def test_global_location_uses_the_platform_cache_dir
    with_workspace(config: "[disk_index]\nlocation = \"global\"\n") do |graph, dir|
      store = graph.send(:store_cache_path)
      refute(store.start_with?(dir.to_s), "global must leave the workspace")
      assert(store.end_with?("/index.redb"))
    end
  end

  def test_an_absolute_location_is_used_as_written
    with_workspace(config: "[disk_index]\nlocation = \"/var/tmp/rubydex-indexes\"\n") do |graph, _dir|
      assert_equal("/var/tmp/rubydex-indexes/index.redb", graph.send(:store_cache_path))
    end
  end

  def test_the_context_default_uses_the_workspace_tmp_when_one_exists
    with_workspace(tmp: true) do |graph, dir|
      assert_equal(File.join(dir, "tmp", "rubydex", "index.redb"), graph.send(:store_cache_path))
    end
  end

  def test_the_context_default_falls_back_to_the_platform_cache_dir
    with_workspace do |graph, dir|
      refute(graph.send(:store_cache_path).start_with?(dir.to_s), "no tmp/: the store stays out of the workspace")
    end
  end

  def test_the_global_store_dir_is_named_so_du_is_readable
    with_workspace do |graph, dir|
      store = graph.send(:store_cache_path)
      key = File.basename(File.dirname(store))

      assert(key.start_with?("#{File.basename(dir)}-"), "the workspace name leads so du output means something")
      assert_match(/-[0-9a-f]{8}\z/, key, "the path hash keeps distinct workspaces apart")
    end
  end

  def test_rubydex_cache_dir_overrides_everything
    with_workspace(config: "[disk_index]\nlocation = \"tmp\"\n", tmp: true) do |graph, _dir|
      ENV["RUBYDEX_CACHE_DIR"] = "/opt/rubydex-cache"
      begin
        store = graph.send(:store_cache_path)
        assert(store.start_with?("/opt/rubydex-cache/rubydex/"), "the env root wins over everything else")
        assert(store.end_with?("/index.redb"))
      ensure
        ENV.delete("RUBYDEX_CACHE_DIR")
      end
    end
  end

  def test_the_committed_config_enables_the_disk_index
    with_workspace(config: "[disk_index]\nenabled = true\n") do |graph, _dir|
      assert(graph.send(:disk_index_enabled?), "a committed section opts the workspace in, no env var")
    end
  end

  def test_the_env_var_overrides_the_committed_config
    with_workspace(config: "[disk_index]\nenabled = true\n") do |graph, _dir|
      ENV["RUBYDEX_DISK_INDEX"] = "0"
      begin
        refute(graph.send(:disk_index_enabled?), "a one-off run can turn it off without touching the repo")
      ensure
        ENV.delete("RUBYDEX_DISK_INDEX")
      end
    end
  end

  def test_a_store_in_an_unignored_tmp_gets_excluded_from_git
    Dir.mktmpdir do |dir|
      FileUtils.mkdir_p(File.join(dir, "tmp"))
      system("git", "init", dir, out: File::NULL)
      graph = Rubydex::Graph.configure_for_workspace(dir)
      store_dir = File.join(dir, "tmp", "rubydex")

      graph.send(:ensure_store_ignored, store_dir)
      exclude = File.join(dir, ".git", "info", "exclude")

      assert_includes(File.read(exclude), "tmp/rubydex/", "a GB store must not be committable by accident")
    end
  end

  def test_an_already_ignored_tmp_is_left_alone
    Dir.mktmpdir do |dir|
      FileUtils.mkdir_p(File.join(dir, "tmp"))
      system("git", "init", dir, out: File::NULL)
      File.write(File.join(dir, ".gitignore"), "tmp\n")
      graph = Rubydex::Graph.configure_for_workspace(dir)

      graph.send(:ensure_store_ignored, File.join(dir, "tmp", "rubydex"))
      exclude = File.join(dir, ".git", "info", "exclude")

      refute_includes(File.read(exclude), "tmp/rubydex/", "Rails already ignores tmp/: don't touch the repo")
    end
  end
end
