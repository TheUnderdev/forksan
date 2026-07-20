//! The selection pipeline and plan execution for one set of fork moments.
//!
//! Pipeline (order matters): refresh discovery → queue
//! roster → live re-read each rostered fork → tag filter (per-session
//! enable/disable, falling back to config) → match moments → skip-ran-after
//! guard (boot sweep) → throttle → context/boot latch → dependency
//! resolution → execution (a readiness-counted DAG: each fork runs once all
//! its `after` dependencies finished, receiving their reports; the first
//! root runs alone for provider cache warming, the rest under the
//! concurrency cap).

use crate::daemon::{now, Daemon};
use crate::runner::{run_one_fork, Predecessor, SelectedFork};
use forksan_core::frontmatter::parse_fork_file;
use forksan_core::moments::{match_moments, ForkMoment};
use forksan_core::schedule::resolve_deps;
use forksan_core::tags::tags_allowed;
use std::path::Path;
use std::sync::Arc;

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

/// Run every rostered fork that matches `moments` for this session.
///
/// `skip_ran_after`: boot-sweep guard — skip forks whose `ran_at` is after
/// this cutoff (they already ran for this idle period before a daemon
/// death). `spawned`: fired once every root fork's subprocess exists (the
/// PreCompact snapshot barrier); sending implies plan selection is final.
pub async fn run_moments(
    daemon: &Arc<Daemon>,
    session_id: &str,
    moments: &[ForkMoment],
    skip_ran_after: Option<i64>,
    spawned: Option<tokio::sync::oneshot::Sender<()>>,
) {
    let Some(session) = ({
        let store = daemon.store.lock().unwrap();
        store.get_session(session_id).ok().flatten()
    }) else {
        if let Some(tx) = spawned {
            let _ = tx.send(());
        }
        return;
    };
    let cfg = daemon.cfg_for(Some(&session.project_root));

    refresh_roster(daemon, session_id, &session.cwd);

    // Selection.
    let roster = {
        let store = daemon.store.lock().unwrap();
        store.roster(session_id).unwrap_or_default()
    };
    // Effective tag filter for this session: the session's own values (from
    // the hook env) if set, else the config defaults.
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
        // Live re-read: the file is the source of truth at fire time.
        let Ok(content) = std::fs::read_to_string(&entry.fork_path) else {
            continue;
        };
        let Some(parsed) = parse_fork_file(&entry.fork_name, &content) else {
            continue;
        };
        // Per-session enable/disable tag filter (manual runs bypass this).
        if !tags_allowed(&parsed.def.tags, effective_enable, effective_disable) {
            continue;
        }
        let Some(trigger) = match_moments(&parsed.def, moments, cfg.default_idle_deadline_secs)
        else {
            continue;
        };
        if let (Some(cutoff), Some(ran_at)) = (skip_ran_after, entry.ran_at) {
            if ran_at > cutoff {
                continue;
            }
        }
        if let (Some(throttle), Some(ran_at)) = (parsed.def.throttle_secs, entry.ran_at) {
            if (t - ran_at).max(0) < throttle as i64 {
                tracing::debug!(fork = %entry.fork_name, "throttled, skipping");
                continue;
            }
        }
        // Context thresholds and boot fire at most once per session leg.
        let label = trigger.label();
        let once_per_session = matches!(
            trigger,
            forksan_core::frontmatter::ForkRunOn::ContextTokens(_)
                | forksan_core::frontmatter::ForkRunOn::ContextUsedPct(_)
                | forksan_core::frontmatter::ForkRunOn::ContextLeft(_)
                | forksan_core::frontmatter::ForkRunOn::Boot
        );
        if once_per_session {
            let latched = {
                let store = daemon.store.lock().unwrap();
                store
                    .try_latch_fire(session_id, &entry.fork_name, &label, t)
                    .unwrap_or(false)
            };
            if !latched {
                continue;
            }
        }
        selected.push(SelectedFork {
            name: entry.fork_name.clone(),
            path: entry.fork_path.clone(),
            def: parsed.def,
            body: parsed.body,
            trigger: label,
        });
    }

    if selected.is_empty() {
        if let Some(tx) = spawned {
            let _ = tx.send(());
        }
        return;
    }
    tracing::info!(
        session = session_id,
        forks = ?selected.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
        "fork moment firing"
    );

    let deps = resolve_deps(&selected);
    run_plan(daemon, &cfg, &session, selected, deps, spawned).await;

    let store = daemon.store.lock().unwrap();
    let _ = store.set_forks_ran_at(session_id, now());
}

