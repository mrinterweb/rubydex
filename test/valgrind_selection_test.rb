# frozen_string_literal: true

require "test_helper"

# The memleak job runs the suite under valgrind with `--trace-children=yes`. A traced child costs
# ~1000x, and a child killed mid-report leaves a truncated XML file that ruby_memcheck cannot parse,
# so the memleak task skips any test file whose header declares the opt-out. The header is the only
# thing that decides, which means a spawn-heavy test that forgets it silently rejoins the traced run
# and the job fails hours later with a parse error. This test is what notices instead.
# See CONTRIBUTING.md, "Tests under valgrind".
class ValgrindSelectionTest < Minitest::Test
  ROOT = File.expand_path("..", __dir__)
  MARKER = "# valgrind: skip"
  # The memleak task looks at this many lines, so a marker below them is invisible to it.
  HEADER_LINES = 10

  # A test file that can start a child process: our own store builder or manager, `git`, a compiler,
  # the CLI, or the workspace setup that shells out. `index_workspace` only spawns when the disk
  # index is on, which is why the env assignment is part of the pattern.
  SPAWNING = %r{
    Process\.spawn | Process\.kill | Open3\. | \bsystem\( |
    build_store_in_child | ENV\["RUBYDEX_DISK_INDEX"\]\s*=\s*"1"
  }x

  def test_every_test_file_that_can_spawn_a_child_opts_out_of_valgrind
    unmarked = test_files.reject { |file| marked?(file) }.select { |file| spawns?(file) }

    assert_empty(
      unmarked,
      "these files can spawn a child but do not declare `#{MARKER}` in their first #{HEADER_LINES} " \
        "lines, so the memleak job would trace those children: #{unmarked.sort.join(", ")}",
    )
  end

  def test_the_spawners_are_the_files_the_memleak_job_skips
    marked = test_files.select { |file| marked?(file) }

    assert_operator(
      marked.size,
      :>,
      5,
      "the marker is the only thing that keeps spawners out of the traced run; finding #{marked.size} " \
        "suggests the header convention was dropped rather than that nothing spawns",
    )
  end

  private

  def test_files
    Dir.glob(File.join(ROOT, "test/**/*_test.rb"))
      .reject { |file| file.delete_prefix("#{ROOT}/").start_with?("test/integration/") }
      # This file names the patterns it looks for, so it matches itself; it spawns nothing.
      .reject { |file| file == File.expand_path(__FILE__) }
  end

  def marked?(file)
    File.foreach(file).first(HEADER_LINES).any? { |line| line.start_with?(MARKER) }
  end

  def spawns?(file)
    File.read(file).match?(SPAWNING)
  end
end
