# frozen_string_literal: true

require "test_helper"
require "tmpdir"
require "fileutils"
require "json"

# A workspace that opts into `[disk_index] manager` registers itself with the machine-wide
# manager when it indexes, so the manager can keep the store fresh between sessions.
class DiskIndexManagerSessionTest < Minitest::Test
  MANAGER_BIN = File.expand_path("../lib/rubydex/rubydex-index-manager", __dir__)

  def with_workspace(config)
    Dir.mktmpdir do |dir|
      File.write(File.join(dir, "rubydex.toml"), config)
      FileUtils.mkdir_p(File.join(dir, "lib"))
      File.write(File.join(dir, "lib", "app.rb"), "class App\nend\n")
      graph = Rubydex::Graph.configure_for_workspace(dir)
      graph.index_workspace
      yield(graph, dir)
    end
  end

  def test_an_opted_in_session_gets_registered
    with_workspace("[disk_index]\nenabled = true\nmanager = true\n") do |graph, dir|
      registry = File.join(graph.send(:platform_cache_root), "rubydex", "manager", "sessions")
      sessions = JSON.parse(%x(#{MANAGER_BIN} --registry #{registry} --list))
      mine = sessions.select { |session| session["workspace"] == dir }

      assert_equal(1, mine.size, "the live session must be listed exactly once")
    end
  end

  def test_a_workspace_that_stays_out_of_the_manager_leaves_no_session
    with_workspace("[disk_index]\nenabled = true\n") do |graph, dir|
      registry = File.join(graph.send(:platform_cache_root), "rubydex", "manager", "sessions")
      sessions = JSON.parse(%x(#{MANAGER_BIN} --registry #{registry} --list))

      assert_equal(
        [],
        sessions.select { |session| session["workspace"] == dir },
        "no opt-in means no session registered",
      )
    end
  end
end
