# Installing rubydex from this checkout and using it from Neovim

Two parts: build/install the gem from this Git repo, then wire the Ruby LSP add-on that ships
inside it into Neovim's **native** LSP client (`vim.lsp.config` / `vim.lsp.enable`, Neovim 0.11+).

Everything below was run against this branch (`disk-persisted-index`, gem version `0.4.4`) on
Ruby 4.0.5, rustc 1.99.1 and Neovim 0.12.6.

---

## What rubydex does and does not provide

| Component | What it is | Entry point |
|---|---|---|
| Ruby API | The graph: index, resolve, query declarations/definitions/references | `require "rubydex"` → `Rubydex::Graph` |
| CLI | `lint`, `query`, `dead_code`, `mcp`, `skill`, `console` | `exe/rdx` → `rdx <command>` |
| MCP server | Agent-facing tools over stdio | `rdx mcp` |
| **LSP** | **No server of its own.** Editor integration is a **Ruby LSP add-on** that lives in this gem | `lib/ruby_lsp/rubydex/addon.rb` |

So "configure it with Neovim's LSP" means: install this gem, install the prerelease Ruby LSP that
supports it, and point Neovim at `ruby-lsp`. Neovim talks to Ruby LSP; Ruby LSP loads rubydex as an
add-on.

---

## Part 1 — Install the gem from the Git source

### Requirements

| Requirement | Why |
|---|---|
| Ruby `>= 3.2.0`, RubyGems `>= 3.3.11` | `required_ruby_version` / `required_rubygems_version` in `rubydex.gemspec` |
| Rust `>= 1.99.1` and `cargo` on `PATH` | `rust/Cargo.toml` sets `rust-version = "1.99.1"`; `ext/rubydex/extconf.rb` aborts with `Installing Rubydex requires Cargo…` without it |
| A C compiler and `make` | The gem is a native extension (`spec.extensions = ["ext/rubydex/extconf.rb"]`) |
| Neovim `>= 0.11` | `vim.lsp.config` / `vim.lsp.enable` / `lsp/<name>.lua` |

### 1a. Build it in the checkout

```bash
cd /home/sean/code/rubydex          # or wherever this repo lives

bundle install                      # development dependencies (rake-compiler, etc.)
bundle exec rake compile            # Rust crate + C extension, then the index manager
```

`rake compile` is enhanced with the `index_manager` task, so it also produces
`lib/rubydex/rubydex-index-manager` (the background manager used by the disk index). Confirm the
extension loads:

```bash
bundle exec ruby -e 'require "rubydex"; puts "#{Rubydex::VERSION} / #{Rubydex::Graph.name}"'
# => 0.4.4 / Rubydex::Graph
```

Notes:

- If you change `ext/rubydex/extconf.rb` (features, flags), force a re-configure:
  `rm -rf tmp/x86_64-linux/rubydex && bundle exec rake compile`.
- Feature switches are environment variables read by `extconf.rb`: `RUBYDEX_NO_REDB_STORE=1` drops
  the disk-index/redb layer, `RUBYDEX_JEMALLOC_DEBUG=1` turns on the allocator invariant checks.
