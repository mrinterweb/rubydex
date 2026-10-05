//! `rubydex-index-manager`: the single machine-wide file-system event subscriber.
//!
//! Sessions register their workspace and store; the manager is the only process that
//! subscribes to file-system events, debounces them, and spawns one indexer
//! subprocess at a time. It never indexes itself, and it exits once no session is
//! alive. Sessions never depend on it: with the manager absent or killed, they fall
//! back to the git fast path and the stat walk.

pub mod builder;
pub mod fs_events;
pub mod registry;