/// Execute a resolved plan: each fork runs as soon as all its dependencies
/// finished (readiness counting), receiving their outcomes as predecessors.
/// Without a barrier the first root runs alone (the provider cache entry for
/// the shared context prefix only exists once a first response begins) and
/// the rest respect the concurrency cap; in barrier mode (PreCompact) every
/// root spawns immediately, uncapped, and `spawned` fires once all their
/// subprocesses exist so compaction can't outrun the snapshots.
async fn run_plan(
    daemon: &Arc<Daemon>,
    cfg: &forksan_core::config::Config,
    session: &forksan_core::store::SessionRow,
    nodes: Vec<SelectedFork>,
    deps: Vec<Vec<usize>>,
    spawned: Option<tokio::sync::oneshot::Sender<()>>,
) {
    let n = nodes.len();
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    for (i, d) in deps.iter().enumerate() {
        for &p in d {
            dependents[p].push(i);
        }
    }
    let mut remaining: Vec<usize> = deps.iter().map(|d| d.len()).collect();
    let roots = forksan_core::schedule::roots(&deps);
    let barrier_mode = spawned.is_some();

    let nodes = Arc::new(nodes);
    let outcomes: Arc<std::sync::Mutex<Vec<Option<Predecessor>>>> =
        Arc::new(std::sync::Mutex::new(vec![None; n]));
    let (done_tx, mut done_rx) = tokio::sync::mpsc::unbounded_channel::<usize>();
    let cap = if barrier_mode {
        n.max(1)
    } else {
        cfg.concurrency
    };
    let semaphore = Arc::new(tokio::sync::Semaphore::new(cap));

    let spawn_node = |idx: usize, spawn_sig: Option<tokio::sync::oneshot::Sender<()>>| {
        let daemon = daemon.clone();
        let cfg = cfg.clone();
        let sid = session.session_id.clone();
        let proot = session.project_root.clone();
        let cwd = session.cwd.clone();
        let nodes = nodes.clone();
        let deps_of = deps[idx].clone();
        let outcomes = outcomes.clone();
        let done_tx = done_tx.clone();
        let semaphore = semaphore.clone();
        tokio::spawn(async move {
            let _permit = semaphore.acquire_owned().await;
            // Dependencies are complete by construction; failed ones simply
            // contribute no predecessor.
            let preds: Vec<Predecessor> = {
                let o = outcomes.lock().unwrap();
                deps_of.iter().filter_map(|d| o[*d].clone()).collect()
            };
            let outcome = run_one_fork(
                &daemon,
                &cfg,
                &sid,
                &proot,
                &cwd,
                &nodes[idx],
                &preds,
                spawn_sig,
            )
            .await;
            if let Some(o) = outcome {
                outcomes.lock().unwrap()[idx] = Some(Predecessor {
                    name: nodes[idx].name.clone(),
                    reply: o.reply,
                    fork_session_id: o.fork_session_id,
                });
            }
            let _ = done_tx.send(idx);
        });
    };

    let mut completed = 0usize;
    if barrier_mode {
        let mut waits = Vec::new();
        for &root in &roots {
            let (stx, srx) = tokio::sync::oneshot::channel();
            waits.push(srx);
            spawn_node(root, Some(stx));
        }
        for w in waits {
            let _ = w.await;
        }
        if let Some(tx) = spawned {
            let _ = tx.send(());
        }
    } else {
        // Leader alone first, then the remaining roots.
        if let Some(&leader) = roots.first() {
            spawn_node(leader, None);
            if let Some(idx) = done_rx.recv().await {
                completed += 1;
                for &dep in &dependents[idx] {
                    remaining[dep] -= 1;
                    if remaining[dep] == 0 {
                        spawn_node(dep, None);
                    }
                }
            }
        }
        for &root in roots.iter().skip(1) {
            spawn_node(root, None);
        }
    }

    while completed < n {
        let Some(idx) = done_rx.recv().await else {
            break;
        };
        completed += 1;
        for &dep in &dependents[idx] {
            remaining[dep] -= 1;
            if remaining[dep] == 0 {
                spawn_node(dep, None);
            }
        }
    }
}
