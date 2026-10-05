//! `rubydex-index-manager`: the single machine-wide file-system event subscriber.
//!
//! Sessions register their workspace and store; the manager is the only process that
//! subscribes to file-system events, debounces them, and spawns one indexer
//! subprocess at a time. It never indexes itself, and it exits once no session is
//! alive. Sessions never depend on it: with the manager absent or killed, they fall
//! back to the git fast path and the stat walk.

mod registry;

use clap::Parser;
use serde_json::to_string;

use std::path::PathBuf;
use std::process::exit;
use std::thread::sleep;
use std::time::Duration;

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

    #[arg(long = "run", help = "Poll the registry until no session is alive")]
    run: bool,

    #[arg(long = "poll-ms", default_value = "500", help = "Registry poll interval")]
    poll_ms: u32,
}

fn main() {
    let args = Args::parse();

    if args.list {
        let sessions = registry::live_sessions(&args.registry);
        let json = to_string(&sessions).unwrap();
        println!("{json}");
        exit(0);
    }

    if args.run {
        loop {
            let sessions = registry::live_sessions(&args.registry);
            if sessions.is_empty() {
                exit(0);
            }
            sleep(Duration::from_millis(args.poll_ms.into()));
        }
    }

    exit(1);
}
