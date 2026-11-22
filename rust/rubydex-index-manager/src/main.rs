//! `rubydex-index-manager`: the single machine-wide file-system event subscriber.
//!
//! Sessions register their workspace and store; the manager is the only process that
//! subscribes to file-system events, debounces them, and spawns one indexer
//! subprocess at a time. It never indexes itself, and it exits once no session is
//! alive. Sessions never depend on it: with the manager absent or killed, they fall
//! back to the git fast path and the stat walk.

use clap::Parser;
use serde_json::to_string;

use std::path::PathBuf;
use std::process::exit;
use std::time::{Duration, Instant};

use rubydex_index_manager::prune::prune_orphaned_stores;
use rubydex_index_manager::watches::Watch;
use rubydex_index_manager::watches::groups;
use rubydex_index_manager::watches::sync;

#[derive(Parser, Debug)]
#[command(
    name = "rubydex-index-manager",
    about = "Watch registered workspaces and spawn indexers",
    version
)]
struct Args {
    #[arg(long = "registry", help = "Directory holding one locked registry file per session")]
    registry: PathBuf,

    #[arg(long = "list", help = "Print the live sessions as JSON and exit")]
    list: bool,

    #[arg(
        long = "run",
        help = "Watch the registered workspaces and spawn indexers until no session is alive"
    )]
    run: bool,

    #[arg(
        long = "debounce-ms",
        default_value = "200",
        help = "A burst of events inside this window collapses into one indexer"
    )]
    debounce_ms: u32,
}

/// How often to look for store directories whose workspace has been deleted. Deleting a checkout is
/// rare and re-scanning the cache root is a few `stat`s per store, so this can be lazy.
const PRUNE_INTERVAL: Duration = Duration::from_secs(600);

/// Evicts the stores of workspaces that no longer exist. `registry` is `<cache_root>/rubydex/manager/
/// sessions`, so the stores live two levels up.
fn prune(registry: &std::path::Path, sessions: &[rubydex_index_manager::registry::Session]) {
    let Some(cache_root) = registry.parent().and_then(std::path::Path::parent) else {
        return;
    };
    let active: Vec<PathBuf> = sessions.iter().map(|session| session.store.clone()).collect();
    let removed = prune_orphaned_stores(cache_root, &active);
    if removed > 0 {
        eprintln!("rubydex-index-manager: evicted {removed} store(s) of deleted workspaces");
    }
}

fn main() {
    let args = Args::parse();

    if args.list {
        let sessions = rubydex_index_manager::registry::live_sessions(&args.registry);
        let json = to_string(&sessions).unwrap();
        println!("{json}");
        exit(0);
    }

    if args.run {
        // Only one manager per machine: it holds an exclusive lock on `manager.lock` for its whole
        // lifetime, so a manager that finds the lock taken knows another one is running. The OS
        // releases the lock when the process dies, which is also how sessions are pruned.
        let Some(parent) = args.registry.parent() else {
            exit(1);
        };
        let Ok(handle) = std::fs::File::create(parent.join("manager.lock")) else {
            exit(1);
        };
        if handle.try_lock().is_err() {
            exit(0);
        }

        let debounce = Duration::from_millis(args.debounce_ms.into());
        let mut watches: Vec<Watch> = Vec::<Watch>::new();
        let mut last_sync: Option<Instant> = None;
        let mut last_prune: Option<Instant> = None;
        prune(
            &args.registry,
            &rubydex_index_manager::registry::live_sessions(&args.registry),
        );

        loop {
            // Re-scan the registry on the debounce cadence instead of every 20ms idle tick, so an
            // idle daemon does not open, flock and parse every session file 50 times a second.
            if last_sync.is_none_or(|at| at.elapsed() >= debounce) {
                let sessions = rubydex_index_manager::registry::live_sessions(&args.registry);
                if sessions.is_empty() {
                    exit(0);
                }

                sync(&mut watches, groups(&sessions), debounce);
                last_sync = Some(Instant::now());

                if last_prune.is_none_or(|at| at.elapsed() >= PRUNE_INTERVAL) {
                    prune(&args.registry, &sessions);
                    last_prune = Some(Instant::now());
                }
            }

            // One pass drains every workspace without blocking, and the pass sleeps once when
            // nothing happened. Waiting on each workspace's debounce in turn made one pass cost N
            // debounce windows, stalling session pruning and workspace sync behind them.
            let mut idle = true;
            for watch in &mut watches {
                watch.builder.reap();
                let batch = watch.source.take_ready_batch();
                idle &= batch.is_empty();
                watch.builder.record(batch.len());
                let started = watch.builder.start(watch.manifest);
                if started.is_some() {
                    idle = false;
                    eprintln!("rubydex-index-manager: started {started:?}");
                }
            }
            if idle {
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }

    exit(1);
}
