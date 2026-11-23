# frozen_string_literal: true

require "rubydex/cli/command"

module Rubydex
  module CLI
    # `rdx mcp [PATH]` — runs the MCP server for AI assistants. The workspace defaults to the
    # current directory.
    class Command
      class Mcp < Command
        command "mcp"
        arguments "[PATH]"
        summary "Run the MCP server for AI assistants (workspace defaults to the current dir)"

        #: -> void
        def run
          parse_options!

          path = argv.shift || Dir.pwd
          abort_with_usage("unexpected argument: #{argv.first}") unless argv.empty?

          # The MCP server manages its own graph lifecycle for the given workspace, so it does not go
          # through `build_graph`.
          require "rubydex/mcp_server"
          if !ENV.key?("RUBYDEX_DISK_INDEX") && Rubydex::Graph.store_format_version.positive?
            ENV["RUBYDEX_DISK_INDEX"] = "1"
          end
          Rubydex::MCPServer.run(path)
        end
      end
    end
  end
end
