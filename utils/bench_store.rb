# frozen_string_literal: true

# Benchmarks the disk-backed redb store: cold build time, database size (both file size and redb's
# live page footprint), and Ruby query latencies, with optional diffing against a saved baseline.
#
#   bundle exec ruby utils/bench_store.rb <workspace> [options]
#
# Options:
#   --store PATH     benchmark an existing store file instead of building a fresh one
#   --rounds N       query rounds to median over (default: 3)
#   --json           print results as JSON (save this to feed into --compare later)
#   --compare FILE   compare results against a baseline JSON produced with --json
#
# If <workspace> is omitted, DEFAULT_BENCH_WORKSPACE is used. When the workspace has a Gemfile,
# its own Bundler lockfile is used to resolve dependency paths so the benchmark matches what an
# editor session in that project indexes.
#
# Requires a release build of both the CLI and native extension:
#   bundle exec rake compile_release
#   (cd rust && cargo build --release --features redb-store --bin rubydex_cli)

require "bundler"
require "English"
require "fileutils"
require "json"
require "rbconfig"
require "rubydex"
require "tmpdir"

PROJECT_ROOT = File.expand_path("..", __dir__)
CLI_BIN = File.join(PROJECT_ROOT, "rust", "target", "release", "rubydex_cli")
MIB = 1_048_576.0

root = ARGV.first && !ARGV.first.start_with?("--") ? ARGV.shift : ENV["DEFAULT_BENCH_WORKSPACE"]
store_arg = nil
rounds = 3
json_out = false
compare_path = nil

while (arg = ARGV.shift)
  case arg
  when "--store" then store_arg = File.expand_path(ARGV.shift || abort("--store requires a path"))
  when "--rounds" then rounds = Integer(ARGV.shift || abort("--rounds requires a number"))
  when "--json" then json_out = true
  when "--compare" then compare_path = File.expand_path(ARGV.shift || abort("--compare requires a path"))
  else abort("unknown argument: #{arg}")
  end
end

if !store_arg && (!root || root.empty?)
  abort("usage: bundle exec ruby utils/bench_store.rb <workspace> [--store PATH] [--rounds N] [--json] [--compare FILE]")
end

abort("release CLI not found at #{CLI_BIN}\nbuild it with: (cd rust && cargo build --release --features redb-store --bin rubydex_cli)") unless File.executable?(CLI_BIN)

# Resolve workspace + bundled dependency paths using the target workspace's own Gemfile (not
# rubydex's), matching what a live session inside that workspace indexes.
def resolve_workspace_paths(root)
  gemfile = File.join(root, "Gemfile")
  unless File.exist?(gemfile)
    return Rubydex::Graph.configure_for_workspace(root).workspace_paths
  end

  script = <<~RUBY
    require "bundler/setup"
    require "rubydex"
    puts Rubydex::Graph.configure_for_workspace(Dir.pwd).workspace_paths
  RUBY

  out = Bundler.with_unbundled_env do
    IO.popen(
      [{ "BUNDLE_GEMFILE" => gemfile }, RbConfig.ruby, "-I#{File.join(PROJECT_ROOT, "lib")}", "-e", script],
      chdir: root,
      &:read
    )
  end
  abort("failed to resolve workspace paths for #{root}") unless $CHILD_STATUS.success?

  out.lines.map(&:chomp).reject(&:empty?)
end

def parse_cli_stats(output)
  stats = {}
  stats["files"] = Integer(Regexp.last_match(1)) if output =~ /^Indexed (\d+) files$/
  stats["declarations"] = Integer(Regexp.last_match(1)) if output =~ /^Found (\d+) names$/
  stats["definitions"] = Integer(Regexp.last_match(1)) if output =~ /^Found (\d+) definitions$/

  {
    "listing_s" => /^  Listing\s+([\d.]+)s/,
    "indexing_s" => /^  Indexing\s+([\d.]+)s/,
    "resolution_s" => /^  Resolution\s+([\d.]+)s/,
    "store_build_s" => /^  Store build\s+([\d.]+)s/,
    "total_build_s" => /^  Total:\s+([\d.]+)s/,
    "peak_rss_mb" => /^Maximum RSS:\s+\d+ bytes \(([\d.]+) MB\)/,
  }.each do |key, pattern|
    stats[key] = Float(Regexp.last_match(1)) if output =~ pattern
  end

  {
    "rows" => /^  Total rows:\s+(\d+)/,
    "leaf_pages" => /^  Total leaf pages:\s+(\d+)/,
    "stored_bytes" => /^  Stored bytes:\s+(\d+)/,
    "fragmented_bytes" => /^  Fragmented bytes:\s+(\d+)/,
    "metadata_bytes" => /^  Metadata bytes:\s+(\d+)/,
    "live_footprint_bytes" => /^  Live footprint:\s+(\d+)/,
    "file_bytes" => /^  File size:\s+(\d+)/,
  }.each do |key, pattern|
    stats[key] = Integer(Regexp.last_match(1)) if output =~ pattern
  end

  stats
