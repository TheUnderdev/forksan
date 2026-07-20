//! Per-fork run gates: by default two runs of the same fork never overlap.
//!
//! Keyed by (project root, fork name) — a session resume changes the session
//! id mid-conversation, and two sessions in one project touch the same
//! files, so the project is the right serialization boundary.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Gate {
    lock: Arc<tokio::sync::Mutex<()>>,
    /// Whether a fire is already parked waiting on `lock`.
    waiting: AtomicBool,
}

#[derive(Default)]
pub struct RunGates {
    gates: Mutex<HashMap<(PathBuf, String), Arc<Gate>>>,
}

impl RunGates {
    /// Serialize a run of `fork` within `project`: waits for any running
    /// instance to finish, then returns the held guard. Returns `None` when
    /// another fire is already waiting — the caller should skip entirely
    /// (fork bodies are idempotent; the parked fire will run against fresh
    /// state, so stacking a third run adds nothing).
    pub async fn acquire(
        &self,
        project: &Path,
        fork: &str,
    ) -> Option<tokio::sync::OwnedMutexGuard<()>> {
        let gate = {
            let mut gates = self.gates.lock().unwrap();
            gates
                .entry((project.to_path_buf(), fork.to_string()))
                .or_insert_with(|| Arc::new(Gate::default()))
                .clone()
        };
        if gate.waiting.swap(true, Ordering::SeqCst) {
            return None;
        }
        let guard = gate.lock.clone().lock_owned().await;
        gate.waiting.store(false, Ordering::SeqCst);
        Some(guard)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn serializes_and_coalesces() {
        let gates = Arc::new(RunGates::default());
        let project = Path::new("/p");

        // First fire runs immediately.
        let g1 = gates.acquire(project, "f").await.expect("first fire runs");

        // Second fire parks; third coalesces away while second waits.
        let gates2 = gates.clone();
        let waiter =
            tokio::spawn(async move { gates2.acquire(Path::new("/p"), "f").await.is_some() });
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished(), "second fire must wait");
        assert!(
            gates.acquire(project, "f").await.is_none(),
            "third fire coalesces"
        );

        // Different fork / different project are independent.
        assert!(gates.acquire(project, "other").await.is_some());
        assert!(gates.acquire(Path::new("/q"), "f").await.is_some());

        // Releasing lets the parked fire through, and the gate resets.
        drop(g1);
        assert!(waiter.await.unwrap(), "waiter runs after release");
        // (waiter's guard dropped with its task) — a fresh fire runs.
        assert!(gates.acquire(project, "f").await.is_some());
    }
}
