// Temporary: per-table redb accounting for measuring the storage refactor.
#![cfg(feature = "redb-store")]

use redb::{ReadOnlyDatabase, ReadableDatabase, ReadableTableMetadata, TableDefinition};

fn main() {
    let path = std::env::args().nth(1).expect("usage: store_stats <path>");
    let db = ReadOnlyDatabase::open(&path).expect("open");
    let txn = db.begin_read().expect("read txn");
    let (mut stored, mut fragmented, mut meta) = (0, 0, 0);
    for name in [
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
        "store_metadata",
    ] {
        let stats = if let Ok(table) = txn.open_table(TableDefinition::<u64, &[u8]>::new(name)) {
            let stats = table.stats().expect("stats");
            println!(
                "{name:<20} stored {:>12} fragmented {:>12} meta {:>10} leaves {:>7}",
                stats.stored_bytes(),
                stats.fragmented_bytes(),
                stats.metadata_bytes(),
                stats.leaf_pages()
            );
            Some(stats)
        } else {
            let table = txn
                .open_table(TableDefinition::<u32, &[u8]>::new(name))
                .expect("a table keyed by u32");
            let stats = table.stats().expect("stats");
            println!(
                "{name:<20} stored {:>12} fragmented {:>12} meta {:>10} leaves {:>7}",
                stats.stored_bytes(),
                stats.fragmented_bytes(),
                stats.metadata_bytes(),
                stats.leaf_pages()
            );
            Some(stats)
        };
        if let Some(stats) = stats {
            stored += stats.stored_bytes();
            fragmented += stats.fragmented_bytes();
            meta += stats.metadata_bytes();
        }
    }
    println!("totals: stored {stored} fragmented {fragmented} meta {meta}");
    println!("file: {}", std::fs::metadata(&path).expect("metadata").len());
}