end

def run_cli(*args)
  out = IO.popen([CLI_BIN, *args], err: [:child, :out], &:read)
  abort("rubydex_cli failed:\n#{out}") unless $CHILD_STATUS.success?

  out
end

def median(values)
  sorted = values.sort
  sorted[sorted.size / 2]
end

def time_query(iterations, &block)
  t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  iterations.times(&block)
  ((Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0) * 1000.0 / iterations).round(3)
end

def benchmark_queries(store_path, rounds)
  graph = Rubydex::Graph.new
  graph.attach_store(store_path)

  fuzzy_matches = graph.fuzzy_search("arbs").count
  samples = Hash.new { |h, k| h[k] = [] }

  rounds.times do
    samples["search_exact_ms"] << time_query(20) { graph.search("String").to_a }
    samples["search_fuzzy_ms"] << time_query(20) { graph.fuzzy_search("arbs").to_a }
    samples["resolve_constant_ms"] << time_query(200) { graph.resolve_constant("Object", []) }
    samples["comments_hover_ms"] << time_query(50) do
      definition = graph["Object"]&.definitions&.first
      definition&.comments
    end
    samples["require_paths_ms"] << time_query(10) { graph.require_paths([]) }
  end

  result = { "search_fuzzy_matches" => fuzzy_matches }
  samples.each { |key, values| result[key] = median(values) }
  result
end

def format_value(value, unit)
  return "—" if value.nil?

  case unit
  when :mib then format("%.1f MiB", value / MIB)
  when :sec then format("%.3f s", value)
  when :ms then format("%.3f ms", value)
  when :mb then format("%.1f MB", value)
  else value.to_s
  end
end

def format_delta(current, baseline)
  return "" if current.nil? || baseline.nil? || baseline == 0

  pct = ((current - baseline).to_f / baseline) * 100.0
  format("  (vs %+.1f%%)", pct)
end

tmpdir = nil
store_path = store_arg

begin
  results = {}

  if store_path
    abort("store file not found: #{store_path}") unless File.exist?(store_path)

    results["store"] = store_path
    results.merge!(parse_cli_stats(run_cli("--stats", "--open-store", store_path)))
  else
    root = File.expand_path(root)
    abort("workspace not found: #{root}") unless Dir.exist?(root)

    tmpdir = Dir.mktmpdir("rubydex-bench-store-")
    store_path = File.join(tmpdir, "index.redb")
    paths = resolve_workspace_paths(root)

    results["workspace"] = root
    results["workspace_paths"] = paths.size
    results.merge!(parse_cli_stats(run_cli("--stats", "--build-store", store_path, *paths)))
  end

  results["file_bytes"] ||= File.size(store_path)
  results.merge!(benchmark_queries(store_path, rounds))

  if json_out
    puts JSON.pretty_generate(results)
  else
    baseline = compare_path ? JSON.parse(File.read(compare_path)) : {}
    rows = [
      ["Files indexed", "files", :int],
      ["Declarations", "declarations", :int],
      ["Definitions", "definitions", :int],
      ["Store file size", "file_bytes", :mib],
      ["Live footprint", "live_footprint_bytes", :mib],
      ["  stored", "stored_bytes", :mib],
      ["  fragmented", "fragmented_bytes", :mib],
      ["  metadata", "metadata_bytes", :mib],
      ["Leaf pages", "leaf_pages", :int],
      ["B-tree rows", "rows", :int],
      ["Store build", "store_build_s", :sec],
      ["Total cold build", "total_build_s", :sec],
      ["Peak RSS", "peak_rss_mb", :mb],
      ["search exact (String)", "search_exact_ms", :ms],
      ["search fuzzy (arbs)", "search_fuzzy_ms", :ms],
      ["resolve_constant", "resolve_constant_ms", :ms],
      ["comments (hover)", "comments_hover_ms", :ms],
      ["require_paths", "require_paths_ms", :ms],
    ]

    puts "workspace: #{results["workspace"] || results["store"]}"
    puts "comparing against: #{compare_path}" if compare_path
    rows.each do |label, key, unit|
      val = results[key]
      next if val.nil?

      base = baseline[key]
      delta = compare_path ? format_delta(val, base) : ""
      base_col = compare_path && base ? format("  [was %s]", format_value(base, unit)) : ""
      puts format("%-24s %12s%s%s", label, format_value(val, unit), base_col, delta)
    end
  end
ensure
  FileUtils.rm_rf(tmpdir) if tmpdir
end
