//! Per-fork in-flight tracking: by default (`overlap: false`) a fork moment
//! firing while a previous run of the same fork is still going is simply
//! skipped — the fork fires again at its next moment, by which time the
//! previous run's report has reached the parent conversation (reports are
//! delivered at the parent's next prompt, and e.g. an idle deadline only
//! re-fires after such activity), so the fresh fork context contains it
//! natively.
//!
//! Keyed by (project root, fork name) — a session resume changes the session
//! id mid-conversation, and two sessions in one project touch the same
//! files, so the project is the right serialization boundary.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

#[derive(Default)]
pub struct RunGates {
    running: Arc<Mutex<HashSet<(PathBuf, String)>>>,
}

/// Marks a fork run as in flight for its lifetime; dropping it releases.
pub struct RunToken {
    running: Arc<Mutex<HashSet<(PathBuf, String)>>>,
    key: (PathBuf, String),
}

impl Drop for RunToken {
    fn drop(&mut self) {
        self.running.lock().unwrap().remove(&self.key);
    }
}

impl RunGates {
    /// Claim (project, fork) as running. `None` when a run is already in
    /// flight — the caller should skip this fire entirely.
    pub fn try_start(&self, project: &Path, fork: &str) -> Option<RunToken> {
        let key = (project.to_path_buf(), fork.to_string());
        let mut running = self.running.lock().unwrap();
        if !running.insert(key.clone()) {
            return None;
        }
        Some(RunToken {
            running: self.running.clone(),
            key,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn skips_while_in_flight_and_releases_on_drop() {
        let gates = RunGates::default();
        let project = Path::new("/p");

        let t1 = gates.try_start(project, "f").expect("first fire runs");
        assert!(
            gates.try_start(project, "f").is_none(),
            "second fire skipped"
        );
        assert!(
            gates.try_start(project, "f").is_none(),
            "third fire skipped"
        );

        // Different fork / different project are independent.
        assert!(gates.try_start(project, "other").is_some());
        assert!(gates.try_start(Path::new("/q"), "f").is_some());

        // Finishing releases: the next moment's fire runs again.
        drop(t1);
        assert!(gates.try_start(project, "f").is_some());
    }
}
