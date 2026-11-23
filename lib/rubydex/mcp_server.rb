# frozen_string_literal: true

require "json"

require "rubydex"
require "rubydex/mcp_server/protocol"

Dir[File.join(__dir__, "mcp_server", "tools", "*_tool.rb")].sort.each { |file| require file }

module Rubydex
  module MCPServer
    class Server
      WORKER_COUNT = 4
      SHUTDOWN_JOIN_TIMEOUT = 2

      PREFIX = "Tools query a static index of workspace code, available bundle dependencies, " \
        "and available Ruby core and standard-library RBS definitions."

      # An out-of-band edit (a formatter, `sed`, a script) is only seen if the session re-checks the
      # tree, which is exactly what disk-backed indexing buys and in-memory indexing does not. The
      # instructions have to say which one is running, or an agent is promised a refresh it cannot get.
      DISK_BACKED_INSTRUCTIONS = <<~TEXT
        #{PREFIX}
        Out-of-band edits are checked before every tool call, and one is picked up once the check expires (#{Graph::REFRESH_TTL} seconds) or immediately when the commit moves, so a query can answer from a slightly stale index. A full rebuild may take longer than 100 ms; during it a usable previous snapshot can answer until a later call attaches the new store.
      TEXT

      IN_MEMORY_INSTRUCTIONS = <<~TEXT
        #{PREFIX}
        Edits arrive through the editor's own save path: a write made outside the editor (a formatter, `sed`, a script) is not seen until the session restarts.
      TEXT

      #: (root_path: String, ?transport: StdioTransport) -> void
      def initialize(root_path:, transport: nil)
        @root_path = root_path
        @transport = transport
        @graph = Graph.configure_for_workspace(@root_path)
        @graph_mutex = Mutex.new
        @index_finished = false
        @incoming_queue = Thread::Queue.new
        @outgoing_queue = Thread::Queue.new
        @workers = []
        @outgoing_dispatcher = nil
        @indexer = nil
      end

      attr_reader :root_path

      #: -> Thread
      def spawn_indexer
        @indexer = Thread.new do
          @graph.index_workspace
          @graph.resolve
          @index_finished = true
        end
      end

      #: -> void
      def main_loop
        @workers = Array.new(WORKER_COUNT) { new_worker }
        @outgoing_dispatcher = Thread.new do
          while (response = @outgoing_queue.pop)
            @transport.write(response)
          end
        end

        @transport.open do |request, parse_error|
          if parse_error
            send_message(parse_error)
          else
            @incoming_queue << request
          end
        end
      ensure
        run_shutdown
        @transport.close
      end

      #: (Hash | Array | untyped) -> Hash | Array[Hash]?
      def handle(request)
        if request.is_a?(Array)
          return JSONRPC.error_response(nil, JSONRPC::INVALID_REQUEST, "Invalid Request", data: "Request is an empty array") if request.empty?

          responses = request.filter_map { |entry| handle(entry) }
          return responses if responses.any?

          return
        end

        unless request.is_a?(Hash)
          return JSONRPC.error_response(nil, JSONRPC::INVALID_REQUEST, "Invalid Request", data: "Request must be a hash")
        end

        has_id = request.key?(:id)
        id = request[:id]
        method = request[:method]
        params = request[:params]

        unless request[:jsonrpc] == "2.0"
          return JSONRPC.error_response(nil, JSONRPC::INVALID_REQUEST, "Invalid Request", data: "JSON-RPC version must be 2.0")
        end

        unless !has_id || id.is_a?(Integer) || (id.is_a?(String) && id.match?(/\A[a-zA-Z0-9_-]+\z/))
          return JSONRPC.error_response(nil, JSONRPC::INVALID_REQUEST, "Invalid Request", data: "Request ID must be a string or integer")
        end

        unless method.is_a?(String) && !method.start_with?("rpc.")
          return JSONRPC.error_response(nil, JSONRPC::INVALID_REQUEST, "Invalid Request", data: 'Method name must be a string and not start with "rpc."')
        end

        unless params.nil? || params.is_a?(Hash)
          return JSONRPC.error_response(id, JSONRPC::INVALID_PARAMS, "Invalid params", data: "Method parameters must be an object or null")
        end

        result = case method
        when "initialize"
          {
            protocolVersion: "2025-03-26",
            capabilities: { tools: {} },
            serverInfo: {
              name: "rubydex_mcp",
              version: Rubydex::VERSION,
            },
            instructions: instructions,
          }
        when "tools/list"
          { tools: Tool.tools.map(&:to_h) }
        when "tools/call"
          call_tool(params || {})
        when "ping"
          {}
        when "notifications/initialized"
          return
        else
          return has_id ? JSONRPC.error_response(id, JSONRPC::METHOD_NOT_FOUND, "Method not found", data: method) : nil
        end

        has_id ? { jsonrpc: "2.0", id: id, result: result } : nil
      rescue KeyError => e
        has_id ? JSONRPC.error_response(id, JSONRPC::INVALID_PARAMS, "Invalid params", data: e.message) : nil
      rescue StandardError => e
        has_id ? JSONRPC.error_response(id, JSONRPC::INTERNAL_ERROR, "Internal error", data: e.message) : nil
      end

      #: -> Graph | Error
      def graph_or_error
        return @graph if @index_finished

        Error.new(
          "indexing",
          "Rubydex is still indexing the codebase",
          "The server is starting up. Please retry in a few seconds.",
        )
      end

      private

      #: -> String
      def instructions
        @graph.send(:disk_index_enabled?) ? DISK_BACKED_INSTRUCTIONS : IN_MEMORY_INSTRUCTIONS
      end

      #: -> void
      def run_shutdown
        @incoming_queue.close unless @incoming_queue.closed?
        join_or_kill_all(@workers + [@indexer])
        @outgoing_queue.close unless @outgoing_queue.closed?
        join_or_kill_all([@outgoing_dispatcher])
      end

      #: (Array[Thread?]) -> void
      def join_or_kill_all(threads)
        live_threads = threads.compact.select(&:alive?)
        return if live_threads.empty?

        deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + SHUTDOWN_JOIN_TIMEOUT
        live_threads.each do |thread|
          remaining = deadline - Process.clock_gettime(Process::CLOCK_MONOTONIC)
          break if remaining <= 0

          thread.join(remaining)
        end

        # `kill` is a request, not an await: a thread parked in a blocking call, or one that defers
        # interrupts, may not die promptly, and joining it without a bound is how the server fails to
        # exit at all. Ruby ends the process with the thread still parked; what must not happen is
        # waiting for it.
        interrupted = live_threads.select(&:alive?)
        interrupted.each(&:kill)

        kill_deadline = Process.clock_gettime(Process::CLOCK_MONOTONIC) + SHUTDOWN_JOIN_TIMEOUT
        interrupted.each do |thread|
          remaining = kill_deadline - Process.clock_gettime(Process::CLOCK_MONOTONIC)
          break if remaining <= 0

          thread.join(remaining)
        end
      end

      #: -> Thread
      def new_worker
        Thread.new do
          while (request = @incoming_queue.pop)
            send_message(handle(request))
          end
        end
      end

      #: (Hash | Array[Hash]?) -> void
      def send_message(response)
        return unless response
        return if @outgoing_queue.closed?

        @outgoing_queue << response
      end

      #: (Hash) -> Hash
      def call_tool(params)
        tool_name = params.fetch(:name)
        tool = Tool.tools_by_name.fetch(tool_name, nil)
        raise KeyError, "Tool not found: #{tool_name}" unless tool

        arguments = params[:arguments] || {}
        known_arguments = tool.input_schema.fetch(:properties).keys.map(&:to_s)
        unknown_arguments = arguments.keys.map(&:to_s) - known_arguments
        unless unknown_arguments.empty?
          return Tool::Response.new(
            [{ type: "text", text: "Unknown arguments: #{unknown_arguments.join(", ")}" }],
            error: true,
          ).to_h
        end

        missing_arguments = Array(tool.input_schema[:required]) - arguments.keys.map(&:to_s)
        unless missing_arguments.empty?
          return Tool::Response.new(
            [{ type: "text", text: "Missing required arguments: #{missing_arguments.join(", ")}" }],
            error: true,
          ).to_h
        end

        graph = graph_or_error
        return Tool::Response.new([{ type: "text", text: JSON.generate(graph) }]).to_h if graph.is_a?(Error)

        # Refresh may keep a usable old snapshot after 100 ms; the next call rechecks the marker.
        # Re-attached stores and surgical updates are resolved, so resolving here would rescan it.
        @graph_mutex.synchronize do
          graph.prepare_for_tool_call

          arguments = arguments.transform_keys(&:to_sym)
          response = begin
            tool.new(graph).call(**arguments)
          rescue StandardError
            raise unless graph.store_errors.positive?
          end
          if graph.store_errors.positive?
            graph.prepare_for_tool_call # quarantine and rebuild in memory before returning a partial answer
            response = tool.new(graph).call(**arguments)
          end
          response.to_h
        end
      end
    end

    class << self
      #: (?String) -> void
      def run(path = ".")
        root = File.realpath(path)
        server = Server.new(root_path: root, transport: StdioTransport.new)
        server.spawn_indexer

        server.main_loop
      end
    end
  end
end
