//! The selection pipeline: turn a set of fork moments for one session into a
//! wake payload (or nothing).
//!
//! Pipeline (order matters): refresh discovery → queue roster → live re-read
//! each rostered fork → tag filter (per-session enable/disable, falling back
//! to config) → match moments → per-fork throttle → per-tag throttle →
//! once-per-session latch (context triggers) → dependency resolution → build
//! the wake payload. When a wake is issued, per-fork and per-tag throttles are
//! stamped (the daemon can't observe fork completion, so it stamps at
//! wake-issuance).

use crate::daemon::{now, Daemon};
use forksan_core::config::Config;
use forksan_core::frontmatter::{ForkParse, ForkRunOn};
use forksan_core::moments::{match_moments, ForkMoment};
use forksan_core::schedule::{resolve_deps, Selected};
use forksan_core::store::SessionRow;
use forksan_core::tags::tags_allowed;
use forksan_core::wake::{build_wake_payload, DueFork};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A fork selected to fire at the current moment.
#[derive(Clone)]
pub struct SelectedFork {
    pub name: String,
    pub path: PathBuf,
    pub trigger: String,
    pub overlap: bool,
    pub after: Vec<String>,
    pub tags: Vec<String>,
    /// True for context triggers, which fire at most once per session and so
    /// get latched at wake-issuance.
    pub once_per_session: bool,
}

impl Selected for SelectedFork {
    fn name(&self) -> &str {
        &self.name
    }
    fn after(&self) -> Vec<&str> {
        self.after.iter().map(|a| a.as_str()).collect()
    }
}

/// Refresh discovery for a session's cwd and queue every visible fork onto
/// the session's roster.
pub fn refresh_roster(daemon: &Arc<Daemon>, session_id: &str, cwd: &Path) {
    let (entries, _) =
        forksan_core::discovery::discover_forks(cwd, Some(&daemon.user_forks_root()));
    let store = daemon.store.lock().unwrap();
    let t = now();
    for entry in entries {
        if let Ok(true) = store.queue_fork(session_id, &entry.name, &entry.path, t) {
            tracing::info!(fork = %entry.name, session = session_id, "fork rostered");
        }
    }
}

/// Run the selection pipeline for `moments` and return the forks that should
/// fire (empty = nothing due). Read-only / side-effect-free: no latches or
/// throttles are stamped here (that happens at wake-issuance in [`build_wake`],
/// so a wait cancelled during the debounce window stamps nothing).
pub fn select_forks(
    daemon: &Arc<Daemon>,
    session: &SessionRow,
    cfg: &Config,
    moments: &[ForkMoment],
) -> Vec<SelectedFork> {
    refresh_roster(daemon, &session.session_id, &session.cwd);

    let roster = {
        let store = daemon.store.lock().unwrap();
        store.roster(&session.session_id).unwrap_or_default()
    };
    let effective_enable = session
        .enable_tags
        .as_deref()
        .or(cfg.enable_tags.as_deref());
    let effective_disable = session
        .disable_tags
        .as_deref()
        .or(cfg.disable_tags.as_deref());

    let mut selected: Vec<SelectedFork> = Vec::new();
    let t = now();
    for entry in roster {
        let Ok(content) = std::fs::read_to_string(&entry.fork_path) else {
            continue;
        };
        let ForkParse::Fork(parsed) = parse_fork(&entry.fork_name, &content) else {
            continue;
        };
        if !tags_allowed(&parsed.def.tags, effective_enable, effective_disable) {
            continue;
        }
        let Some(trigger) = match_moments(&parsed.def, moments, cfg.default_idle_deadline_secs)
        else {
            continue;
        };
        // Per-fork throttle.
        if let (Some(throttle), Some(ran_at)) = (parsed.def.throttle_secs, entry.ran_at) {
            if (t - ran_at).max(0) < throttle as i64 {
                tracing::debug!(fork = %entry.fork_name, "throttled, skipping");
                continue;
            }
        }
        // Per-tag shared throttle.
        if !parsed.def.tags.is_empty() && !cfg.tag_throttles.is_empty() {
            let store = daemon.store.lock().unwrap();
            let mut hit = None;
            for tag in &parsed.def.tags {
                let Some(&window) = cfg.tag_throttles.get(tag) else {
                    continue;
                };
                if let Ok(Some(last)) =
                    store.last_run_for_tags(&session.project_root, std::slice::from_ref(tag))
                {
                    if (t - last).max(0) < window as i64 {
                        hit = Some(tag.clone());
                        break;
                    }
                }
            }
            drop(store);
            if let Some(tag) = hit {
                tracing::debug!(fork = %entry.fork_name, %tag, "tag-throttled, skipping");
                continue;
            }
        }
        // Context thresholds fire at most once per session leg; skip a fork
        // already latched (read-only — the latch is consumed at issuance).
        let label = trigger.label();
        let once_per_session = matches!(
            trigger,
            ForkRunOn::ContextTokens(_) | ForkRunOn::ContextUsedPct(_) | ForkRunOn::ContextLeft(_)
        );
        if once_per_session {
            let latched = {
                let store = daemon.store.lock().unwrap();
                store
                    .is_latched(&session.session_id, &entry.fork_name, &label)
                    .unwrap_or(false)
            };
            if latched {
                continue;
            }
        }
        selected.push(SelectedFork {
            name: entry.fork_name.clone(),
            path: entry.fork_path.clone(),
            trigger: label,
            overlap: parsed.def.overlap,
            after: parsed.def.after.clone(),
            tags: parsed.def.tags.clone(),
            once_per_session,
        });
    }
    selected
}

fn parse_fork(name: &str, content: &str) -> ForkParse {
    forksan_core::frontmatter::parse_fork_file(name, content)
}

/// Given the forks selected to fire, stamp their throttles (per-fork and
/// per-tag) and build the wake payload the session should act on. Returns
/// `None` when `selected` is empty.
pub fn build_wake(
    daemon: &Arc<Daemon>,
    session: &SessionRow,
    selected: Vec<SelectedFork>,
) -> Option<String> {
    if selected.is_empty() {
        return None;
    }
    tracing::info!(
        session = %session.session_id,
        forks = ?selected.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        "issuing wake"
    );

    // Resolve `after` dependencies within the selected set.
    let deps = resolve_deps(&selected);

    // Stamp throttles and once-per-session latches at wake-issuance.
    let t = now();
    {
        let store = daemon.store.lock().unwrap();
        for sel in &selected {
            let _ = store.touch_fork_ran(&session.session_id, &sel.name, t);
            let tags_joined = (!sel.tags.is_empty()).then(|| sel.tags.join(","));
            let _ = store.record_issued_run(
                &session.session_id,
                &sel.name,
                &sel.trigger,
                tags_joined.as_deref(),
                t,
            );
            if sel.once_per_session {
                let _ = store.try_latch_fire(&session.session_id, &sel.name, &sel.trigger, t);
            }
        }
    }

    let due: Vec<DueFork> = selected
        .iter()
        .enumerate()
        .map(|(i, sel)| DueFork {
            name: sel.name.clone(),
            path: sel.path.to_string_lossy().into_owned(),
            trigger: sel.trigger.clone(),
            overlap: sel.overlap,
            after: deps[i].iter().map(|&j| selected[j].name.clone()).collect(),
        })
        .collect();

    Some(build_wake_payload(
        &session.session_id,
        &session.project_root.to_string_lossy(),
        &due,
    ))
}
