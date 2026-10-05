# frozen_string_literal: true

require "rubydex"
require "minitest/autorun"
require "tmpdir"
require "fileutils"

# Git gives the changed set far cheaper than walking the tree, so a session can
# decide "nothing changed" without stat-ing every file. It is only a fast path:
# it never concludes on its own unless the workspace is a clean git checkout, and
# paths are translated from repo-root-relative to workspace-relative.
class GitDetectTest < Minitest::Test
  GIT_ENV = {
    "GIT_AUTHOR_NAME" => "t",
    "GIT_AUTHOR_EMAIL" => "t@t",
    "GIT_COMMITTER_NAME" => "t",
    "GIT_COMMITTER_EMAIL" => "t@t",
  }.freeze

  def sh(where, *args)
    system(GIT_ENV, *args, chdir: where)
  end

  def capture(where, *args)
    %x(git -C #{where} #{args.join(" ")}).strip
  end

  def test_it_detects_a_committed_change
    Dir.mktmpdir("rdx-git-") do |dir|
      ws = File.join(dir, "app")
      FileUtils.mkdir_p(ws)
      sh(ws, "git", "init", "-q", ".")
      File.write(File.join(ws, "a.rb"), "class A; end\n")
      sh(ws, "git", "add", "--", ".")
      sh(ws, "git", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "one")
      before = capture(ws, "rev-parse", "HEAD")

      File.write(File.join(ws, "a.rb"), "class A; def switched; end; end\n")
      sh(ws, "git", "add", "--", ".")
      sh(ws, "git", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "two")

      assert_equal(["a.rb"], Rubydex::Graph.new.git_changed_files(ws, before))
    end
  end

  def test_it_refuses_when_the_checkout_is_not_clean
    Dir.mktmpdir("rdx-git-") do |dir|
      ws = File.join(dir, "app")
      FileUtils.mkdir_p(ws)
      sh(ws, "git", "init", "-q", ".")
      File.write(File.join(ws, "a.rb"), "class A; end\n")
      assert_nil(
        Rubydex::Graph.new.git_changed_files(ws, "HEAD"),
        "untracked files make git diff incomplete",
      )
    end
  end

  def test_it_translates_paths_for_a_workspace_inside_a_larger_repo
    Dir.mktmpdir("rdx-git-") do |dir|
      repo = File.join(dir, "monorepo")
      ws = File.join(repo, "app")
      FileUtils.mkdir_p(ws)
      sh(repo, "git", "init", "-q", ".")
      File.write(File.join(ws, "a.rb"), "class A; end\n")
      sh(repo, "git", "add", "--", ".")
      sh(repo, "git", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "one")
      before = capture(repo, "rev-parse", "HEAD")

      File.write(File.join(ws, "a.rb"), "class A; def switched; end; end\n")
      sh(repo, "git", "add", "--", ".")
      sh(repo, "git", "-c", "commit.gpgsign=false", "commit", "-q", "-m", "two")

      assert_equal(
        ["a.rb"],
        Rubydex::Graph.new.git_changed_files(ws, before),
        "repo-root-relative paths must be translated",
      )
    end
  end
end
