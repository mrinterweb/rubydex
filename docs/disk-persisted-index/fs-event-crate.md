# File-system event crate — decision record (Task 3.1)

Status: **awaiting approval** before the index manager depends on it. The adapter
spike (`rust/rubydex/src/fs_events.rs`, `tests/fs_events.rs`) already exists so the
choice can be judged against working code.

## Candidates (crates.io data, fetched 2026-10-05)

| crate | version | downloads / recent | platforms | notes |
|-------|---------|--------------------|-----------|-------|
| `notify` | 9.0.0-rc.5 (updated 2026-08-30) | 164.8M / 41.7M | Linux+Android inotify; FreeBSD inotify or kqueue; macOS FSEvents (or kqueue); Windows ReadDirectoryChangesW; polling fallback everywhere | MSRV 1.88 (repo floor is 1.89). Used by alacritty, cargo-watch, deno, mdBook, rust-analyzer, watchexec, zed. `Watcher` trait + `recommended_watcher` |
| `hotwatch` | 0.5.0 | 414k / 37.6k | same as notify (it is a thin wrapper over notify) | last release ~2021; maintenance risk |
| `file-watcher` | 0.0.18 | 45.8k / 167 | unverified | negligible traction |
| `fswatch` | 0.1.10 | 3.4k / 47 | needs the external `fswatch` C tool at runtime | extra runtime dependency |
| `inotify` | 0.11.5 | 173M / 43M | **Linux only** | not multi-platform |
| `watchify`, `fsnotify`, `hotdog`, `dirwatch` | — | — | — | not published on crates.io under those names; could not verify |

## Decision

`notify` v9, behind the optional Cargo feature `fs_events`, wrapped by the
`FsEventSource` adapter trait (`next_batch`, `try_next_batch`).

Why:
- it is the only maintained crate that actually covers Linux/macOS/Windows in one
  API, and it is the de-facto standard (41.7M recent downloads, used by the
  editors/CLIs this project is compared against);
- MSRV 1.88 fits the repo's `rust-version = "1.89"` floor;
- the adapter trait means the manager never names notify, so a backend swap
  (kqueue-only builds, polling, or a future crate) is one file.

Rejected:
- `inotify` — Linux-only, would need a second crate per platform;
- `hotwatch` — wrapper over notify with no release in ~4 years;
- `fswatch` — shells out to a C tool we do not ship;
- `file-watcher` — no traction, unverified platform coverage.

## Known limitations (accepted)

- **Rename/move** while watched is platform-dependent (notify issues #165/#166);
  the adapter treats every reported path as changed, and the indexer re-reads the
  file, so a rename costs two reindexes rather than being missed.
- **inotify watch count**: one watch per watched root (recursive), so the limit
  scales with worktrees, not files. Measured on this machine: 2,005 dirs ≈ 9 ms
  setup, ~2 MB RSS; stock distro default is 8,192 watches.
- **Debounce is ours, not notify's**: notify v9 dropped its debouncers, so
  `NotifySource` coalesces events that have been quiet for `debounce`.
- **Access events** are filtered out via `EventKindMask::CREATE | ALL_MODIFY | REMOVE`.

## Approval

Approving means the index manager (Phase 3, Tasks 3.2–3.4) may subscribe through
`FsEventSource` and no other FS-event crate is introduced.

## Approval

Approved 2026-10: the user chose `notify`. Tasks 3.2–3.4 build on it, and the
adapter lives in the manager crate (`rust/rubydex-index-manager/src/fs_events.rs`), so
notify stays out of every gem build; `default_source` is the one place a backend swap
changes.
