# Disk-persisted index

Rubydex normally builds its whole index in the heap of the process that asks for it. On a large
workspace that is hundreds of megabytes per process: on a 27,674-file corpus, 692 MB resident. The
disk-persisted index keeps that bulk index in a [redb](https://github.com/cberner/redb) store on disk
instead, opens it read-only, and holds in memory only the small set of nodes a live edit touches. The
same session drops to 73 MB, and a bare open costs about 5 MB.

It is opt-in per workspace, and it is a cache: the store is always rebuildable from your source files,
and deleting it is always safe.

## Enabling it

Commit a `rubydex.toml` with a `[disk_index]` section:

```toml
[disk_index]
enabled = true          # keep the bulk index in a redb store instead of the heap
location = "tmp"        # "tmp" (the workspace's tmp/), "global" (the platform cache), or an absolute dir
manager = true          # let the machine-wide background manager keep the store fresh
```

| Variable | Effect |
| --- | --- |
| `RUBYDEX_DISK_INDEX=0` | Use the in-memory path for this run, ignoring the config. |
| `RUBYDEX_CACHE_DIR=/path` | Put stores under a directory you choose. |
| `RUBYDEX_INDEX_MANAGER=0` | Never start or register with the background manager. |
| `RUBYDEX_REFRESH_TTL=0` | Re-check the filesystem before every call instead of reusing the last check. |

Where the store goes: `RUBYDEX_CACHE_DIR` wins, then `location`, then the workspace's own `tmp/` when
one exists (the Rails convention, and the right home for a worktree workflow, where every checkout is
a different workspace), then the platform cache. A store kept outside the workspace is named
`<workspace>-<hash8>` so `du` on the cache root reads as the projects it belongs to. When rubydex
writes a store inside your repo it adds the directory to `.git/info/exclude`, so it cannot be
`git add -A`'d by accident — and it is never something you should commit.

## What lands on disk

| File | What it is |
| --- | --- |
| `index.redb` | The index itself. Read-only to every session; rebuilt, never patched. |
| `index.redb.hash` | Signature of the source state the store was built from. |
| `index.redb.files` | Manifest: every file in the store with its mtime and size. |
| `index.redb.corrupt` | A store that reported read failures, moved out of the way (see Troubleshooting). |
| `index.redb.rebuild/` | Create-exclusive lock directory: N stale sessions pay one rebuild. |

## How a session keeps up with your edits

At boot a session builds the store if it is missing or stale, then opens it. After that, before every
query it asks "has anything changed?" with two signals, cheap one first:

- **The commit under the workspace** (`git rev-parse HEAD`, ~1.3 ms). Catches commits, branch switches
  and stashes — the changes that rewrite many files at once — without touching a single file.
- **A walk of the workspace** (stat every indexable file; ~105 ms on 6,441 files). Reused for 2 seconds
  at a time, so a burst of queries pays for it once. It never runs on a timer: no query, no walk.

What that means in practice: edits saved through your editor are visible immediately (the editor
indexes what it saves), a branch switch is visible on the next call, and an edit made by `sed` or a
formatter outside the editor can be answered up to two seconds late. Set `RUBYDEX_REFRESH_TTL=0` if you
want that window closed.

When something did change, the size of the change picks the remedy. A small diff is applied into the
session's own overlay in milliseconds (200 files: ~71 ms). A large diff — or one that would leave the
overlay holding more than a quarter of the workspace — triggers a rebuild in a short-lived child
process, whose memory is reclaimed when it exits, and every session in the directory moves to the new
store. Rebuilding is what keeps a long-lived server from quietly accumulating the whole index in memory
again (~12 MB per 1,000 edited files).

## The background manager (optional)

`rubydex-index-manager` is a small Rust process, one per machine, that watches the workspaces with a
live session and keeps their stores fresh while nobody is asking. It is the only process that
subscribes to filesystem events; it debounces 200 ms of events into one indexer, runs one indexer at a
time, and spawns it as a child so build memory dies with it. A burst covering a quarter of the
workspace rebuilds from scratch. It exits when no session is alive.

It is a head start, never a dependency: with it absent, killed, or never started, sessions still
refresh exactly as described above. Liveness is an OS lock rather than a PID — a session holds its
registry file under an exclusive lock for its whole lifetime, so the manager learns a session died the
moment the OS released that lock, however it died. Measured with three idle sessions: 3.4 MB resident,
0.2% CPU; a 200-file branch-switch storm reaches a fresh store marker 9 s later.

## Costs

On a 6,441-file workspace: first build ~25 s (once, then only when the source state genuinely changes
or the layout version moves), re-attaching to a store someone else built ~71 ms, a 200-file surgical
refresh ~71 ms. Cold builds happen in a child process, so the peak belongs to the child and is returned
to the operating system when it exits.

## Troubleshooting

**Answers look stale.** Force a fresh look: `RUBYDEX_REFRESH_TTL=0 bundle exec rdx ...`. If it clears
up, the 2-second window was the cause and you can leave it at 0 for that process.

**You suspect the store is wrong or you just upgraded.** Delete it. `rm -rf <store dir>` — the next
session rebuilds. Nothing else needs to be reset.

**An `index.redb.corrupt` appeared.** A read from the store failed (a bad disk, a truncated file, an
incompatible layout from a development build). Rubydex warns, moves the store aside, detaches it and
indexes in memory, so you keep working. The next run rebuilds. If it happens on every run after an
upgrade, that is a bug — open an issue with the warning line.

**Two editors and an agent server on one directory.** They share one store. The first one to notice a
change claims the rebuild lock and builds; the others re-attach to the result. One rebuild, not three.

**You want the memory back but not the disk path.** `RUBYDEX_DISK_INDEX=0` for one run, or
`enabled = false` in the config.

## Limits

- Live edits live in the session's memory and are never written back to the store. Another session sees
  them only once the store is rebuilt from disk.
- Out-of-band edits can be answered up to `RUBYDEX_REFRESH_TTL` late (2 s by default).
- The store is a local, rebuildable cache: not shareable between machines, not worth committing, and
  safe to delete at any moment.

## Internals

- `docs/disk-persisted-index/memory-baseline.md` — the measurement log (resident memory and timings by
  corpus and stage) that the performance comments in the code quote. Reproduce with
  `utils/session_profile.rb` and the `bench_build_write_vs_drop` test.
- `docs/disk-persisted-index/fs-event-crate.md` — the decision record for the file-system event crate
  the manager uses.
