use clap::{Parser, ValueEnum};
use std::{fs, mem, path::PathBuf};

use rubydex::{
    dot,
    indexing::{self, IndexerBackend},
    integrity, listing,
    model::graph::Graph,
    resolution::Resolver,
    stats::{
        memory::MemoryStats,
        timer::{Timer, time_it},
    },
};

#[derive(Parser, Debug)]
#[command(name = "rubydex_cli", about = "A Static Analysis Toolkit for Ruby", version)]
#[allow(clippy::struct_excessive_bools)]
struct Args {
    #[arg(
        value_name = "PATHS",
        default_value = ".",
        help = "Path(s) to index. If the first path is a directory, it is used as the workspace root for rubydex.toml"
    )]
    paths: Vec<String>,

    #[arg(long = "stop-after", help = "Stop after the given stage")]
    stop_after: Option<StopAfter>,

    #[arg(long = "dot", help = "Output a DOT graph visualization")]
    dot: bool,

    #[arg(long = "show-builtins", help = "Include built-in declarations in DOT output")]
    show_builtins: bool,

    #[arg(long = "stats", help = "Show detailed performance statistics")]
    stats: bool,

    #[arg(long = "check-integrity", help = "Check the integrity of the graph after resolution")]
    check_integrity: bool,

    #[arg(
        long = "indexer",
        value_enum,
        default_value = "ruby-indexer",
        help = "Which indexer backend to use for Ruby files"
    )]
    indexer: Indexer,

    #[arg(
        long = "report-orphans",
        value_name = "PATH",
        num_args = 0..=1,
        require_equals = true,
        default_missing_value = "/tmp/rubydex-orphan-report.txt",
        help = "Write orphan definitions report to specified file"
    )]
    report_orphans: Option<String>,

    #[cfg(feature = "redb-store")]
    #[arg(
        long = "build-store",
        value_name = "PATH",
        help = "Persist the resolved graph to a redb store at PATH"
    )]
    build_store: Option<String>,

    #[cfg(feature = "redb-store")]
    #[arg(
        long = "open-store",
        value_name = "PATH",
        help = "Open a prebuilt redb store at PATH and answer queries from disk instead of indexing"
    )]
    open_store: Option<String>,

    #[cfg(feature = "redb-store")]
    #[arg(
        long = "query",
        value_name = "FQN",
        help = "Fully qualified name to look up (with --open-store)"
    )]
    query: Option<String>,

    #[cfg(feature = "redb-store")]
    #[arg(
        long = "search",
        value_name = "PREFIX",
        help = "Prefix-search declaration short names from the store (with --open-store)"
    )]
    search: Option<String>,
}

#[derive(Debug, Clone, ValueEnum)]
enum StopAfter {
    Listing,
    Indexing,
    Resolution,
}

#[derive(Debug, Clone, ValueEnum)]
enum Indexer {
    RubyIndexer,
    OperationBuilder,
}

impl From<&Indexer> for IndexerBackend {
    fn from(indexer: &Indexer) -> Self {
        match indexer {
            Indexer::RubyIndexer => IndexerBackend::RubyIndexer,
            Indexer::OperationBuilder => IndexerBackend::OperationBuilder,
        }
    }
}

fn exit(print_stats: bool) {
    if print_stats {
        Timer::print_breakdown();
        MemoryStats::print_memory_usage();
    }

    std::process::exit(0);
}

fn workspace_path_for(paths: &[String]) -> Option<PathBuf> {
    let first_path = paths.first()?;
    fs::canonicalize(first_path).ok().filter(|path| path.is_dir())
}

fn main() {
    let args = Args::parse();

    if args.stats {
        Timer::set_global_timer(Timer::new());
    }

    let mut graph = Graph::new();

    if let Some(workspace_path) = workspace_path_for(&args.paths) {
        graph.set_workspace_path(workspace_path);
        if let Err(error) = graph.load_config(None) {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }

    // Disk-backed query path: open a prebuilt store and answer queries without indexing anything.
    // The resident memory reported here reflects only what the query touched on disk.
    #[cfg(feature = "redb-store")]
    if let Some(path) = args.open_store.as_deref() {
        let store = rubydex::model::store::RedbStore::open(std::path::Path::new(path)).expect("open store");
        if let Some(fqn) = args.query.as_deref() {
            match store.definition_location(fqn).expect("query store") {
                Some((uri, start)) => println!("{fqn} -> {uri} @ {start}"),
                None => println!("{fqn} -> not found"),
            }
        }
        if let Some(prefix) = args.search.as_deref() {
            let names = store.search_prefix(prefix, 20).expect("search store");
            println!("{} match(es) for prefix {prefix:?}:", names.len());
            for name in names {
                println!("  {name}");
            }
        }
        MemoryStats::print_memory_usage();
        std::process::exit(0);
    }

    // Listing

    let (file_paths, errors) = time_it!(listing, {
        listing::collect_file_paths(args.paths, &graph.excluded_patterns())
    });

    for error in errors {
        eprintln!("{error}");
    }

    if let Some(StopAfter::Listing) = args.stop_after {
        return exit(args.stats);
    }

    // Indexing

    let backend = IndexerBackend::from(&args.indexer);

    let errors = time_it!(indexing, { indexing::index_files(&mut graph, file_paths, backend) });

    for error in errors {
        eprintln!("{error}");
    }

    if let Some(StopAfter::Indexing) = args.stop_after {
        return exit(args.stats);
    }

    // Resolution

    time_it!(resolution, {
        let mut resolver = Resolver::new(&mut graph);
        resolver.resolve();
    });

    if let Some(StopAfter::Resolution) = args.stop_after {
        return exit(args.stats);
    }

    // Persist the resolved graph to an on-disk redb store.
    #[cfg(feature = "redb-store")]
    if let Some(path) = args.build_store.as_deref() {
        rubydex::model::store::RedbStore::build(std::path::Path::new(path), &graph).expect("build store");
        println!("Built redb store at {path}");
    }

    // Integrity check
    if args.check_integrity {
        let errors = time_it!(integrity_check, { integrity::check_integrity(&graph) });

        if errors.is_empty() {
            println!("Integrity check passed: no issues found");
        } else {
            eprintln!("Integrity check found {} issue(s):", errors.len());

            for error in &errors {
                eprintln!("  - {error}");
            }

            std::process::exit(1);
        }
    }

    // Querying

    if args.stats {
        time_it!(querying, {
            graph.print_query_statistics();
        });
    }

    if args.stats {
        Timer::print_breakdown();
        MemoryStats::print_memory_usage();
    }

    // Orphan report
    if let Some(ref path) = args.report_orphans {
        match std::fs::File::create(path) {
            Ok(mut file) => {
                if let Err(e) = graph.write_orphan_report(&mut file) {
                    eprintln!("Failed to write orphan report: {e}");
                } else {
                    println!("Orphan report written to {path}");
                }
            }
            Err(e) => eprintln!("Failed to create orphan report file: {e}"),
        }
    }

    // Generate visualization or print statistics
    if args.dot {
        println!("{}", dot::DotBuilder::generate(&graph, args.show_builtins));
    } else {
        println!("Indexed {} files", graph.documents().len());
        println!("Found {} names", graph.declarations().len());
        println!("Found {} definitions", graph.definitions().len());
        println!("Found {} URIs", graph.documents().len());
    }

    // Forget the graph so we don't have to wait for deallocation and let the system reclaim the memory at exit
    mem::forget(graph);
}
