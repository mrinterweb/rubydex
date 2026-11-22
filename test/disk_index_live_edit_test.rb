# frozen_string_literal: true

# valgrind: skip — index_workspace builds the store in a child

require "tmpdir"
require "fileutils"
require "test_helper"

class DiskIndexLiveEditTest < Minitest::Test
  def setup
    super
    skip_unless_store_feature
    # On macOS `$TMPDIR` is `/var/folders/...`, a symlink to `/private/var/...`, and the store keys
    # documents by the real path, so a URI built from the alias would shadow a different document.
    @tmp = File.realpath(Dir.mktmpdir)
    ENV["RUBYDEX_DISK_INDEX"] = "1"
    ENV["RUBYDEX_REFRESH_TTL"] = "0" # tests edit out of band and expect the next refresh to see them
    ENV["RUBYDEX_CACHE_DIR"] = @tmp
  end

  def teardown
    ENV.delete("RUBYDEX_DISK_INDEX")
    ENV.delete("RUBYDEX_REFRESH_TTL")
    ENV.delete("RUBYDEX_CACHE_DIR")
    FileUtils.remove_entry(@tmp) if @tmp && File.exist?(@tmp)
    super
  end

  def test_index_source_applies_to_store_backed_graph
    # `index_workspace` builds the store in a short-lived child process, which requires spawning a
    # Ruby process with rubydex loadable. Without it, `index_workspace` silently falls back to the
    # in-memory path (covered by other tests) — there's nothing store-backed to verify here.
    skip("ruby subprocess unavailable; index_workspace can't build a store here") unless Process.respond_to?(:spawn)

    rb = File.join(@tmp, "foo.rb")
    File.write(rb, "class Foo\n  def bar; end\nend\n")

    graph = Rubydex::Graph.configure_for_workspace(@tmp)
    graph.index_workspace # builds + attaches the store

    # The store-backed graph can still read declarations from disk.
    refute_nil(graph["Foo"], "graph[\"Foo\"] should read from the store")

    # Live edit: change the method name. Store documents are keyed by `file://` URIs (from
    # index_workspace), so the edit must use the same URI to replace the store-backed document.
    # `path_to_uri` is that form: hand-building `file://#{rb}` yields `file://D:/...` on Windows
    # where the store holds `file:///D:/...`, and the edit lands on a second document.
    graph.index_source(graph.path_to_uri(rb), "class Foo\n  def baz; end\nend\n", "ruby")
    graph.resolve

    # The overlay document shadows the store's, and resolution surfaces the edited members.
    foo = graph["Foo"]
    refute_nil(foo, "graph[\"Foo\"] still readable after live edit")
    member_names = foo.members.map(&:name)
    assert_includes(member_names, "Foo#baz()")
    refute_includes(member_names, "Foo#bar()")
  end

  def test_manifest_records_the_workspace_so_orphaned_stores_can_be_evicted
    skip("ruby subprocess unavailable; index_workspace can't build a store here") unless Process.respond_to?(:spawn)

    File.write(File.join(@tmp, "foo.rb"), "class Foo; end\n")

    graph = Rubydex::Graph.configure_for_workspace(@tmp)
    graph.index_workspace # builds + attaches the store

    manifest = JSON.parse(File.read("#{graph.send(:store_cache_path)}.files"))
    assert_equal(@tmp, manifest["workspace"], "the manifest must name the checkout it belongs to")
    assert_kind_of(Hash, manifest["files"])
  end

  def test_store_backed_definition_comments_are_read_from_source
    skip("ruby subprocess unavailable; index_workspace can't build a store here") unless Process.respond_to?(:spawn)

    rb = File.join(@tmp, "foo.rb")
    File.write(rb, "# Doc comment\nclass Foo; end\n")

    graph = Rubydex::Graph.configure_for_workspace(@tmp)
    graph.index_workspace # builds + attaches the store

    # The store keeps comment offsets only, so the text has to come back from the source file.
    foo = graph["Foo"]
    refute_nil(foo, "graph[\"Foo\"] should read from the store")
    assert_equal(["# Doc comment"], foo.definitions.first.comments.map(&:string))

    # Edit/truncate the file on disk behind the store (without refreshing): attaching directly
    # exercises the C FFI comment path against stale offsets and must return empty text, not crash.
    File.write(rb, "x\n")
    stale = Rubydex::Graph.new
    stale.attach_store(graph.send(:active_store_path))
    assert_equal([""], stale["Foo"].definitions.first.comments.map(&:string))
  ensure
    stale&.detach_store
    graph&.detach_store
  end

  def test_rebuilds_publish_new_generations_and_reject_unsafe_store_names
    skip("ruby subprocess unavailable; index_workspace can't build a store here") unless Process.respond_to?(:spawn)

    rb = File.join(@tmp, "foo.rb")
    File.write(rb, "class Foo; end\n")

    graph = Rubydex::Graph.configure_for_workspace(@tmp)
    graph.index_workspace
    cache = graph.send(:store_cache_path)
    first_gen = graph.send(:active_store_path)
    assert_match(Rubydex::Graph::GENERATION_STORE_PATTERN, File.basename(first_gen))

    # A rebuild writes a fresh generation file and cleans up the old one once published.
    graph.send(:build_store_in_child, cache)
    second_gen = File.join(File.dirname(cache), JSON.parse(File.read("#{cache}.files")).fetch("store"))
    refute_equal(first_gen, second_gen, "each rebuild must write a distinct generation file")
    assert(File.exist?(second_gen))
    unless Gem.win_platform?
      refute(File.exist?(first_gen), "POSIX unlinks superseded generations immediately")
    end

    # Moving the session onto the new generation releases its handle on the old one and deletes it.
    graph.send(:attach_store_generation, second_gen)
    refute(File.exist?(first_gen), "detaching the old generation cleans it up")

    # Trust boundary: a manifest naming a path outside the store directory must be rejected.
    File.write(
      "#{cache}.files",
      JSON.generate({ "store" => "../escape.redb", "lockfile" => "no-lockfile", "files" => {} }),
    )
    assert_equal([nil, nil, nil], graph.send(:read_manifest, "#{cache}.files"))
  ensure
    graph&.detach_store
  end

  def test_failed_store_build_cleans_up_temp_files
    ws = File.join(@tmp, "ws")
    FileUtils.mkdir_p(ws)
    File.write(File.join(ws, "a.rb"), "class A; end\n")

    graph = Rubydex::Graph.configure_for_workspace(ws)
    cache_dir = File.join(@tmp, "cache", "key")
    cache = File.join(cache_dir, "index.redb")

    # The builder runs in a fresh process, so the failure has to be simulated at the process
    # boundary: `false` exits non-zero without writing anything, like a builder that dies.
    original_spawn = Process.method(:spawn)
    Process.define_singleton_method(:spawn) { |*| original_spawn.call("false") }

    assert_raises(RuntimeError) { graph.send(:build_store_in_child, cache) }
    assert_empty(
      Dir.glob(File.join(cache_dir, "*.building")),
      "temp files leaked after a failed store build",
    )
  ensure
    Process.define_singleton_method(:spawn, original_spawn) if original_spawn
  end

  def test_build_does_not_publish_a_store_for_a_lockfile_changed_during_indexing
    ws = File.join(@tmp, "ws")
    FileUtils.mkdir_p(ws)
    File.write(File.join(ws, "a.rb"), "class A; end\n")
    lockfile = File.join(ws, "Gemfile.lock")
    File.write(lockfile, "before\n")
    graph = Rubydex::Graph.configure_for_workspace(ws)
    cache = File.join(@tmp, "cache", "index.redb")

    original_wait2 = Process.method(:wait2)
    Process.define_singleton_method(:wait2) do |pid|
      result = original_wait2.call(pid)
      File.write(lockfile, "after\n")
      result
    end

    assert_raises(RuntimeError) { graph.send(:build_store_in_child, cache) }
    refute(File.exist?("#{cache}.hash"), "a build from the old dependency set must not be marked fresh")
  ensure
    Process.define_singleton_method(:wait2, original_wait2) if original_wait2
  end

  def test_store_signature_tracks_layout_version_not_gem_version
    graph = Rubydex::Graph.configure_for_workspace(@tmp)
    baseline = graph.send(:store_signature)

    # The gem version is not part of the signature, so changing it must not change the signature.
    with_stubbed_const(Rubydex, :VERSION, "9.9.9") do
      assert_equal(
        baseline,
        graph.send(:store_signature),
        "the gem version must not invalidate a store whose layout is unchanged",
      )
    end

    # The layout version is a method backed by the FFI constant (STORE_FORMAT_VERSION in
    # rust/rubydex/src/model/store.rs); stubbing the method stubs the constant.
    with_stubbed_method(Rubydex::Graph.singleton_class, :store_format_version, 999_999) do
      refute_equal(
        baseline,
        graph.send(:store_signature),
        "a store-layout change must invalidate the store",
      )
    end
  end

  def with_stubbed_const(owner, name, value)
    original = owner.const_get(name)
    owner.send(:remove_const, name)
    owner.const_set(name, value)
    yield
  ensure
    owner.send(:remove_const, name)
    owner.const_set(name, original)
  end

  def with_stubbed_method(owner, name, value)
    # Alias-restore, not define+remove: the original may be a C method, and remove_method after a
    # Ruby redefinition deletes it for the rest of the process (order-dependent suite breakage).
    original = "#{name}_original_for_stub"
    owner.alias_method(original, name)
    owner.define_method(name) { value }
    yield
  ensure
    owner.alias_method(name, original)
    owner.remove_method(original)
  end
end
