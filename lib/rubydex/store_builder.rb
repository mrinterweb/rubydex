# frozen_string_literal: true

# Builds a disk index in a short-lived process, so the server's peak memory stays low and the
# indexing memory is reclaimed when this process exits. Invoked by `Graph#build_store_in_child`:
#   ruby -I<lib> store_builder.rb <store_path> <workspace_path>
require "rubydex"

store_path, workspace = ARGV

graph = Rubydex::Graph.configure_for_workspace(workspace)
graph.index_all(graph.workspace_paths)
graph.resolve
graph.build_store(store_path)