- In this checkout's own bundle the extension is built in **debug** mode; an install from another
  project builds it in **release** mode (`extconf.rb` decides by comparing `BUNDLE_GEMFILE` to this
  repo's directory).

### 1b. Use it from another project (recommended for local development)

In the Gemfile of the project you want indexed:

```ruby
group :development do
  gem "rubydex", path: "/home/sean/code/rubydex", require: false
end
```

```bash
bundle install
bundle exec ruby -e 'require "rubydex"; puts Rubydex::VERSION'   # => 0.4.4
```

Use an **absolute** path: the Ruby LSP evaluates the project Gemfile from a generated Gemfile under
`.ruby-lsp/`, and a relative `path:` there resolves against the wrong directory.

### 1c. Other install shapes

Build and install the `.gem` into the active Ruby (needs Rust at install time, because the extension
compiles from source):

```bash
bundle exec rake build             # -> pkg/rubydex-0.4.4.gem
gem install pkg/rubydex-0.4.4.gem
# or, equivalently:
bundle exec rake install:local
```

Straight from Git (Bundler builds the extension for you):

```ruby
gem "rubydex", git: "git@github.com:mrinterweb/rubydex.git", branch: "disk-persisted-index", require: false
```

A `git:` source is a **snapshot**, not a live checkout: Bundler installs one revision into its own
directory, so a later `git pull` in a clone changes nothing until you `bundle install` (which
re-resolves the branch) in every project that uses it. Use it for reproducibility, not for a rapid
loop — `path:` (1b) is the live one.

One caveat for gem installs: `lib/rubydex/rubydex-index-manager` is only packaged when the gem is
built with `RELEASE=1` **and** the binary already exists (see `rubydex.gemspec`). Without it, the
disk index's background manager is simply unavailable — sessions warn
(`rubydex: index manager unavailable …`) and keep working. A `path:` install reads the binary
straight out of the checkout, so `rake compile` there is enough.

### Global install without editing every project Gemfile?

The per-project `path:` line in 1b is only there to get this checkout onto the `ruby-lsp` server's
load path: `ruby-lsp` discovers add-ons via `Gem.find_files("ruby_lsp/**/addon.rb")` inside the
server process, and its composed bundle (`.ruby-lsp/Gemfile`) otherwise resolves the published
`rubydex` gem from RubyGems.

If you want to trial this checkout across multiple projects without modifying their Gemfiles, each
global workaround carries a trade-off (the first two were probed, not run end-to-end as a daily
driver):

- **`cmd_env = { RUBYOPT = "-I/home/sean/code/rubydex/lib" }` in Neovim.** Prepends the checkout's
  `lib/` to `$LOAD_PATH`, so `Gem.find_files` finds the checkout's `addon.rb` first and
  `$LOADED_FEATURES` loads `/home/sean/code/rubydex/lib/rubydex.rb`. Caveat:
  `Gem.loaded_specs["rubydex"].full_gem_path` still points at the published gem, and the compiled
  extension in the checkout must match the project's Ruby ABI.
- **Shared `cmd_env = { BUNDLE_GEMFILE = "~/.ruby-lsp/Gemfile" }`.** `exe/ruby-lsp` skips composing
  when `BUNDLE_GEMFILE` is set, so one personal Gemfile can hold `path: "/home/sean/code/rubydex"`.
  Caveat: `Bundler.locked_gems` then comes from that personal bundle, so core Ruby LSP loses the
  project's own gem dependencies.
- **Installing a locally built `.gem` globally (`rake build && gem install`).** RubyGems distributes
  a *platform* gem (`rubydex-0.4.4-x86_64-linux`, `extensions = []`) whereas `rake build` produces a
  *source* gem (`rubydex-0.4.4`, `extensions = ["ext/rubydex/extconf.rb"]`). Because the spec names
  differ, installing the source gem leaves the platform gem installed alongside it unless you
  uninstall the platform gem first.

---

## Part 1d — Iterating: pull, rebuild, no version juggling

A `path:` gem is **not a copy**. Bundler puts the checkout's `lib/` on the load path, so after a
`git pull` the next `bundle exec` in your project loads the new Ruby code with no `bundle install`,
no gem build, no version bump, no rename:

```
$ cd /tmp/rdxapp && bundle exec ruby -e 'require "rubydex"; puts $LOADED_FEATURES.grep(/rubydex\/version\.rb/)'
/home/sean/code/rubydex/lib/rubydex/version.rb
```

The only thing that is a build artifact is the native extension (`lib/rubydex/rubydex.so`, plus the
index manager binary). That is what has to be rebuilt when Rust or C changed — which is what the
script below does.

### The script

`utils/dev_install.sh` is idempotent: run it once on a fresh machine, then after every pull. It
fetches and fast-forwards the branch, installs the checkout's own bundle, rebuilds the extension and
manager (a no-op if nothing relevant changed), and prints what a project will load.

```bash
git clone -b disk-persisted-index git@github.com:mrinterweb/rubydex.git ~/code/rubydex
~/code/rubydex/utils/dev_install.sh          # bootstrap

cd ~/code/rubydex && git pull && ./utils/dev_install.sh   # every iteration
```

```
branch   disk-persisted-index at 4dbcfeb Bound the valgrind marker to lines, …
rubydex  0.4.4 (path gem /home/sean/code/rubydex)
loading  /home/sean/code/rubydex/lib/rubydex/version.rb
manager  built
```

Overrides: `RUBYDEX_BRANCH=other` to track another branch, `RUBYDEX_SKIP_PULL=1` to build what is
checked out without network, `RUBYDEX_NO_REDB_STORE=1` to build without the disk layer.

The project side never changes: one line, one `bundle install` (only when you add or update the gem):

```ruby
gem "rubydex", path: "/home/sean/code/rubydex", require: false
```

An editor holds the loaded code in memory, so restart the language server to pick a pull up
(`:LspRestart` in Neovim). Command-line use (`bundle exec rdx …`) needs no restart.

### What a version bump does — and does not — break

The checkout's version is whatever `lib/rubydex/version.rb` says after the pull; you never edit it.
Bundler re-reads the path gemspec on every run, which gives three behaviours (all measured):

| Change in the checkout | Effect in the project |
|---|---|
| Ruby code only | Picked up immediately. Nothing to run. |
| Rust/C only | Needs `rake compile` (the script). Ruby code alone is never enough. |
| `VERSION` moves, still inside every consumer's constraint (e.g. `0.4.4` → `0.4.5` against `ruby-lsp`'s `>= 0.4.0, < 0.5.0`) | Still runs without `bundle install`; `bundle install` quietly refreshes `Gemfile.lock`. |
| `VERSION` moves outside a consumer's constraint (`0.4.x` → `0.5.0`) | `bundle exec` fails with `Bundler::SolveFailure: Could not find compatible versions … and rubydex … could not be found in source at <path>, so <consumer> cannot be used`. No gem-file editing fixes it: the consumer has to resolve to a version that allows the new rubydex. |

