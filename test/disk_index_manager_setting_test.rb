# frozen_string_literal: true

require "test_helper"
require "tmpdir"

# The background manager is opt-in: `[disk_index] manager = true` opts a workspace in, and
# RUBYDEX_INDEX_MANAGER lets CI or a one-off run flip it without touching the repo.
class DiskIndexManagerSettingTest < Minitest::Test
  def with_workspace(config, env: nil)
    Dir.mktmpdir do |dir|
      File.write(File.join(dir, "rubydex.toml"), config) if config
      ENV["RUBYDEX_INDEX_MANAGER"] = env if env
      begin
        yield(Rubydex::Graph.configure_for_workspace(dir))
      ensure
        ENV.delete("RUBYDEX_INDEX_MANAGER") if env
      end
    end
  end

  def test_the_committed_config_opts_a_workspace_into_the_manager
    with_workspace("[disk_index]\nenabled = true\nmanager = true\n") do |graph|
      assert(graph.send(:index_manager_enabled?), "the workspace asked for the background manager")
    end
  end

  def test_the_manager_stays_off_until_asked_for
    with_workspace("[disk_index]\nenabled = true\n") do |graph|
      refute(graph.send(:index_manager_enabled?), "no manager without an explicit opt-in")
    end
  end

  def test_the_env_var_overrides_the_committed_config
    with_workspace("[disk_index]\nenabled = true\nmanager = true\n", env: "0") do |graph|
      refute(graph.send(:index_manager_enabled?), "a one-off run can leave the manager out")
    end
  end

  def test_a_build_without_the_store_feature_never_enables_disk_indexing
    with_workspace("[disk_index]\nenabled = true\n") do |graph|
      assert(graph.send(:disk_index_enabled?), "the committed config asks for a store")
      without_store_feature do
        refute(graph.send(:disk_index_enabled?), "a build without redb-store cannot write or open one")
      end
    end
  end

  # Alias-restore, not define+remove: `store_format_version` is plain Ruby here but the pattern keeps
  # the class intact for whatever other suite runs next in the same process.
  def without_store_feature
    klass = Rubydex::Graph.singleton_class
    klass.alias_method(:store_format_version_original_for_stub, :store_format_version)
    klass.define_method(:store_format_version) { 0 }
    yield
  ensure
    klass.alias_method(:store_format_version, :store_format_version_original_for_stub)
    klass.remove_method(:store_format_version_original_for_stub)
  end
end
