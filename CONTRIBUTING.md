# Contributing

## Project structure

This project is structured in 3 parts:

- `rust/rubydex`: the Rust crate implementing all of the core logic for statically analyzing Ruby code
- `rust/rubydex-sys`: the Rust crate for FFI bindings that allow using the main crate from C code
- the top level of the repository is a Ruby gem with a native extension, which uses C code to link against the Rust
crate and provide a Ruby API for the Rust backed implementation

## Architecture, concepts and analysis

To understand how the analysis is structured, please see [the architecture docs](docs/architecture.md). We also have
documentation for [Ruby concepts and behavior](docs/ruby-behaviors.md).

## Practical development tips

In general, we strive for consistency in our development environments and techniques. For example, recommended
extensions and settings for VS Code are already provided in the `.vscode` directory.

Instructions for AI models and agents are in the [agents file](AGENTS.md).

### Rust

The `rust` directory is a worskpace, where all common cargo commands can be used to build, lint or run tests.

- Testing: `cargo test`
- Linting: `cargo clippy`
- Formatting: `rustfmt`
- Compiling: `cargo build`

With the recommended extensions, it's possible to use Rust Analyzer's debug code lens actions to interactively
debug tests through VS Code.

We try to be on the latest version of Rust and CI always runs against the latest.

### Ruby

- Compiling: `bundle exec rake compile` (triggers the compilation of the Rust crates too)
- Testing: `bundle exec rake ruby_test`
- Linting: `bundle exec rubocop`
- Formatting: `bundle exec rubocop -a`

### Tests under valgrind

A separate CI job runs the suite under valgrind with `--trace-children=yes` to catch leaks
(`bundle exec rake ruby_test:valgrind`, one job per sanitizer). A traced child process costs roughly
1000x its normal time — a store build, `git`, a compiler, the index manager — and a child killed
mid-report leaves a truncated XML file that `ruby_memcheck` cannot parse, which fails the job after
hours with `Premature end of data in tag valgrindoutput` instead of naming a leak.

So a test file that can spawn a child declares it in its header, within the first ten lines:

```ruby
# frozen_string_literal: true

# valgrind: skip — <why this file cannot run under the tracer>

require "test_helper"
```

The memleak task selects on that marker, and `test/integration/**` is skipped wholesale.
`test/valgrind_selection_test.rb` fails when a file that can spawn a child does not declare it, which
is what keeps a new spawn-heavy test from silently rejoining the traced run. The marker has to be a
line of its own, starting at column 0; a mention mid-sentence does not count.

One consequence worth knowing: the disk-index tests are the only ones that read a store-backed graph,
and they all spawn store builds, so that read path has no valgrind coverage. It runs on the plain
`ruby_test` task and under the FFI allocator-invariants job instead.

## Releasing

Releases are cut by maintainers from `main`. GitHub Actions builds and publishes
precompiled gems to RubyGems and publishes the crates to crates.io. Do not run
`rake release` or `cargo publish` locally.

To cut a new release:

1. Check out `main` and make sure it is current:

   ```sh
   git checkout main
   git pull --ff-only
   ```

2. Set the release version in `rust/Cargo.toml`. Both Rust crates inherit this
   value:

   ```toml
   [workspace.package]
   version = "X.Y.Z"
   ```

   For a beta release, use `X.Y.Z-beta.N` in this file.

3. Run the synchronization task to update `lib/rubydex/version.rb` and the
   `rubydex` dependency version in `rust/rubydex-sys/Cargo.toml`. Do not edit
   those versions manually:

   ```sh
   bundle exec rake sync_versions
   ```

4. Refresh `rust/Cargo.lock` and `Gemfile.lock` to record the new crate and gem
   versions:

   ```sh
   cargo check --manifest-path rust/Cargo.toml
   bundle lock --local
   ```

5. Commit the version bump directly on `main`:

   ```sh
   git add lib/rubydex/version.rb rust/Cargo.toml rust/rubydex-sys/Cargo.toml rust/Cargo.lock Gemfile.lock
   git commit -m "Bump version to vX.Y.Z"
   git push origin main
   ```

6. Tag the same commit using `v` followed by the generated Ruby version, then
   push the tag:

   ```sh
   git tag vX.Y.Z
   git push origin vX.Y.Z
   ```

   For example, Cargo version `1.2.3-beta.1` generates Ruby version
   `1.2.3.beta1`. Use `v1.2.3.beta1` in both tag commands for that release.

Pushing a tag matching `vX.Y.Z` or `vX.Y.Z.betaN` triggers the release workflow
in `.github/workflows/release.yml`, which publishes the gem to RubyGems and the
crates to crates.io. Workflow dispatch can be used for a dry run; only tag pushes
publish a release.