That last row is the only version work you can be forced into, and it comes from the consumer
(`ruby-lsp`), not from this checkout. Two ways to keep it automatic:

- **Pin nothing, let the composed bundle resolve.** Do not put `ruby-lsp` in the project Gemfile at
  all; run the server as `ruby-lsp --beta`, which builds `.ruby-lsp/Gemfile` that evaluates your
  project Gemfile and runs `(bundle check && bundle update ruby-lsp) || bundle install` on boot. When
  the checkout's version moves, that update picks a `ruby-lsp` that accepts it. It needs a
  startable `ruby-lsp` outside the project: `gem install ruby-lsp` (it pulls the published `rubydex`
  for its own boot, and the project still resolves the `path:` gem). The mechanism above is read from
  `ruby-lsp`'s `SetupBundler`; the end-to-end runs in this document used the pinned route, not this
  one.
- **Pin `ruby-lsp`, accept the rare unlock.** Keep `gem "ruby-lsp", "0.27.0.beta5"` in the project
  Gemfile (Part 2b). Bumping that one line is what a real version break costs — and only when the
  checkout crosses `0.5.0`.

---

## Part 2 — Wire it into Neovim's LSP

### 2a. Which Ruby LSP version, and why exactly one works

Ruby LSP discovers add-ons by looking for `ruby_lsp/**/addon.rb`:

```ruby
# ruby-lsp 0.27.0.beta5, lib/ruby_lsp/addon.rb
addon_files = Gem.find_files("ruby_lsp/**/addon.rb")
```

This gem ships `lib/ruby_lsp/rubydex/addon.rb`, which activates only on Ruby LSP
`>= 0.27.0.beta4, < 0.28` (`lib/ruby_lsp/rubydex/addon.rb:9`). Ruby LSP then gained a hard runtime
dependency on rubydex, and the constraints differ per prerelease:

| `ruby-lsp` | rubydex requirement | Works with this checkout (0.4.4)? |
|---|---|---|
| `0.26.11` (stable) | none — no rubydex-backed beta | No: add-on guard rejects it |
| `0.27.0.beta4` | `>= 0.2.7, < 0.3.0` | No: Bundler conflict |
| **`0.27.0.beta5`** | **`>= 0.4.0, < 0.5.0`** | **Yes** |

