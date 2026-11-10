# frozen_string_literal: true

require "json"
require "open3"
require "bundler/gem_tasks"
require "rubocop/rake_task"
require "rake/extensiontask"
require "rake/testtask"
require "rdoc/task"

GEMSPEC = Gem::Specification.load("rubydex.gemspec")

Rake::ExtensionTask.new("rubydex", GEMSPEC) do |ext|
  ext.lib_dir = "lib/rubydex"
end

test_config = lambda do |t|
  t.libs << "test"
  t.libs << "lib"
  t.ruby_opts << ["--enable=frozen_string_literal"]
  t.test_files = FileList["test/**/*_test.rb"]
end
Rake::TestTask.new(ruby_test: :compile, &test_config)

begin
  require "ruby_memcheck"
  namespace(:ruby_test) do
    # Ruby 3.4+ reports every leak valgrind sees in every process it traces, and `--trace-children`
    # is on by default: the suite starts a compiler (extconf test) and child interpreters, and their
    # own leaks were the whole report (233 KB from `cc1` alone), with ours buried under them.
    # Naming the extension restricts reporting to stacks that pass through it. Leaks in the Rust
    # library still surface: whatever allocates them is called from the extension, so its frame is
    # always below them.
    memcheck = RubyMemcheck::Configuration.new(binary_name: "rubydex")
    RubyMemcheck::TestTask.new(memcheck, valgrind: :compile) do |task|
      test_config.call(task)
      # Valgrind runs with --trace-children=yes and --leak-check=full, which costs ~1000x: the
      # whole suite is hours per sanitizer. Keep the tests that exercise the native surface on
      # small workspaces; the ones that index a real workspace stay on the plain ruby_test task.
      #
      # The rule is "spawns a process, or boots a store in a child": those children are what valgrind
      # instruments, and test/disk_index_surgical_refresh_test.rb joining this run unnoticed is how the
      # memleak job went from ~2.5 to ~6 hours (94 s of plain-ruby store builds, every child traced).
      task.test_files = FileList["test/**/*_test.rb"].exclude(
        "test/integration/**/*_test.rb",
        "test/cli_test.rb",
        "test/disk_index_session_refresh_test.rb",
        "test/disk_index_corrupt_store_test.rb",
        "test/disk_index_manager_session_test.rb",
        "test/disk_index_live_edit_test.rb",
        "test/disk_index_surgical_refresh_test.rb",
        "test/disk_index_cypher_test.rb",
      )
    end
  end
rescue LoadError
  # ruby_memcheck is not available on Windows
end

RuboCop::RakeTask.new

RDoc::Task.new do |doc|
  doc.rdoc_dir = "_site"
end

task :lint do
  puts "******** Linting ********\n"
  Rake::Task["rubocop"].invoke
  Rake::Task["lint_rust"].invoke
end

task :format do
  puts "******** Formatting ********\n"
  Rake::Task["rubocop:autocorrect"].invoke
  Rake::Task["format_rust"].invoke
end

# Enhance the clean task to also clean Rust artifacts
Rake::Task[:clean].enhance([:clean_rust])

desc "Build the background index manager next to the extension"
task :index_manager do
  binary = Gem.win_platform? ? "rubydex-index-manager.exe" : "rubydex-index-manager"
  sh "cargo build --release -p rubydex-index-manager", chdir: "rust"
  cp "rust/target/release/#{binary}", "lib/rubydex/#{binary}"
end

Rake::Task[:compile].enhance([:index_manager])

desc "Generate the Ruby classes for the graph's built-in rules"
task :generate_rules do
  sh "cargo run --quiet --bin generate_ruby_rules", chdir: "rust"
end

task compile: :generate_rules

task compile_release: :clean do
  ENV["RELEASE"] = "true"
  Rake::Task[:compile].invoke
end

desc "Sync the gem version and crate dependency with the Cargo workspace version"
task :sync_versions do
  metadata, status = Open3.capture2(
    "cargo", "metadata", "--no-deps", "--offline", "--format-version=1", "--manifest-path=rust/Cargo.toml"
  )
  abort "Could not read the Cargo workspace version" unless status.success?

  packages = JSON.parse(metadata).fetch("packages")
  rubydex_package = packages.find { |package| package.fetch("name") == "rubydex" }
  abort "Could not find rubydex in Cargo metadata" unless rubydex_package

  cargo_version = rubydex_package.fetch("version")
  gem_version = cargo_version.sub(/-beta\.(\d+)\z/) { ".beta#{Regexp.last_match(1)}" }

  replacements = {
    "lib/rubydex/version.rb" => [/^  VERSION = "[^"]+"$/, "  VERSION = #{gem_version.inspect}"],
    "rust/rubydex-sys/Cargo.toml" => [/^rubydex = \{ version = "[^"]+"/, "rubydex = { version = \"=#{cargo_version}\""],
  }

  updated_files = replacements.to_h do |path, (pattern, replacement)|
    contents = File.read(path)
    abort("Could not find the version in #{path}") unless contents.sub!(pattern, replacement)

    [path, contents]
  end

  updated_files.each do |path, contents|
    File.write(path, contents)
  end
end

task test: [:cargo_test, :ruby_test]
task check: [:lint, :test]

task default: :check
