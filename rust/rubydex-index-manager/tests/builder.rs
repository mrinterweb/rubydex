//! The single-flight build policy: one indexer at a time per store, events during a
//! build re-arm the next one, and a burst over a quarter of the manifest rebuilds.

use rubydex_index_manager::builder::Builder;
use rubydex_index_manager::builder::FULL_FLAG;

// The indexer argv every test uses.
fn indexer() -> Vec<String> {
    ["ruby", "-e", ""]
        .iter()
        .map(std::string::ToString::to_string)
        .collect()
}

#[test]
fn events_while_a_build_is_running_do_not_start_a_second_one() {
    let mut builder = Builder::new(indexer());
    builder.record(1);
    assert!(builder.start(100).is_some(), "the first batch starts a build");
    builder.record(50);
    assert!(
        builder.start(100).is_none(),
        "single-flight: one indexer at a time per store"
    );
}

#[test]
fn events_during_a_build_re_arm_the_next_one() {
    let mut builder = Builder::new(indexer());
    builder.record(1);
    assert!(builder.start(100).is_some(), "the first batch starts a build");
    builder.record(5);
    builder.reap();
    let argv = builder.start(100);
    assert!(
        argv.is_some(),
        "events that arrived during the build must trigger another one"
    );
    assert!(argv.unwrap() == indexer(), "the follow-up is incremental");
}

#[test]
fn a_burst_over_a_quarter_of_the_manifest_builds_full() {
    let mut builder = Builder::new(indexer());
    builder.record(25);
    let argv = builder.start(100);
    assert!(
        argv.unwrap().last().unwrap() == FULL_FLAG,
        "a burst covering a quarter of the manifest is cheaper to rebuild"
    );
}

#[test]
fn a_small_batch_builds_incrementally() {
    let mut builder = Builder::new(indexer());
    builder.record(24);
    let argv = builder.start(100);
    assert!(argv.unwrap() == indexer(), "just under the cap stays incremental");
}

#[test]
fn an_empty_batch_starts_nothing() {
    let mut builder = Builder::new(indexer());
    assert!(builder.start(100).is_none(), "no events means no build");
}