So pin `0.27.0.beta5`. A `bundle install` against `beta4` fails resolution, not silently.

### 2b. Add both gems to the project

```ruby
# Gemfile of the project you want indexed
group :development do
  gem "rubydex", path: "/home/sean/code/rubydex", require: false
  gem "ruby-lsp", "0.27.0.beta5", require: false
end
```

```bash
bundle install
# resolves as:
#   ruby-lsp (0.27.0.beta5) … rubydex (>= 0.4.0, < 0.5.0)
#   rubydex (0.4.4) from path /home/sean/code/rubydex
```

`require: false` matters: neither gem is needed by the app itself, only by tooling.

### 2c. Neovim native LSP configuration

Neovim 0.11+ merges configurations from `lsp/<name>.lua` files on `'runtimepath'`, then
`vim.lsp.enable()` starts the server for a buffer based on `filetypes` and `root_markers`
(`:help lsp-config`, `:help vim.lsp.enable()`).

Create `~/.config/nvim/lsp/ruby_lsp.lua`:

```lua
---@type vim.lsp.Config
return {
  -- Run the server through the project bundle so it can see `rubydex` and the rubygems it indexes.
  -- Alternatives, depending on your setup:
  --   { "ruby-lsp", "--beta" }                  -- let the composed bundle manage versions
  --   { "~/.local/share/mise/shims/ruby-lsp" }  -- version-manager shim, no bundle exec
  cmd = { "bundle", "exec", "ruby-lsp" },
  filetypes = { "ruby", "eruby" },
  root_markers = { "Gemfile", ".git" },
  -- Required for the add-on to see file changes. Neovim only advertises
  -- `dynamicRegistration` on macOS and Windows, and the add-on requires
  -- `relativePatternSupport` as well before it registers its watcher.
  capabilities = {
    workspace = {
      didChangeWatchedFiles = {
        dynamicRegistration = true,
        relativePatternSupport = true,
      },
    },
  },
  init_options = {
    formatter = "auto", -- rubydex does not format; see 2e
  },
}
```

Without that `capabilities` block the client still attaches and indexes, but the add-on never
registers its `rubydex.toml` watcher — on Linux by default, and on every platform when another
plugin narrows capabilities.

and enable it once, in `init.lua`:

```lua
vim.lsp.enable("ruby_lsp")
```

If you already configure the client inline — e.g. `vim.lsp.config("ruby_lsp", { cmd = … })` plus
`vim.lsp.enable("ruby_lsp")` — that is the same mechanism; the `lsp/ruby_lsp.lua` file is just the
default that inline calls override. When `nvim-lspconfig` is installed, its `ruby_lsp` defaults
(`cmd = { "ruby-lsp" }`, `filetypes = { "ruby", "eruby" }`, `root_markers = { "Gemfile", ".git" }`)
merge underneath yours.

Choosing `cmd`:

- `{ "bundle", "exec", "ruby-lsp" }` — simplest when `ruby-lsp` is in the project Gemfile; the
  bundle already contains both gems, so the add-on is discoverable.
- `{ "ruby-lsp" }` or `{ "ruby-lsp", "--beta" }` — Ruby LSP builds a *composed bundle* in
  `<project>/.ruby-lsp/Gemfile` that `eval_gemfile`s your project Gemfile and re-executes itself with
  it; `--beta` is the real flag that opts into prerelease server gems.
- `{ "~/.local/share/mise/shims/ruby-lsp" }` (or `rbenv`/`asdf` shims, or `mise x -- ruby-lsp`) —
  when the editor is not launched from a shell with the right Ruby environment.

### 2d. Optional: the linter configuration file

Add `rubydex.toml` at the project root. Diagnostics come from `rdx lint`; the add-on re-reads this
file whenever it changes:

```toml
[linter.rules.DynamicAncestor]
severity = "warning"   # "hint" | "information" | "warning" | "error"
```

