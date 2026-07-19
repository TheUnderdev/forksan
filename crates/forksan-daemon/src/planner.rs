//! The selection pipeline and plan execution for one set of fork moments.
//!
//! Pipeline (order matters): refresh discovery → queue
//! roster → live re-read each rostered fork → match moments → skip-ran-after
//! guard (boot sweep) → throttle → context/boot latch → dependency layering
//! → execution (leader alone first for provider cache warming, then bounded
//! concurrency; `after` chains sequential inside their root's slot).

use crate::daemon::{now, Daemon};
use crate::runner::{run_one_fork, Predecessor, SelectedFork};
use forksan_core::frontmatter::parse_fork_file;
use forksan_core::moments::{match_moments, ForkMoment};
use forksan_core::schedule::layer_dependencies;
use std::path::Path;
use std::pin::Pin;
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

    let (roots, children) = layer_dependencies(&selected);
    let nodes = Arc::new(selected);
    let children = Arc::new(children);

    if let Some(tx) = spawned {
        // Snapshot mode (PreCompact): every root spawns concurrently and the
        // barrier fires once all subprocesses exist — no leader-first, no
        // concurrency cap, so compaction can't outrun the snapshots.
        let mut spawn_waits = Vec::new();
        let mut joins = Vec::new();
        for &root in &roots {
            let (stx, srx) = tokio::sync::oneshot::channel();
            spawn_waits.push(srx);
            joins.push(tokio::spawn(run_chain(
                daemon.clone(),
                cfg.clone(),
                session.session_id.clone(),
                session.project_root.clone(),
                session.cwd.clone(),
                nodes.clone(),
                children.clone(),
                root,
                None,
                Some(stx),
            )));
        }
        for w in spawn_waits {
            let _ = w.await;
        }
        let _ = tx.send(());
        for j in joins {
            let _ = j.await;
        }
    } else {
        // Leader runs alone first: the provider cache entry for the shared
        // context prefix only exists once a first response begins.
        if let Some(&leader) = roots.first() {
            run_chain(
                daemon.clone(),
                cfg.clone(),
                session.session_id.clone(),
                session.project_root.clone(),
                session.cwd.clone(),
                nodes.clone(),
                children.clone(),
                leader,
                None,
                None,
            )
            .await;
        }
        let semaphore = Arc::new(tokio::sync::Semaphore::new(cfg.concurrency));
        let mut joins = Vec::new();
        for &root in roots.iter().skip(1) {
            let permit = semaphore.clone().acquire_owned().await;
            let daemon = daemon.clone();
            let cfg = cfg.clone();
            let sid = session.session_id.clone();
            let proot = session.project_root.clone();
            let cwd = session.cwd.clone();
            let nodes = nodes.clone();
            let children = children.clone();
            joins.push(tokio::spawn(async move {
                let _permit = permit;
                run_chain(
                    daemon, cfg, sid, proot, cwd, nodes, children, root, None, None,
                )
                .await;
            }));
        }
        for j in joins {
            let _ = j.await;
        }
    }

    let store = daemon.store.lock().unwrap();
    let _ = store.set_forks_ran_at(session_id, now());
}

/// Run one fork, then its `after` dependents sequentially (each seeing this
/// fork's report — or forking its session for `context: fork`). Boxed for
/// recursion.
#[allow(clippy::too_many_arguments)]
fn run_chain(
    daemon: Arc<Daemon>,
    cfg: forksan_core::config::Config,
    session_id: String,
    project_root: std::path::PathBuf,
    cwd: std::path::PathBuf,
    nodes: Arc<Vec<SelectedFork>>,
    children: Arc<Vec<Vec<usize>>>,
    idx: usize,
    predecessor: Option<Predecessor>,
    spawned: Option<tokio::sync::oneshot::Sender<()>>,
) -> Pin<Box<dyn std::future::Future<Output = ()> + Send>> {
    Box::pin(async move {
        let sel = &nodes[idx];
        let outcome = run_one_fork(
            &daemon,
            &cfg,
            &session_id,
            &project_root,
            &cwd,
            sel,
            predecessor.as_ref(),
            spawned,
        )
        .await;

        let pred = outcome.map(|o| Predecessor {
            name: sel.name.clone(),
            reply: o.reply,
            fork_session_id: o.fork_session_id,
        });
        for &child in &children[idx] {
            // Failed predecessor: dependents still run, just without a report.
            run_chain(
                daemon.clone(),
                cfg.clone(),
                session_id.clone(),
                project_root.clone(),
                cwd.clone(),
                nodes.clone(),
                children.clone(),
                child,
                pred.clone(),
                None,
            )
            .await;
        }
    })
}
