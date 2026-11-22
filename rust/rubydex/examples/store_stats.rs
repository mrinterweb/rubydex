// Temporary: per-table redb accounting for measuring the storage refactor.
#[cfg(feature = "redb-store")]
mod imp {
    use redb::{ReadOnlyDatabase, ReadableDatabase, ReadableTableMetadata, TableDefinition};

    const TABLES: [&str; 10] = [
        "strings",
        "declarations",
        "definitions",
        "names",
        "constant_references",
        "method_references",
        "documents",
        "name_dependents",
        "search_names",
        "document_uris",
    ];

    fn stats_of(txn: &redb::ReadTransaction, name: &str) -> redb::TableStats {
        if let Ok(table) = txn.open_table(TableDefinition::<u64, &[u8]>::new(name)) {
            return table.stats().expect("stats");
        }
        let table = txn
            .open_table(TableDefinition::<u32, &[u8]>::new(name))
            .expect("a table keyed by u32");
        table.stats().expect("stats")
    }

    pub fn run(path: &str) {
        let db = ReadOnlyDatabase::open(path).expect("open");
        let txn = db.begin_read().expect("read txn");
        let (mut stored, mut fragmented, mut meta) = (0_u64, 0_u64, 0_u64);
        for name in TABLES {
            let stats = stats_of(&txn, name);
            println!(
                "{name:<20} stored {:>12} fragmented {:>12} meta {:>10} leaves {:>7}",
                stats.stored_bytes(),
                stats.fragmented_bytes(),
                stats.metadata_bytes(),
                stats.leaf_pages()
            );
            stored += stats.stored_bytes();
            fragmented += stats.fragmented_bytes();
            meta += stats.metadata_bytes();
        }
        println!("totals: stored {stored} fragmented {fragmented} meta {meta}");
        println!("file: {}", std::fs::metadata(path).expect("metadata").len());
    }
}

#[cfg(feature = "redb-store")]
fn main() {
    let path = std::env::args().nth(1).expect("usage: store_stats <path>");
    imp::run(&path);
    if std::env::args().any(|arg| arg == "--scan") {
        time_scan(&path);
    }
}

#[cfg(not(feature = "redb-store"))]
fn main() {}

#[cfg(feature = "redb-store")]
#[allow(dead_code)]
fn time_scan(path: &str) {
    use rubydex::model::graph::Graph;
    use rubydex::model::store::RedbStore;

    let graph = Graph::with_store(RedbStore::open(std::path::Path::new(path)).expect("open"));
    let start = std::time::Instant::now();
    let mut n = 0;
    for _ in 0..20 {
        n = graph
            .store_declaration_ids_matching(&|_, name| name.contains("arbs"))
            .len();
    }
    println!("scan-only contains(arbs): {:?}/op ({n} matches)", start.elapsed() / 20);

    let start = std::time::Instant::now();
    for _ in 0..20 {
        n = graph.store_declaration_ids_matching(&|_, name| !name.is_empty()).len();
    }
    println!("scan-only all: {:?}/op ({n} matches)", start.elapsed() / 20);

    let fuzzy = |name: &str| {
        let mut query = "arbs".chars().peekable();
        for character in name.chars() {
            if let Some(&wanted) = query.peek()
                && wanted.eq_ignore_ascii_case(&character)
            {
                query.next();
            }
        }
        query.peek().is_none()
    };

    let start = std::time::Instant::now();
    for _ in 0..20 {
        n = graph
            .store_declaration_ids_matching(&|_, name| fuzzy(name))
            .into_iter()
            .filter_map(|id| graph.declaration(id))
            .count();
    }
    println!("fuzzy scan + point reads (FQN order): {:?}/op ({n} matches)", start.elapsed() / 20);

    let start = std::time::Instant::now();
    let mut sorted = 0;
    for _ in 0..20 {
        let mut ids = graph.store_declaration_ids_matching(&|_, name| fuzzy(name));
        ids.sort_unstable_by_key(rubydex::model::id::Id::get);
        n = ids.into_iter().filter_map(|id| graph.declaration(id)).count();
        sorted += n;
    }
    println!("fuzzy scan + sort + point reads (id order): {:?}/op ({n} matches, {sorted})", start.elapsed() / 20);
}
