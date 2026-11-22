//! Eviction of store directories whose workspace no longer exists.
//!
//! The cache root accumulates one directory per workspace that ever ran a disk-indexed session, and
//! nothing removed the directories of workspaces that were deleted or re-hashed. Reaping them is the
//! only part of the cache lifecycle that has no natural owner: sessions reap watchers, the OS
//! releases locks, but a store outlives everyone who could notice it is orphaned.
//!
//! Every rule here exists to make a wrong deletion impossible. A directory is only removed when it
//! is a real directory (not a symlink), it holds an `index.redb.files` manifest that names an
//! absolute `workspace`, that workspace is *gone* (not merely unreadable), no live session holds its
//! store, and no rebuild is in flight.

use std::path::{Path, PathBuf};

/// A store directory's manifest, which is what proves the directory is ours.
const MANIFEST: &str = "index.redb.files";

/// The store file each directory publishes, and the marker suffix of a directory a rebuild has
/// claimed. Both are relative to the store directory.
const STORE: &str = "index.redb";
const REBUILD_LOCK: &str = "index.redb.rebuild";

/// Removes the store directories under `cache_root` whose workspace has been deleted.
///
/// `active_stores` are the store files live sessions hold, which must never be removed even if their
/// workspace is gone (the directory can be deleted while an editor still has it open).
///
/// Returns how many directories were removed.
#[must_use]
pub fn prune_orphaned_stores(cache_root: &Path, active_stores: &[PathBuf]) -> usize {
    let Ok(entries) = cache_root.read_dir() else {
        return 0;
    };

    let mut removed = 0;
    for entry in entries.flatten() {
        let dir = entry.path();
        // `file_type` does not follow symlinks: a symlinked directory is not a store, and removing
        // it could delete something outside the cache root.
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `manager` holds the registry and the lock, not a store; dotfiles are ours to ignore.
        if name == "manager" || name.starts_with('.') {
            continue;
        }
        // A rebuild claimed this directory: wait for it to finish rather than evicting the store or
        // racing its generation publish.
        if dir.join(REBUILD_LOCK).exists() {
            continue;
        }
        let session_active = active_stores.iter().any(|store| store == &dir.join(STORE));
        if !session_active && workspace_is_gone(&dir) {
            // Best effort: a directory that cannot be removed is retried on the next tick.
            if std::fs::remove_dir_all(&dir).is_ok() {
                removed += 1;
            }
            continue;
        }
        prune_stale_generations(&dir);
    }
    removed
}

