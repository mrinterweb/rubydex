# frozen_string_literal: true

# The indexer the background manager spawns: it refreshes one store and exits. The
# manager already runs it as a subprocess, so the store build forks inside it only to
# keep the builder's peak memory off the daemon's resident set. `--full` (appended by
# the manager for a burst that covers a quarter of the manifest) skips the fast paths
# and rebuilds the store from scratch.

require "rubydex"

workspace, store, *flags = ARGV
# The builder is not a session: it must not register itself with the manager.
ENV["RUBYDEX_INDEX_MANAGER"] = "0"

graph = Rubydex::Graph.configure_for_workspace(workspace)
if flags.include?("--full")
  graph.send(:claim_rebuild, store)
  graph.send(:build_store_in_child, store)
else
  graph.refresh_if_stale
end
