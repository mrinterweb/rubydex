# frozen_string_literal: true

# The indexer the background manager spawns: it refreshes one store and exits. The
# manager already runs it as a subprocess, and it in turn spawns the store build, so
# the builder's peak memory is reclaimed by a process that exits rather than by the
# daemon. `--full` (appended by the manager for a burst that covers a quarter of the
# manifest) skips the fast paths and rebuilds the store from scratch.

require "rubydex"

workspace, store, *flags = ARGV
# The builder is not a session: it must not register itself with the manager.
ENV["RUBYDEX_INDEX_MANAGER"] = "0"

graph = Rubydex::Graph.configure_for_workspace(workspace)
if flags.include?("--full")
  lock = flags.include?("--claimed") ? "#{store}.rebuild" : graph.send(:claim_rebuild, store)
  exit(0) unless lock

  begin
    graph.send(:build_store_in_child, store)
  ensure
    graph.send(:release_rebuild_lock, lock)
  end
else
  # `store` is the path the manager registered, which need not be the one this process would compute:
  # the manager does not carry the session's `RUBYDEX_CACHE_DIR`.
  graph.refresh_if_stale(store)
  # Bounded: the lock directory is removed by the detached builder's `ensure`, so a builder killed
  # before it could run one would otherwise spin this indexer — and the manager waits on this indexer.
  exit(1) unless graph.send(:wait_for_rebuild, store)
  exit(1) unless graph.send(:store_fresh?, store)
end
