// Single-flight build policy for one store: the manager never runs two indexers
// for the same store, and a burst that touches a quarter of the manifest is cheaper
// to rebuild than to patch.

use std::time::{Duration, Instant};

// A burst matching this fraction of the manifest triggers a full rebuild.
const REBUILD_DIFF_RATIO: usize = 4;
// Appended to the indexer argv when the burst is big enough for a full rebuild.
pub const FULL_FLAG: &str = "--full";
// A spawn that fails (broken install, missing interpreter) is not retried on every pass: the loop
// turns over once per debounce window, so an immediate retry spends the manager on a child that
// cannot start and writes its stderr continuously.
const SPAWN_RETRY_INTERVAL: Duration = Duration::from_secs(5);

pub struct Builder {
    argv: Vec<String>,
    child: Option<std::process::Child>,
    re_arm: bool,
    pending: usize,
    /// When a failed spawn may be retried. `None` once a spawn succeeds or before the first one.
    next_attempt: Option<Instant>,
}

impl Builder {
    #[must_use]
    pub fn new(argv: Vec<String>) -> Builder {
        Builder {
            argv,
            child: None,
            re_arm: false,
            pending: 0,
            next_attempt: None,
        }
    }

    /// Record a batch of file-system events for this store.
    pub fn record(&mut self, events: usize) {
        self.pending += events;
    }

    /// Whether an indexer is still running.
    #[must_use]
    pub fn is_running(&self) -> bool {
        self.child.is_some()
    }

    /// Collect the running indexer if it has finished, then let a recorded burst re-arm the next one.
    ///
    /// Never blocks: the manager's only loop calls this for every watch, so waiting here would stop it
    /// pruning sessions, syncing newly registered workspaces and draining event batches for the whole
    /// length of a build — and forever if the child hung.
    pub fn reap(&mut self) {
        let Some(child) = self.child.as_mut() else {
            return;
        };
        match child.try_wait() {
            Ok(None) => return,
            Ok(Some(exit)) => {
                if !exit.success() {
                    eprintln!("rubydex-index-manager: the indexer exited {exit}");
                }
            }
            Err(error) => eprintln!("rubydex-index-manager: the indexer failed: {error:?}"),
        }
        self.child = None;
        if self.re_arm {
            self.re_arm = false;
            self.pending = self.pending.max(1);
        }
    }

    /// The indexer argv started now, or None when nothing is warranted.
    /// Events recorded while a build runs never start a second one; they re-arm the next.
    pub fn start(&mut self, manifest: usize) -> Option<Vec<String>> {
        if self.child.is_some() {
            if self.pending > 0 {
                // Keep the count: a branch switch during a build must still be able to ask for a
                // full rebuild when the running child exits.
                self.re_arm = true;
            }
            return None;
        }
        if self.pending == 0 {
            return None;
        }
        // A spawn that just failed keeps its batch, and this is what stops the loop retrying it on
        // every pass while it cannot start.
        if self.next_attempt.is_some_and(|at| at > Instant::now()) {
            return None;
        }

        let events = self.pending;
        let full = manifest > 0 && events * REBUILD_DIFF_RATIO >= manifest;
        let argv = if full {
            let mut full_argv = self.argv.clone();
            full_argv.push(FULL_FLAG.to_string());
            full_argv
        } else {
            self.argv.clone()
        };

        let Some((executable, arguments)) = argv.split_first() else {
            // A session that registered an empty builder can never index anything, and keeping the
            // batch would retry it on every pass forever.
            self.pending = 0;
            return None;
        };
        let spawned = std::process::Command::new(executable).args(arguments).spawn();
        let Ok(spawned) = spawned else {
            eprintln!("rubydex-index-manager: could not start the indexer: {argv:?}");
            // `pending` still holds the batch, so the retry is not lost — only spaced out.
            self.next_attempt = Some(Instant::now() + SPAWN_RETRY_INTERVAL);
            return None;
        };
        self.next_attempt = None;
        self.pending = 0;
        self.child = Some(spawned);
        Some(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `reap` is non-blocking, so a test that wants the child collected has to poll for it.
    fn reap_until_done(builder: &mut Builder) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while builder.is_running() && Instant::now() < deadline {
            builder.reap();
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !builder.is_running(),
            "the indexer must be collected before the next start"
        );
    }

    #[test]
    fn a_burst_during_a_build_keeps_its_size_for_the_next_start() {
        let mut builder = Builder::new(vec!["ruby".into(), "-e".into(), String::new(), "--".into()]);

        builder.record(1);
        assert!(builder.start(10).is_some(), "the first event starts a build");

        // A branch switch touches nine more files while the child is still running.
        builder.record(9);
        assert!(
            builder.start(10).is_none(),
            "the manager never runs two indexers at once"
        );
        assert_eq!(builder.pending, 9, "the burst size must survive the running child");

        reap_until_done(&mut builder);
        let argv = builder.start(10).expect("the burst re-arms a build");
        assert!(
            argv.iter().any(|arg| arg == FULL_FLAG),
            "nine events is more than a quarter of a ten-file manifest"
        );
    }

    #[test]
    fn reap_does_not_block_on_a_running_indexer() {
        let mut builder = Builder::new(vec!["ruby".into(), "-e".into(), "sleep 1".into(), "--".into()]);
        builder.record(1);
        assert!(builder.start(10).is_some());

        let started = Instant::now();
        builder.reap();
        assert!(
            started.elapsed() < Duration::from_millis(500),
            "the manager's loop must not wait"
        );
        assert!(builder.is_running(), "a running child stays owned by the builder");
    }

    #[test]
    fn a_failed_spawn_is_not_retried_on_every_pass() {
        let mut builder = Builder::new(vec!["/nonexistent/rubydex-indexer".into()]);
        builder.record(1);
        assert!(builder.start(10).is_none(), "a spawn that cannot start starts nothing");
        assert_eq!(builder.pending, 1, "the batch is kept for the retry");

        builder.record(1);
        assert!(
            builder.start(10).is_none(),
            "the retry waits for the backoff instead of running every pass"
        );
    }
}
