# frozen_string_literal: true

require "tmpdir"
require "fileutils"
require "rbconfig"
require "open3"
require "test_helper"

# The generated Makefile's cargo invocation is the contract for which Cargo features ship in the
# compiled extension. This runs extconf.rb against a copy of the extension directory and reads it.
class ExtconfFeaturesTest < Minitest::Test
  def generate_makefile(env = {})
    Dir.mktmpdir do |dir|
      ext_dir = File.join(dir, "rubydex")
      FileUtils.cp_r("ext/rubydex", ext_dir)

      output, status = Dir.chdir(ext_dir) do
        Open3.capture2e(env, RbConfig.ruby, "extconf.rb")
      end
      flunk("extconf failed: #{output.lines.last}") unless status.success?

      File.read(File.join(ext_dir, "Makefile"))
    end
  end

  def test_redb_store_feature_is_compiled_in_by_default
    makefile = generate_makefile
    assert_includes(
      makefile,
      "--features rubydex-sys/redb-store",
      "the fork exists for the disk index; the store feature must be on without any env var",
    )
  end

  def test_redb_store_feature_opts_out
    makefile = generate_makefile("RUBYDEX_NO_REDB_STORE" => "1")
    refute_includes(makefile, "--features rubydex-sys/redb-store")
  end
end
