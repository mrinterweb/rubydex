#!/usr/bin/env bash
#
# Local development install for this rubydex checkout.
#
# Fresh machine:  ./utils/dev_install.sh
# After pulling:  ./utils/dev_install.sh          (same command, idempotent)
#
# It pulls the branch, installs the checkout's own dependencies, rebuilds the native extension (and
# the index manager) only when something actually changed, and prints exactly what a project that
# uses `gem "rubydex", path: "<this checkout>"` will load. No version bumping, no gem build, no
# reinstall: a path gem loads this checkout's `lib/` directly, so the only artifact that has to be
# rebuilt is the extension.
#
# The extension is built in release mode by default. A debug build of this checkout comes out of
# `rake compile`, and it makes the disk index ~30x slower (a 2s store rebuild becomes 68s, a 90ms
# edit becomes 330ms), which is easy to mistake for a performance regression. `--fast` keeps the
# debug build for iterating on Rust, where the compile time matters more than the runtime.
#
# Environment overrides:
#   RUBYDEX_BRANCH=other-branch     branch to check out and pull (default: current if it tracks one)
#   RUBYDEX_SKIP_PULL=1             build what is checked out, without touching the network
#   RUBYDEX_NO_REDB_STORE=1         compile without the disk-index/redb layer
#   RUBYDEX_FAST_BUILD=1            same as --fast: build the extension in debug mode
#
set -euo pipefail

repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"

if ! command -v cargo >/dev/null 2>&1; then
  required="$(sed -n 's/^rust-version = "\(.*\)"/\1/p' rust/Cargo.toml)"
  echo "error: cargo is not on PATH; this extension needs Rust ${required:-1.99+} (rustup update stable)" >&2
  exit 1
fi

# 1. Source
if [ -d .git ] && [ -z "${RUBYDEX_SKIP_PULL:-}" ]; then
  branch="${RUBYDEX_BRANCH:-$(git rev-parse --abbrev-ref HEAD)}"
  if [ "$branch" = "HEAD" ]; then
    echo "error: detached HEAD; set RUBYDEX_BRANCH to the branch to pull" >&2
    exit 1
  fi
  git fetch --quiet origin "$branch"
  git checkout --quiet "$branch"
  git merge --ff-only --quiet "origin/$branch"
  echo "branch   $branch at $(git log -1 --format='%h %s')"
else
  echo "branch   $(git rev-parse --abbrev-ref HEAD) at $(git log -1 --format='%h %s') (not pulled)"
fi

# 2. Dependencies of the checkout itself (no-op unless its Gemfile.lock moved)
bundle install --quiet

# 3. Native extension + index manager. Quiet on success, loud on failure.
fast=0
for arg in "$@"; do
  [ "$arg" = "--fast" ] && fast=1
done
[ -n "${RUBYDEX_FAST_BUILD:-}" ] && fast=1

if [ "$fast" = "1" ]; then
  profile=debug
  task=compile
else
  profile=release
  task=compile_release
fi

if ! build_log="$(bundle exec rake "$task" 2>&1)"; then
  printf '%s\n' "$build_log" >&2
  exit 1
fi

# 4. Report what the project will actually load through the path gem
RUBYDEX_PROFILE="$profile" bundle exec ruby -e '
  require "rubydex"
  version = Gem.loaded_specs["rubydex"]
  source = $LOADED_FEATURES.grep(%r{/rubydex/version\.rb\z}).first
  puts "rubydex  #{Rubydex::VERSION}#{" (path gem #{version.full_gem_path})" if version&.full_gem_path}"
  puts "loading  #{source}"
  puts "profile  #{ENV.fetch("RUBYDEX_PROFILE", "release")}"
  puts "manager  #{File.exist?(File.join(__dir__, "lib/rubydex/rubydex-index-manager")) ? "built" : "missing (background refresh off; sessions still work)"}"
'

cat <<'NOTE'

Now, in the project that uses it:
  Gemfile:  gem "rubydex", path: "<this checkout>", require: false
  then:     bundle install
A running editor keeps the old code in memory; restart its language server to pick this up (in
Neovim: :LspRestart, or :lua vim.lsp.enable("ruby_lsp", false) followed by :lua vim.lsp.enable("ruby_lsp")).
NOTE
