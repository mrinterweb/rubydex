# frozen_string_literal: true

# Session profile: RSS + latency of a long-lived Ruby process using the graph API,
# disk-store-backed vs in-memory. Usage:
#   ruby -Ilib docs/disk-persisted-index/session-profile.rb <corpus> <store> [disk|memory]
#
# <corpus> is the workspace to index (e.g. $(ruby -e 'print RbConfig::CONFIG["rubylibdir"]')/..).
# <store> is a store built from that corpus, which disk mode copies into its cache:
#   cargo run --manifest-path rust/Cargo.toml --release --features redb-store -- \
#     --bin rubydex_cli -- --build-store <store> <corpus>
#
# IMPORTANT: build the release extension first (`bundle exec rake compile_release`) — against the
# debug .so the same workload reads ~10x slower, and every number below is a release number.
require "rubydex"
require "fileutils"

corpus = File.expand_path(ARGV.fetch(0))
store_src = ARGV.fetch(1)
mode = ARGV[2] || "disk"

def rss_kb
  File.read("/proc/self/status")[/^VmRSS:\s+(\d+) kB/, 1].to_i
end

def hwm_kb
  File.read("/proc/self/status")[/^VmHWM:\s+(\d+) kB/, 1].to_i
end

g = Rubydex::Graph.configure_for_workspace(corpus)

if mode == "disk"
  cache = g.send(:store_cache_path)
  FileUtils.mkdir_p(File.dirname(cache))
  FileUtils.cp(store_src, cache)
  File.write("#{cache}.hash", g.send(:store_signature))
end

t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
g.index_workspace
boot = Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0

puts "mode=#{mode} boot=#{boot.round(2)}s disk_index=#{g.send(:disk_index_enabled?)}"
puts "after boot: rss=#{rss_kb / 1024}MB hwm=#{hwm_kb / 1024}MB"

SEARCH_PREFIXES = ["enum", "ma", "ra", "ac", "en", "ha", "st"].freeze
CONSTANTS = ["Enumerable", "Hash", "String", "Ractor", "Fiber", "Kernel", "Module", "Object", "BasicObject", "Struct"].freeze
REQUIRES = ["set", "json", "pathname", "ractor", "fiber", "tmpdir"].freeze

# warmup
5.times do
  g.search(SEARCH_PREFIXES[0]) { |_d| }
end

def run_phase(label, n, &op)
  t0 = Process.clock_gettime(Process::CLOCK_MONOTONIC)
  errors = 0
  results = 0
  n.times do |i|
    r = op.call(i)
    results += r.respond_to?(:size) ? [r.size, 1].max : 1
  rescue StandardError
    errors += 1
  end
  dt = Process.clock_gettime(Process::CLOCK_MONOTONIC) - t0
  printf(
    "%-28s %6.2f ms/op  %5d results  %3d errors  rss=%dMB hwm=%dMB\n",
    label,
    dt / n * 1000,
    results,
    errors,
    rss_kb / 1024,
    hwm_kb / 1024,
  )
end

run_phase("search (completion)", 200) do |i|
  c = 0
  g.search(SEARCH_PREFIXES[i % SEARCH_PREFIXES.size]) { |_d| c += 1 }
  c
end
run_phase("resolve_constant (hover)", 200) { |i| g.resolve_constant(CONSTANTS[i % CONSTANTS.size], []) ? 1 : 0 }
run_phase("resolve_require_path", 200) { |i| g.resolve_require_path(REQUIRES[i % REQUIRES.size], []) ? 1 : 0 }
run_phase("require_paths (full)", 20) { |_i| g.require_paths([corpus]).size }

puts "final: rss=#{rss_kb / 1024}MB hwm=#{hwm_kb / 1024}MB"