/// Whether `name` matches a generation store filename (`index.<16 lowercase hex>.redb`).
fn is_generation_store_name(name: &str) -> bool {
    let Some(hex) = name.strip_prefix("index.").and_then(|s| s.strip_suffix(".redb")) else {
        return false;
    };
    hex.len() == 16 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// Removes unreferenced generation store files (and superseded legacy `index.redb` files) from a
/// live store directory. Best-effort: files still held open by a Windows session fail to remove and
/// are retried on a later tick.
fn prune_stale_generations(dir: &Path) {
    let Ok(manifest) = std::fs::read(dir.join(MANIFEST)) else {
        return;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&manifest) else {
        return;
    };
    let active = match json.get("store") {
        Some(value) => match value.as_str() {
            Some(name) if is_generation_store_name(name) => name,
            _ => return,
        },
        None => STORE,
    };
    let Ok(entries) = dir.read_dir() else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_file() {
            continue;
        }
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if name == active {
            continue;
        }
        if name == STORE || is_generation_store_name(name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// Whether the store directory's manifest names a workspace that no longer exists.
///
/// Every "not sure" answer is `false`: a manifest this version cannot read, without a `workspace`
/// key (written before that key existed), or naming a relative path is left alone. So is a workspace
/// that exists but cannot be stat'd — a transient mount or permissions error must never be mistaken
/// for a deletion.
fn workspace_is_gone(dir: &Path) -> bool {
    let Ok(manifest) = std::fs::read(dir.join(MANIFEST)) else {
        return false;
    };
    let Ok(json) = serde_json::from_slice::<serde_json::Value>(&manifest) else {
        return false;
    };
    let Some(workspace) = json.get("workspace").and_then(serde_json::Value::as_str) else {
        return false;
    };
    let workspace = Path::new(workspace);
    if !workspace.is_absolute() {
        return false;
    }
    match std::fs::metadata(workspace) {
        Ok(_) => false,
        Err(error) => error.kind() == std::io::ErrorKind::NotFound,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds `<cache_root>/<name>` as a store directory for `workspace`.
    fn store_dir(cache_root: &Path, name: &str, workspace: Option<&Path>) -> PathBuf {
        let dir = cache_root.join(name);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join(STORE), b"store").expect("write store");
        if let Some(workspace) = workspace {
            let manifest = serde_json::json!({
                "workspace": workspace.to_string_lossy(),
                "lockfile": "",
                "files": {},
            });
            std::fs::write(dir.join(MANIFEST), manifest.to_string()).expect("write manifest");
        }
        dir
    }

    #[test]
    fn removes_the_store_of_a_deleted_workspace() {
        let cache = tempfile::tempdir().expect("cache");
        let workspace = tempfile::tempdir().expect("workspace");
        let gone = workspace.path().to_path_buf();
        drop(workspace);

        let dir = store_dir(cache.path(), "abc123", Some(&gone));
        assert_eq!(1, prune_orphaned_stores(cache.path(), &[]));
        assert!(!dir.exists(), "the orphaned store directory is removed");
    }

    #[test]
    fn keeps_the_store_of_a_live_workspace() {
        let cache = tempfile::tempdir().expect("cache");
        let workspace = tempfile::tempdir().expect("workspace");

        let dir = store_dir(cache.path(), "abc123", Some(workspace.path()));
        assert_eq!(0, prune_orphaned_stores(cache.path(), &[]));
        assert!(dir.exists());
    }

    #[test]
    fn keeps_a_store_a_live_session_holds() {
        let cache = tempfile::tempdir().expect("cache");
        let workspace = tempfile::tempdir().expect("workspace");
        let gone = workspace.path().to_path_buf();
        drop(workspace);

        let dir = store_dir(cache.path(), "abc123", Some(&gone));
        let held = vec![dir.join(STORE)];
        assert_eq!(0, prune_orphaned_stores(cache.path(), &held));
        assert!(dir.exists(), "a store a session still serves is never removed");
    }

    #[test]
    fn keeps_a_store_whose_rebuild_is_in_flight() {
        let cache = tempfile::tempdir().expect("cache");
        let workspace = tempfile::tempdir().expect("workspace");
        let gone = workspace.path().to_path_buf();
        drop(workspace);

        let dir = store_dir(cache.path(), "abc123", Some(&gone));
        std::fs::create_dir_all(dir.join(REBUILD_LOCK)).expect("claim rebuild");
        assert_eq!(0, prune_orphaned_stores(cache.path(), &[]));
        assert!(dir.exists());
    }

    #[test]
    fn keeps_directories_it_cannot_prove_are_orphaned_stores() {
        let cache = tempfile::tempdir().expect("cache");

        // Not a store: no manifest at all.
        let bare = cache.path().join("bare");
        std::fs::create_dir_all(&bare).expect("mkdir");
        // A manifest without the `workspace` key (written by an older version).
        let old = store_dir(cache.path(), "old", None);
        // A manifest whose `workspace` is not absolute.
        let relative = store_dir(cache.path(), "relative", Some(Path::new("workspace")));
        // A manifest that is not JSON at all.
        let garbage = store_dir(cache.path(), "garbage", None);
        std::fs::write(garbage.join(MANIFEST), b"not json").expect("write");
        // The manager's own directory.
        let manager = cache.path().join("manager");
        std::fs::create_dir_all(&manager).expect("mkdir");

        assert_eq!(0, prune_orphaned_stores(cache.path(), &[]));
        for dir in [bare, old, relative, garbage, manager] {
            assert!(dir.exists(), "{} must survive", dir.display());
        }
    }

    #[test]
    fn keeps_unreadable_workspaces_that_are_not_missing() {
        // A non-`NotFound` stat failure is not a deletion: traversing beneath a regular file yields
        // `NotADirectory` on Unix without needing to change permissions.
        let cache = tempfile::tempdir().expect("cache");
        let workspace = tempfile::tempdir().expect("workspace");
        let file = workspace.path().join("a-file");
        std::fs::write(&file, b"").expect("write file");
        let beneath_file = file.join("child");
        #[cfg(unix)]
        {
            let err = std::fs::metadata(&beneath_file).expect_err("traversing a file must error");
            assert_ne!(
                err.kind(),
                std::io::ErrorKind::NotFound,
                "expected a non-NotFound error to exercise the non-deletion branch"
            );
        }

        let dir = store_dir(cache.path(), "abc123", Some(&beneath_file));
        #[cfg(unix)]
        {
            assert_eq!(0, prune_orphaned_stores(cache.path(), &[]));
            assert!(dir.exists());
        }
        #[cfg(not(unix))]
        let _ = dir;
    }

    #[test]
    fn sweeps_stale_generation_files_without_touching_the_active_one() {
        let cache = tempfile::tempdir().expect("cache");
        let workspace = tempfile::tempdir().expect("workspace");
        let dir = cache.path().join("live123");
        std::fs::create_dir_all(&dir).expect("mkdir");

        let active = "index.0123456789abcdef.redb";
        let stale = "index.fedcba9876543210.redb";
        std::fs::write(dir.join(active), b"active").expect("write active");
        std::fs::write(dir.join(stale), b"stale").expect("write stale");
        std::fs::write(dir.join(STORE), b"legacy").expect("write legacy");
        std::fs::write(dir.join("index.redb.hash"), b"hash").expect("write hash");
        let manifest = serde_json::json!({
            "store": active,
            "workspace": workspace.path().to_string_lossy(),
            "lockfile": "",
            "files": {},
        });
        std::fs::write(dir.join(MANIFEST), manifest.to_string()).expect("write manifest");

        assert_eq!(0, prune_orphaned_stores(cache.path(), &[]));
        assert!(dir.join(active).exists(), "active generation must survive");
        assert!(dir.join("index.redb.hash").exists(), "sidecars must survive");
        assert!(!dir.join(stale).exists(), "stale generation must be removed");
        assert!(
            !dir.join(STORE).exists(),
            "superseded legacy index.redb must be removed"
        );
    }

    #[test]
    fn leaves_generations_alone_while_a_rebuild_lock_is_held() {
        let cache = tempfile::tempdir().expect("cache");
        let workspace = tempfile::tempdir().expect("workspace");
        let dir = cache.path().join("rebuilding");
        std::fs::create_dir_all(dir.join(REBUILD_LOCK)).expect("lock");

        let active = "index.0123456789abcdef.redb";
        let incoming = "index.fedcba9876543210.redb";
        std::fs::write(dir.join(active), b"active").expect("write active");
        std::fs::write(dir.join(incoming), b"incoming").expect("write incoming");
        let manifest = serde_json::json!({
            "store": active,
            "workspace": workspace.path().to_string_lossy(),
            "lockfile": "",
            "files": {},
        });
        std::fs::write(dir.join(MANIFEST), manifest.to_string()).expect("write manifest");

        assert_eq!(0, prune_orphaned_stores(cache.path(), &[]));
        assert!(dir.join(incoming).exists(), "in-flight generation must not be deleted");
    }
}
