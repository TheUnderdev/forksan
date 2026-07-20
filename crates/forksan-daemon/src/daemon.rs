//! Shared daemon state and Claude Code event handling.

use forksan_core::config::{load_config_at, Config, Paths};
use forksan_core::moments::ForkMoment;
use forksan_core::protocol::{Event, EventKind, ResponseBody, WaitMode};
use forksan_core::store::Store;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub struct SessionRuntime {
    pub idle_timer: Option<tokio::task::JoinHandle<()>>,
}

pub struct Daemon {
    pub paths: Paths,
    pub store: Mutex<Store>,
    pub sessions: Mutex<HashMap<String, SessionRuntime>>,
    /// Per-(project, fork) serialization gates (`overlap: false`).
    pub run_gates: crate::gates::RunGates,
    pub active_runs: AtomicUsize,
    pub connections: AtomicUsize,
    pub last_busy: AtomicI64,
    pub draining: AtomicBool,
    pub shutdown: tokio::sync::Notify,
}

impl Daemon {
    pub fn new(paths: Paths, store: Store) -> Arc<Self> {
        Arc::new(Self {
            paths,
            store: Mutex::new(store),
            sessions: Mutex::new(HashMap::new()),
            run_gates: crate::gates::RunGates::default(),
            active_runs: AtomicUsize::new(0),
            connections: AtomicUsize::new(0),
            last_busy: AtomicI64::new(now()),
            draining: AtomicBool::new(false),
            shutdown: tokio::sync::Notify::new(),
        })
    }

    pub fn touch_busy(&self) {
        self.last_busy.store(now(), Ordering::SeqCst);
    }

    /// The user-level forks root (`<base>/forks`).
    pub fn user_forks_root(&self) -> PathBuf {
        self.paths.base.join("forks")
    }

    /// Effective config for a project.
    pub fn cfg_for(&self, project_root: Option<&Path>) -> Config {
        load_config_at(project_root, &self.paths.user_config()).0
    }

