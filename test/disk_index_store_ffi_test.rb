# frozen_string_literal: true

require "tmpdir"
require "fileutils"
require "test_helper"

# The memleak job skips every file that builds a store in a child process, which left the whole
# `redb-store` FFI surface untraced. This file builds and attaches the store in-process instead, so
# valgrind sees the store readers, the enumerators and the teardown. It must not spawn a child (see
# test/valgrind_selection_test.rb): `index_all` and `build_store` both stay in this process.
#
# The graph that builds a store must not be the graph that reads it — redb holds an exclusive lock
# per file — so the two halves are separate graphs over the same workspace.
class DiskIndexStoreFfiTest < Minitest::Test
  def setup
    super
    skip_unless_store_feature
    @tmp = Dir.mktmpdir
    @workspace = File.join(@tmp, "ws")
    FileUtils.mkdir_p(@workspace)
    File.write(
      File.join(@workspace, "foo.rb"),
      "# The class comment\nclass Foo\n  CONST = Bar\n\n  def bar\n    Baz.new\n  end\nend\n",
    )
    File.write(File.join(@workspace, "other.rb"), "class Bar\nend\n\nclass Baz\nend\n")
    @graphs = []
  end

  def teardown
    @graphs.each(&:detach_store)
    FileUtils.remove_entry(@tmp) if @tmp && File.exist?(@tmp)
    super
  end

  def test_store_reads_round_trip_through_the_ffi
    memory = Rubydex::Graph.configure_for_workspace(@workspace)
    # Just the two fixture files: `workspace_paths` would pull in every gem the workspace resolves.
    memory.index_all([File.join(@workspace, "foo.rb"), File.join(@workspace, "other.rb")])
    memory.resolve
    store = File.join(@tmp, "index.redb")
    memory.build_store(store)
    # Captured before detaching: `detach_store` drops the overlay with the store.
    expected_line = memory["Foo"].definitions.first.location.start_line
    memory.detach_store

    disk = Rubydex::Graph.configure_for_workspace(@workspace)
    @graphs << disk
    disk.attach_store(store)
    disk.resolve

    assert_equal(0, disk.store_errors, "a store this process wrote reads without errors")
    foo = disk["Foo"]
    refute_nil(foo, "declaration lookup reaches the store")
    assert_equal("Foo", foo.name)
    assert_equal(["# The class comment"], foo.definitions.first.comments.map(&:string), "comments are sliced from disk")
    assert_equal(expected_line, foo.definitions.first.location.start_line, "store definitions keep their source location")

    assert_operator(disk.declarations.count, :>, 0)
    assert_operator(disk.documents.count, :>=, 2)
    document = disk.documents.find { |entry| entry.uri.end_with?("foo.rb") }
    refute_nil(document)
    assert_equal(["CONST", "Foo", "bar()"], document.definitions.map(&:name).sort)
    assert_equal(
      ["new"],
      document.method_references.map(&:name).uniq,
      "a store-backed document has to report its own method references",
    )
    assert_equal(document.method_references.count, document.method_references.size)

    assert_operator(disk.constant_references.size, :>, 0)
    resolved = disk.constant_references.grep(Rubydex::ResolvedConstantReference).map(&:declaration)
    assert_includes(resolved.map(&:name), "Bar", "a store-backed reference resolves to its declaration")

    assert_operator(disk.method_references.size, :>, 0)
    assert_includes(disk.method_references.map(&:name), "new")
    assert_empty(disk.diagnostics)
    refute_nil(disk.search("Foo").first)
    refute_empty(Rubydex::Query.parse("MATCH (c:Class {name: 'Foo'}) RETURN c").run(disk).rows)

    disk.detach_store
    assert_equal(0, disk.store_errors, "detaching releases the store handle without recording an error")
  end
end