Only the keys the linter documents are valid (`docs/linter.md`). An unknown key makes the add-on fail
to load — see Troubleshooting.

### 2e. Verify

In Neovim, with a Ruby file from the project open:

```vim
:checkhealth vim.lsp          " confirms ruby_lsp attached and its capabilities
:lua =vim.lsp.get_clients()   " the client named ruby_lsp, with its root_dir
:LspLog                       " server stderr/log, where add-on errors surface
```

Without the editor:

```bash
bundle exec rdx lint                              # 938 files inspected, 1 offense detected: …
bundle exec rdx query "MATCH (c:Class) RETURN c.name"
```

Diagnostics are *pushed* by the add-on (`textDocument/publishDiagnostics`, `source: "Rubydex"`,
`code:` the rule name) on `workspace/didChangeWatchedFiles`, rather than pulled on demand.
`lib/ruby_lsp/rubydex/addon.rb` only registers a watcher for `rubydex.toml` (not `.rb`/`.rbs`
files), and `ruby-lsp` itself registers no base watchers. In Neovim, that means Rubydex diagnostics
only refresh when `rubydex.toml` is created or saved (unless another add-on or client registers a
`.rb` watcher). Rubydex's static-analysis linter is purely output and separate from style linters
like `standard` or `rubocop` — indexing, go-to-definition, hover, completion, and references do not
depend on it.

What this setup does **not** give you: formatting. `Rubydex::Linter` registers a formatter id, but its
`run_formatting` is an empty stub, so `formatter = "rubydex"` produces no edits. Leave
`formatter = "auto"` and let another formatter (RuboCop, Syntax Tree) own formatting.

---

## Part 3 — Optional extras

**Disk index.** The whole point of this branch: opt in per workspace and the bulk index lives in a
redb store instead of the heap. `rubydex.toml`:

```toml
[disk_index]
enabled = true
location = "tmp"     # "tmp" | "global" | an absolute directory
manager = true       # machine-wide background refresher
```

See `docs/disk-index.md` for the file layout, costs and recovery. `RUBYDEX_DISK_INDEX=1` flips it on
for a single run without committing the section.

**MCP server** for agent tooling (not LSP):

```bash
claude mcp add --scope project rubydex -- bundle exec rdx mcp
```

---

## Troubleshooting

| Symptom | Cause | Fix |
|---|---|---|
| `Could not find 'rubydex' (>= 0.2.7, < 0.3.0) … (Gem::MissingSpecError)` when starting `ruby-lsp` | A globally installed `ruby-lsp 0.27.0.beta4` with no rubydex in that gem set | Use `bundle exec ruby-lsp` with `ruby-lsp 0.27.0.beta5` and this gem in the project Gemfile |
| Bundler cannot resolve `rubydex` | `ruby-lsp 0.27.0.beta4` requires `< 0.3.0`; this checkout is `0.4.4` | Pin `gem "ruby-lsp", "0.27.0.beta5"` |
| Neovim shows `Error loading add-ons: Rubydex Linter: Invalid config file … unknown setting …` | `rubydex.toml` uses key names the linter does not define (e.g. `[linter] exclude = []`) | Use the documented shapes only: `[linter.rules.<RuleName>]` with `enabled`, `exclude`, `severity` |
| `Installing Rubydex requires Cargo…` during `bundle install` | No Rust toolchain on `PATH` (or older than `1.99.1`) | `rustup update stable`, then reinstall |
| LSP attaches but no Rubydex diagnostics appear | The add-on only watches `rubydex.toml`, or the client did not accept dynamic watcher registration | Touch/save `rubydex.toml` and check `:LspLog` for `rubydex-linter-file-watcher` registration (or run `bundle exec rdx lint` on the CLI) |
| `rubydex: index manager unavailable (Errno::ENOENT …)` | The manager binary was not packaged with the install | `bundle exec rake index_manager` in the checkout (a `path:` install picks it up), or ignore it — sessions work without the manager |
| Native extension rebuild does not pick up an `extconf.rb` change | Rake caches the configured extension | `rm -rf tmp/*/rubydex && bundle exec rake compile` |
