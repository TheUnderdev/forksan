//! Per-session idle timers: multi-deadline, armed at end of turn (Stop),
//! cancelled on waking activity (UserPromptSubmit). Report delivery never
//! touches the clock.

use crate::daemon::{now, Daemon, SessionRuntime};
use forksan_core::moments::{idle_deadlines, ForkMoment};
use std::sync::Arc;
use std::time::Duration;

/// (Re-)arm the idle timer for a session, measuring idleness from
/// `base_activity`. Deadlines already in the past are skipped (the boot
/// sweep handles owed ones).
pub fn arm_idle_timer(daemon: &Arc<Daemon>, session_id: &str, base_activity: i64) {
    cancel_idle_timer(daemon, session_id);

    let session = {
        let store = daemon.store.lock().unwrap();
        store.get_session(session_id).ok().flatten()
    };
    let Some(session) = session else {
        return;
    };
    let cfg = daemon.cfg_for(Some(&session.project_root));
    let (entries, _) =
        forksan_core::discovery::discover_forks(&session.cwd, Some(&daemon.user_forks_root()));
    let deadlines: Vec<u64> = idle_deadlines(
        entries.iter().map(|e| &e.parsed.def),
        cfg.default_idle_deadline_secs,
    )
    .into_iter()
    .filter(|d| base_activity + *d as i64 > now())
    .collect();
    if deadlines.is_empty() {
        return;
    }

    let daemon_ref = daemon.clone();
    let sid = session_id.to_string();
    let handle = tokio::spawn(async move {
        for d in deadlines {
            let fire_at = base_activity + d as i64;
            let wait = (fire_at - now()).max(0) as u64;
            tokio::time::sleep(Duration::from_secs(wait)).await;
            tracing::info!(session = %sid, deadline = d, "idle deadline reached");
            crate::planner::run_moments(
                &daemon_ref,
                &sid,
                &[ForkMoment::Idle { deadline_secs: d }],
                None,
                None,
            )
            .await;
        }
    });

    let mut sessions = daemon.sessions.lock().unwrap();
    sessions.insert(
        session_id.to_string(),
        SessionRuntime {
            idle_timer: Some(handle),
        },
    );
}

/// Cancel any armed idle timer for the session.
pub fn cancel_idle_timer(daemon: &Arc<Daemon>, session_id: &str) {
    let mut sessions = daemon.sessions.lock().unwrap();
    if let Some(rt) = sessions.get_mut(session_id) {
        if let Some(handle) = rt.idle_timer.take() {
            handle.abort();
        }
    }
}
