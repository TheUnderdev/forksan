//! Boot sweep: on daemon start, service what a dead daemon still owed —
//! owed idle forks, boot-triggered forks, and closing sessions long past
//! the session timeout.

use crate::daemon::{now, Daemon};
use forksan_core::moments::{idle_deadlines, ForkMoment};
use std::sync::Arc;

pub async fn boot_sweep(daemon: &Arc<Daemon>) {
    let (stale, sessions) = {
        let store = daemon.store.lock().unwrap();
        let stale = store.mark_stale_runs_interrupted(now()).unwrap_or(0);
        (stale, store.list_open_sessions().unwrap_or_default())
    };
    if stale > 0 {
        tracing::warn!(count = stale, "marked stale fork runs as interrupted");
    }

    for session in sessions {
        let cfg = daemon.cfg_for(Some(&session.project_root));
        let t = now();
        let idle_secs = (t - session.last_activity).max(0) as u64;

        if cfg.session_timeout_secs > 0 && idle_secs >= cfg.session_timeout_secs {
            tracing::info!(session = %session.session_id, idle_secs, "closing timed-out session");
            crate::planner::run_moments(
                daemon,
                &session.session_id,
                &[ForkMoment::SessionEnd { manual: false }],
                None,
                None,
            )
            .await;
            let store = daemon.store.lock().unwrap();
            let _ = store.close_session(&session.session_id);
            continue;
        }

        // Owed idle deadlines: elapsed, and forks not already run after the
        // last activity (i.e. this idle period wasn't serviced pre-crash).
        let (entries, _) =
            forksan_core::discovery::discover_forks(&session.cwd, Some(&daemon.user_forks_root()));
        let deadlines = idle_deadlines(
            entries.iter().map(|e| &e.parsed.def),
            cfg.default_idle_deadline_secs,
        );
        let owed_serviced = session
            .forks_ran_at
            .is_some_and(|ran| ran > session.last_activity);
        let mut moments: Vec<ForkMoment> = Vec::new();
        if !owed_serviced {
            moments.extend(
                deadlines
                    .iter()
                    .filter(|d| **d <= idle_secs)
                    .map(|d| ForkMoment::Idle { deadline_secs: *d }),
            );
        }
        moments.push(ForkMoment::Boot);
        crate::planner::run_moments(
            daemon,
            &session.session_id,
            &moments,
            Some(session.last_activity),
            None,
        )
        .await;

        // Future deadlines still get their timers.
        crate::idle::arm_idle_timer(daemon, &session.session_id, session.last_activity);
    }
}
