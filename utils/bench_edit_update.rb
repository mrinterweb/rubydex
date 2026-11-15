# frozen_string_literal: true

# Benchmarks the disk index's *update* paths, which is what a long-lived editor session pays:
#
#   bundle exec ruby utils/bench_edit_update.rb <workspace> [--files N] [--rounds N] [--json]
#
# Phases:
#
#   cold build        no store -> build one (index + resolve + write)
#   start (warm)      new process, store matches the tree -> attach only
#   edit N, new proc  edit N files, then start a new process (what the next session pays)
#   edit N, in place  a session that already holds the store applies N edits (refresh_if_stale)
#
# The edits append a `# rubydex-bench` line and are reverted afterwards, so the workspace ends
# byte-identical. Run it against a copy if the workspace is one you care about.
#
# `bundle exec` is not optional: a bare `ruby -Ilib` resolves `rubydex` through RubyGems and
# benchmarks a globally installed build instead of this checkout.
require "rubydex"
require "fileutils"
require "json"
require "rbconfig"

MARKER = "# rubydex-bench"
TTL = 2.2 # REFRESH_TTL is 2s; the scan memo has to expire for a refresh to see the edit

root = ARGV.first && !ARGV.first.start_with?("--") ? ARGV.shift : nil
files = 1
rounds = 3
json_out = false
child_mode = nil
while (arg = ARGV.shift)
  case arg
  when "--files" then files = Integer(ARGV.shift)
  when "--rounds" then rounds = Integer(ARGV.shift)
  when "--json" then json_out = true
  when "--child" then child_mode = ARGV.shift
  when "--root" then root = ARGV.shift
  else abort("unknown argument: #{arg}")
  end
end
root = File.expand_path(root || abort("usage: bench_edit_update.rb <workspace> [--files N] [--rounds N] [--json]"))

ENV["RUBYDEX_DISK_INDEX"] = "1"
ENV["RUBYDEX_INDEX_MANAGER"] = "0" # the manager would race the measurements

def editable_files(root)
  Dir.glob(File.join(root, "**", "*.rb")).reject do |path|
    path.include?("/vendor/") || path.include?("/node_modules/") || path.include?("/tmp/rubydex/") ||
      path.include?("/.git/")
  end.sort
end

def append_marker(paths)
  paths.each { |path| File.open(path, "a") { |f| f.puts(MARKER) } }
end

def remove_marker(paths)
  paths.each { |path| File.write(path, File.readlines(path).reject { |l| l.chomp == MARKER }.join) }
end

# ---------------------------------------------------------------- child: one measured action

if child_mode
  graph = Rubydex::Graph.configure_for_workspace(root)
  case child_mode
  when "start"
    graph.index_workspace
  when "refresh"
    graph.index_workspace # attach (fast when the store matches)
    targets = editable_files(root).first(Integer(ENV.fetch("BENCH_FILES")))
    append_marker(targets)
    sleep(TTL)
    t = Process.clock_gettime(Process::CLOCK_MONOTONIC)
    moved = graph.refresh_if_stale
    ms = ((Process.clock_gettime(Process::CLOCK_MONOTONIC) - t) * 1000).round(1)
    remove_marker(targets)
    puts JSON.generate({ moved: moved, ms: ms })
  else
    abort("unknown child mode: #{child_mode}")
  end
  exit(0)
end

# ---------------------------------------------------------------- parent

def timed
  t = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  yield
  ((Process.clock_gettime(Process::CLOCK_MONOTONIC) - t) * 1000).round(1)
end

def child(root, mode, files: 1)
  out = IO.popen([{ "BENCH_FILES" => files.to_s }, RbConfig.ruby, __FILE__, "--root", root, "--child", mode], &:read)
  line = out.lines.last.to_s
  line.empty? ? {} : JSON.parse(line)
end

graph = Rubydex::Graph.configure_for_workspace(root)
store = graph.send(:store_cache_path)
source_files = editable_files(root)
abort("no Ruby files under #{root}") if source_files.empty?

results = { "workspace" => root, "files" => source_files.size, "store" => store }
FileUtils.rm_f([store, "#{store}.hash", "#{store}.files"])
results["cold_build_ms"] = timed { child(root, "start") }
results["store_mb"] = File.exist?(store) ? (File.size(store) / 1_048_576.0).round(1) : nil
results["start_warm_ms"] = timed { child(root, "start") }

# What the *next* session pays after N files changed while no session was running.
targets = source_files.first(files)
append_marker(targets)
results["next_session_after_#{files}_edits_ms"] = timed { child(root, "start") }
remove_marker(targets)

# What a session that already holds the store pays. The first round absorbs whatever rebuild the
# reverted tree needs, so it is a warm-up and is reported separately.
warmup = child(root, "refresh", files: files)["ms"]
results["in_place_warmup_ms"] = warmup
times = Array.new(rounds) { child(root, "refresh", files: files)["ms"] }
results["in_place_#{files}_edits_ms"] = times
results["in_place_#{files}_edits_median_ms"] = times.sort[times.size / 2]

if json_out
  puts JSON.generate(results)
else
  puts "workspace:                 #{root} (#{results["files"]} .rb files)"
  puts "store:                     #{store} (#{results["store_mb"]} MB)"
  puts "cold build:                #{results["cold_build_ms"]} ms"
  puts "start, store fresh:        #{results["start_warm_ms"]} ms"
  puts "next session, #{files} edit(s):     #{results["next_session_after_#{files}_edits_ms"]} ms"
  puts "in place, #{files} edit(s) warmup:  #{warmup} ms"
  puts "in place, #{files} edit(s)/round:   #{times.join(", ")} ms (median #{results["in_place_#{files}_edits_median_ms"]})"
end
