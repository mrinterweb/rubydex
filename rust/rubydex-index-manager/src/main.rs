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
use std::time::Duration;

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

        loop {
            let sessions = rubydex_index_manager::registry::live_sessions(&args.registry);
            if sessions.is_empty() {
                exit(0);
            }

            sync(&mut watches, groups(&sessions), debounce);

            for watch in &mut watches {
                watch.builder.reap();
                let batch = watch.source.try_next_batch(debounce);
                watch.builder.record(batch.len());
                let started = watch.builder.start(watch.manifest);
                if started.is_some() {
                    eprintln!("rubydex-index-manager: started {started:?}");
                }
            }
        }
    }

    exit(1);
}
