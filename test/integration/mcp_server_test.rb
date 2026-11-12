# frozen_string_literal: true

require "test_helper"
require "helpers/context"
require "json"
require "open3"
require "rbconfig"
require "rubydex/mcp_server"
require "timeout"

class MCPServerIntegrationTest < Minitest::Test
  include Test::Helpers::WithContext

  MAX_INDEXING_RETRIES = 200
  # A read answers immediately once the server is warm, except the first `codebase_stats` call: that
  # one waits for the background indexer, which on a debug-jemalloc or valgrind build takes minutes.
  # A 5 s budget there failed the FFI allocator job with a bare `Timeout::Error` instead of the retry
  # the test was written for.
  READ_BUDGET = 5
  INDEXING_READ_BUDGET = 240
  EXIT_BUDGET = 30
  # A runner slow enough to miss the budget still has to say how slow, or "did not exit" cannot be
  # told apart from a real hang.
  LATE_EXIT_BUDGET = 180

  def test_executable_prints_help
    stdout, _stderr, status = run_executable("--help")

    assert_predicate(status, :success?)
    assert_includes(stdout, "mcp [PATH]")
    assert_includes(stdout, "Run the MCP server for AI assistants")
  end

  def test_executable_prints_version
    stdout, _stderr, status = run_executable("--version")

    assert_predicate(status, :success?)
    assert_equal("v#{Rubydex::VERSION}\n", stdout)
  end

  def test_executable_rejects_extra_arguments
    stdout, stderr, status = run_executable("mcp", "foo", "bar")

    assert_equal(1, status.exitstatus)
    assert_empty(stdout)
    assert_includes(stderr, "unexpected argument: bar")
    assert_includes(stderr, "mcp [PATH]")
  end

  def test_executable_rejects_unknown_options
    stdout, stderr, status = run_executable("mcp", "--unknown")

    assert_equal(1, status.exitstatus)
    assert_empty(stdout)
    assert_includes(stderr, "invalid option: --unknown")
    assert_includes(stderr, "mcp [PATH]")
  end

  def test_mcp_server_can_be_required_directly
    stdout, stderr, status = Open3.capture3(
      RbConfig.ruby,
      "-rbundler/setup",
      "-Ilib",
      "-e",
      <<~RUBY,
        require "rubydex/mcp_server"

        puts Rubydex::VERSION
        puts Rubydex::Graph.name

        response = Rubydex::MCPServer::Server.new(root_path: Dir.pwd).handle(
          jsonrpc: "2.0",
          id: 1,
          method: "initialize",
        )
        puts response.fetch(:result).fetch(:serverInfo).fetch(:version)
      RUBY
    )

    assert_predicate(status, :success?, stderr)
    assert_equal("#{Rubydex::VERSION}\nRubydex::Graph\n#{Rubydex::VERSION}\n", stdout)
  end

  def test_mcp_server_e2e
    with_context do |context|
      context.write!("app.rb", "class Dog; end")

      stderr_output = +""
      Open3.popen3(RbConfig.ruby, "-rbundler/setup", executable_path, "mcp", context.absolute_path) do |stdin, stdout, stderr, wait_thr|
        stderr_reader = Thread.new do
          stderr_output << stderr.read
        rescue IOError
          # This thread only drains stderr. If teardown closes the pipe while read is blocked, there is
          # no more stderr to capture and the subprocess exit status is still checked through wait_thr.
        end

        initialize_session(stdin, stdout)
        assert_tools_are_registered(stdin, stdout)

        stats, request_id = wait_for_indexing_to_complete(stdin, stdout, 3)
        assert_operator(stats.fetch("files"), :>=, 1)
        assert_operator(stats.fetch("declarations"), :>, 0)

        search_response = call_tool(stdin, stdout, request_id + 1, "search_declarations", { query: "Dog", match_mode: "exact" })
        assert_equal(["Dog"], search_response.fetch("results").map { |result| result.fetch("name") })

        dead_code_response = call_tool(stdin, stdout, request_id + 2, "find_dead_code_candidates", { limit: 1 })
        assert_operator(dead_code_response.fetch("total"), :>=, 1)
        assert_equal(1, dead_code_response.fetch("candidates").length)

        stdin.close
        closed_at = Process.clock_gettime(Process::CLOCK_MONOTONIC)
        begin
          Timeout.timeout(EXIT_BUDGET) { wait_thr.value }
        rescue Timeout::Error
          blocked = thread_dump(wait_thr.pid)
          begin
            Timeout.timeout(LATE_EXIT_BUDGET) { wait_thr.value }
            flunk("rdx mcp exited #{elapsed_since(closed_at).round(1)}s after stdin closed, past the " \
              "#{EXIT_BUDGET}s budget. stderr:\n#{stderr_output}")
          rescue Timeout::Error
            Process.kill("TERM", wait_thr.pid)
            flunk("rdx mcp did not exit after stdin closed. Exit threads: #{blocked} stderr:\n#{stderr_output}")
          end
        end
        stderr_reader.join
      end
    end
  end

  # `run_shutdown` has to end rather than wait: a thread that defers its own kill used to be joined
  # without a bound, so the server never exited — the shape of the "did not exit after stdin closed"
  # failure on a loaded runner.
  def test_shutdown_returns_for_a_thread_that_defers_its_kill
    with_context do |context|
      server = Rubydex::MCPServer::Server.new(root_path: context.absolute_path)
      stubborn = Thread.new do
        Thread.handle_interrupt(Object => :never) { sleep(60) }
      end
      sleep(0.05)

      # In a thread, so the version that waits forever fails the assertion instead of hanging the suite.
      shutdown = Thread.new { server.send(:join_or_kill_all, [stubborn]) }

      assert(shutdown.join(10), "shutdown must not wait for a thread that defers its kill")
    end
  end

  private

  def run_executable(*arguments)
    Open3.capture3(
      RbConfig.ruby,
      "-rbundler/setup",
      executable_path,
      *arguments,
    )
  end

  def elapsed_since(started_at)
    Process.clock_gettime(Process::CLOCK_MONOTONIC) - started_at
  end

  # "Did not exit" hides whether the child is stuck or merely slow and where, so read the kernel's
  # view of each thread: a thread parked in wait4 is waiting on a subprocess, in futex on a Ruby-level
  # join, in epoll_wait on input. Linux only; elsewhere this is just a process state.
  def thread_dump(pid)
    state = File.read("/proc/#{pid}/status")[/^State:.*$/]
    threads = Dir.glob("/proc/#{pid}/task/*").map do |task|
      syscall = File.read("#{task}/syscall").split.first
      "#{File.basename(task)}:#{File.read("#{task}/comm").strip}:syscall=#{syscall}:#{File.read("#{task}/wchan").strip}"
    end
    "#{state} #{threads.join(" ")}"
  rescue SystemCallError
    "gone before the dump"
  end

  def executable_path
    File.expand_path("../../exe/rdx", __dir__)
  end

  def send_message(stdin, message)
    stdin.puts(JSON.generate(message))
    stdin.flush
  end

  def send_request(stdin, id, method, params)
    send_message(
      stdin,
      {
        jsonrpc: "2.0",
        id: id,
        method: method,
        params: params,
      },
    )
  end

  def read_response(stdout, budget = READ_BUDGET)
    Timeout.timeout(budget) do
      line = stdout.gets
      flunk("Expected JSON-RPC response, got EOF") unless line

      JSON.parse(line)
    end
  end

  def read_response_for_id(stdout, expected_id, budget = READ_BUDGET)
    response = read_response(stdout, budget)
    assert_equal(expected_id, response.fetch("id"))
    response
  end

  def initialize_session(stdin, stdout)
    send_request(
      stdin,
      1,
      "initialize",
      {
        protocolVersion: "2025-03-26",
        capabilities: {},
        clientInfo: { name: "test-client", version: "0.1.0" },
      },
    )

    response = read_response_for_id(stdout, 1)
    assert_kind_of(Hash, response.fetch("result").fetch("capabilities").fetch("tools"))

    send_message(
      stdin,
      {
        jsonrpc: "2.0",
        method: "notifications/initialized",
      },
    )
  end

  def assert_tools_are_registered(stdin, stdout)
    send_request(stdin, 2, "tools/list", {})
    response = read_response_for_id(stdout, 2)
    tool_names = response.fetch("result").fetch("tools").map { |tool| tool.fetch("name") }

    assert_equal(
      [
        "codebase_stats",
        "find_constant_references",
        "find_dead_code_candidates",
        "get_declaration",
        "get_descendants",
        "get_file_declarations",
        "search_declarations",
      ],
      tool_names.sort,
    )
  end

  def call_tool(stdin, stdout, request_id, tool_name, arguments)
    send_request(
      stdin,
      request_id,
      "tools/call",
      {
        name: tool_name,
        arguments: arguments,
      },
    )

    response = read_response_for_id(stdout, request_id)
    JSON.parse(response.fetch("result").fetch("content")[0].fetch("text"))
  end

  # The first `codebase_stats` call waits for the background indexer instead of answering at once.
  def call_tool_before_the_index_is_ready(stdin, stdout, request_id)
    send_request(stdin, request_id, "tools/call", { name: "codebase_stats", arguments: {} })
    response = read_response_for_id(stdout, request_id, INDEXING_READ_BUDGET)
    JSON.parse(response.fetch("result").fetch("content")[0].fetch("text"))
  end

  def wait_for_indexing_to_complete(stdin, stdout, request_id)
    MAX_INDEXING_RETRIES.times do
      parsed = call_tool_before_the_index_is_ready(stdin, stdout, request_id)
      return [parsed, request_id] unless parsed.key?("error")

      assert_equal("indexing", parsed.fetch("error"))
      request_id += 1
      sleep(0.05)
    end

    flunk("Timed out waiting for indexing to complete")
  end
end