    pub fn version() -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// Handle one lifecycle event; returns the response body.
    pub async fn handle_event(self: &Arc<Self>, ev: Event) -> ResponseBody {
        self.touch_busy();
        let t = now();
        // The per-session tag filter rides on every event; persist it
        // comma-joined so the latest event's values win on the session row.
        let enable_tags = ev.enable_tags.as_ref().map(|v| v.join(","));
        let disable_tags = ev.disable_tags.as_ref().map(|v| v.join(","));
        match ev.event {
            EventKind::SessionStart => {
                {
                    let store = self.store.lock().unwrap();
                    let _ = store.upsert_session(
                        &ev.session_id,
                        &ev.project_root,
                        &ev.cwd,
                        ev.transcript_path.as_deref(),
                        ev.model.as_deref(),
                        enable_tags.as_deref(),
                        disable_tags.as_deref(),
                        t,
                    );
                }
                // `startup`/`clear` are genuinely new sessions; `resume`
                // re-registers under the new leg, `compact` is a
                // continuation — neither re-fires session_start.
                let fire = matches!(ev.source.as_deref(), Some("startup") | Some("clear") | None);
                if fire {
                    let daemon = self.clone();
                    let sid = ev.session_id.clone();
                    tokio::spawn(async move {
                        crate::planner::run_moments(
                            &daemon,
                            &sid,
                            &[ForkMoment::SessionStart],
                            None,
                            None,
                        )
                        .await;
                    });
                }
                crate::idle::arm_idle_timer(self, &ev.session_id, t);
                ResponseBody::Ack
            }
            EventKind::PromptSubmit => {
                {
                    let store = self.store.lock().unwrap();
                    let _ = store.upsert_session(
                        &ev.session_id,
                        &ev.project_root,
                        &ev.cwd,
                        ev.transcript_path.as_deref(),
                        ev.model.as_deref(),
                        enable_tags.as_deref(),
                        disable_tags.as_deref(),
                        t,
                    );
                    let _ = store.set_last_activity(&ev.session_id, t);
                }
                // A turn is in flight: idle deadlines must not fire mid-turn.
                crate::idle::cancel_idle_timer(self, &ev.session_id);
                let cfg = self.cfg_for(Some(&ev.project_root));
                let items = {
                    let store = self.store.lock().unwrap();
                    store
                        .poll_reports(&ev.session_id, &ev.project_root, cfg.poll_budget_chars, t)
                        .unwrap_or_default()
                };
                ResponseBody::Reports { items }
            }
            EventKind::Stop => {
                {
                    let store = self.store.lock().unwrap();
                    let _ = store.upsert_session(
                        &ev.session_id,
                        &ev.project_root,
                        &ev.cwd,
                        ev.transcript_path.as_deref(),
                        ev.model.as_deref(),
                        enable_tags.as_deref(),
                        disable_tags.as_deref(),
                        t,
                    );
                    let _ = store.set_last_activity(&ev.session_id, t);
                }
                // Context gauge from the transcript, then context-threshold
                // moments (latched once per session inside the planner).
                if let Some(transcript) = ev.transcript_path.as_deref() {
                    let session = {
                        let store = self.store.lock().unwrap();
                        store.get_session(&ev.session_id).ok().flatten()
                    };
                    if let Some(session) = session {
                        match crate::transcript::read_gauge(transcript, session.transcript_offset) {
                            Ok(gauge) => {
                                {
                                    let store = self.store.lock().unwrap();
                                    let _ = store.set_transcript_gauge(
                                        &ev.session_id,
                                        gauge.new_offset,
                                        gauge.prompt_tokens,
                                    );
                                }
                                let tokens = gauge.prompt_tokens.or(session.prompt_tokens);
                                if let Some(prompt_tokens) = tokens {
                                    let cfg = self.cfg_for(Some(&ev.project_root));
                                    let model = gauge.model.as_deref().or(session.model.as_deref());
                                    let max_tokens = Some(cfg.window_for(model));
                                    let daemon = self.clone();
                                    let sid = ev.session_id.clone();
                                    tokio::spawn(async move {
                                        crate::planner::run_moments(
                                            &daemon,
                                            &sid,
                                            &[ForkMoment::Context {
                                                prompt_tokens,
                                                max_tokens,
                                            }],
                                            None,
                                            None,
                                        )
                                        .await;
                                    });
                                }
                            }
                            Err(e) => {
                                tracing::debug!(error = %e, "transcript gauge unavailable");
                            }
                        }
                    }
                }
                // End of turn: the idle clock starts now.
                crate::idle::arm_idle_timer(self, &ev.session_id, t);
                ResponseBody::Ack
            }
            EventKind::PreCompact => {
                let (tx, rx) = tokio::sync::oneshot::channel();
                let daemon = self.clone();
                let sid = ev.session_id.clone();
                tokio::spawn(async move {
                    crate::planner::run_moments(
                        &daemon,
                        &sid,
                        &[ForkMoment::Compact],
                        None,
                        Some(tx),
                    )
                    .await;
                });
                if ev.wait == WaitMode::ForksSpawned {
                    // Ack once every matching fork's subprocess exists (they
                    // snapshot the parent transcript at spawn), so compaction
                    // may rewrite it freely afterward. Capped.
                    let _ = tokio::time::timeout(Duration::from_secs(15), rx).await;
                }
                ResponseBody::Ack
            }
            EventKind::SessionEnd => {
                crate::idle::cancel_idle_timer(self, &ev.session_id);
                let (last_activity, deadline) = {
                    let store = self.store.lock().unwrap();
                    let last = store
                        .get_session(&ev.session_id)
                        .ok()
                        .flatten()
                        .map(|s| s.last_activity)
                        .unwrap_or(t);
                    (
                        last,
                        self.cfg_for(Some(&ev.project_root))
                            .default_idle_deadline_secs,
                    )
                };
                // Manual stop = closed while recently active: the idle forks
                // never got their chance. The SessionEnd reason is metadata
                // only (a `clear` after hours of idling is not manual).
                let idle_secs = (t - last_activity).max(0) as u64;
                let manual = deadline == 0 || idle_secs < deadline;
                let daemon = self.clone();
                let sid = ev.session_id.clone();
                tokio::spawn(async move {
                    crate::planner::run_moments(
                        &daemon,
                        &sid,
                        &[ForkMoment::SessionEnd { manual }],
                        None,
                        None,
                    )
                    .await;
                    // Close only after the plan ran (it needs the roster);
                    // closing frees this session's reports for delivery to
                    // successor sessions in the same project.
                    let store = daemon.store.lock().unwrap();
                    let _ = store.close_session(&sid);
                });
                ResponseBody::Ack
            }
        }
    }

    /// True when the daemon has nothing to live for right now.
    pub fn is_quiet(&self) -> bool {
        if self.active_runs.load(Ordering::SeqCst) > 0
            || self.connections.load(Ordering::SeqCst) > 0
        {
            return false;
        }
        let sessions = self.sessions.lock().unwrap();
        !sessions
            .values()
            .any(|s| s.idle_timer.as_ref().is_some_and(|h| !h.is_finished()))
    }

    /// Exit once quiet for the configured period.
    pub async fn quiet_reaper(self: Arc<Self>) {
        loop {
            tokio::time::sleep(Duration::from_secs(30)).await;
            let quiet_period = self.cfg_for(None).quiet_period_secs as i64;
            let quiet_since = now() - self.last_busy.load(Ordering::SeqCst);
            if self.is_quiet() && quiet_since >= quiet_period {
                tracing::info!("quiet for {quiet_since}s, exiting");
                self.shutdown.notify_waiters();
                return;
            }
        }
    }

    /// Begin shutdown; with `drain`, wait for in-flight fork runs first.
    pub async fn request_shutdown(self: &Arc<Self>, drain: bool) {
        self.draining.store(true, Ordering::SeqCst);
        if drain {
            let deadline = Duration::from_secs(self.cfg_for(None).fork_timeout_secs + 30);
            let start = std::time::Instant::now();
            while self.active_runs.load(Ordering::SeqCst) > 0 && start.elapsed() < deadline {
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
        self.shutdown.notify_waiters();
    }
}
