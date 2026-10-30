// Single-flight build policy for one store: the manager never runs two indexers
// for the same store, and a burst that touches a quarter of the manifest is cheaper
// to rebuild than to patch.

// A burst matching this fraction of the manifest triggers a full rebuild.
const REBUILD_DIFF_RATIO: usize = 4;
// Appended to the indexer argv when the burst is big enough for a full rebuild.
pub const FULL_FLAG: &str = "--full";

pub struct Builder {
    argv: Vec<String>,
    child: Option<std::process::Child>,
    re_arm: bool,
    pending: usize,
}

impl Builder {
    #[must_use]
    pub fn new(argv: Vec<String>) -> Builder {
        Builder {
            argv,
            child: None,
            re_arm: false,
            pending: 0,
        }
    }

    /// Record a batch of file-system events for this store.
    pub fn record(&mut self, events: usize) {
        self.pending += events;
    }

    /// Wait for the running indexer, then let a recorded burst re-arm the next one.
    pub fn reap(&mut self) {
        let running = self.child.take();
        if let Some(mut child) = running {
            let status = child.wait();
            if let Ok(exit) = status
                && !exit.success()
            {
                eprintln!("rubydex-index-manager: the indexer exited {exit}");
            }
            if status.is_err() {
                eprintln!("rubydex-index-manager: the indexer failed: {status:?}");
            }
            if self.re_arm {
                self.re_arm = false;
                self.pending = self.pending.max(1);
            }
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

        let events = self.pending;
        let full = manifest > 0 && events * REBUILD_DIFF_RATIO >= manifest;
        let argv = if full {
            let mut full_argv = self.argv.clone();
            full_argv.push(FULL_FLAG.to_string());
            full_argv
        } else {
            self.argv.clone()
        };

        let spawned = std::process::Command::new(argv[0].clone()).args(&argv[1..]).spawn();
        let Ok(spawned) = spawned else {
            eprintln!("rubydex-index-manager: could not start the indexer: {argv:?}");
            return None; // `pending` still holds the batch, so the next event retries it
        };
        self.pending = 0;
        self.child = Some(spawned);
        Some(argv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        builder.reap();
        let argv = builder.start(10).expect("the burst re-arms a build");
        assert!(
            argv.iter().any(|arg| arg == FULL_FLAG),
            "nine events is more than a quarter of a ten-file manifest"
        );
    }
}
