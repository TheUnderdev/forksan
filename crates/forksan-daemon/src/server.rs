//! Unix-socket JSONL server: one request line in, one response line out.

use crate::daemon::{now, Daemon};
use crate::runner::SelectedFork;
use forksan_core::protocol::{
    encode, ErrorCode, ForkInfo, Request, RequestBody, Response, ResponseBody, RunInfo,
    SessionInfo, StatusInfo,
};
use forksan_core::PROTO_VERSION;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

pub async fn serve(daemon: Arc<Daemon>, listener: UnixListener) {
    loop {
        tokio::select! {
            _ = daemon.shutdown.notified() => return,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue };
                let daemon = daemon.clone();
                tokio::spawn(async move {
                    daemon.connections.fetch_add(1, Ordering::SeqCst);
                    daemon.touch_busy();
                    handle_conn(&daemon, stream).await;
                    daemon.connections.fetch_sub(1, Ordering::SeqCst);
                    daemon.touch_busy();
                });
            }
        }
    }
}

async fn handle_conn(daemon: &Arc<Daemon>, stream: UnixStream) {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if line.trim().is_empty() {
            continue;
        }
        let (id, body) = match serde_json::from_str::<Request>(&line) {
            Ok(req) => {
                let id = req.id;
                // The frozen shutdown frame works across any proto skew; all
                // other bodies require a proto match.
                let mismatch =
                    req.proto != PROTO_VERSION && !matches!(req.body, RequestBody::Shutdown { .. });
                if mismatch {
                    (
                        id,
                        ResponseBody::Error {
                            code: ErrorCode::ProtoMismatch,
                            message: format!(
                                "daemon speaks proto {PROTO_VERSION}, client sent {}",
                                req.proto
                            ),
                        },
                    )
                } else {
                    (id, dispatch(daemon, req.body).await)
                }
            }
            Err(e) => (
                0,
                ResponseBody::Error {
                    code: ErrorCode::BadRequest,
                    message: format!("bad request: {e}"),
                },
            ),
        };
        let resp = Response {
            proto: PROTO_VERSION,
            id,
            body,
        };
        let Ok(line) = encode(&resp) else { break };
        if write.write_all(line.as_bytes()).await.is_err() {
            break;
        }
    }
}

async fn dispatch(daemon: &Arc<Daemon>, body: RequestBody) -> ResponseBody {
    daemon.touch_busy();
    match body {
        RequestBody::Hello { version } => {
            tracing::debug!(client = %version, "hello");
            ResponseBody::HelloInfo {
                version: Daemon::version().to_string(),
            }
        }
        RequestBody::Event(ev) => daemon.handle_event(ev).await,
        RequestBody::PollReports {
            session_id,
            project_root,
            budget_chars,
        } => {
            let items = {
                let store = daemon.store.lock().unwrap();
                store
                    .poll_reports(&session_id, &project_root, budget_chars, now())
                    .unwrap_or_default()
            };
            ResponseBody::Reports { items }
        }
        RequestBody::RunFork {
            name,
            project_root,
            cwd,
            session_id,
        } => run_fork_manually(daemon, name, project_root, cwd, session_id).await,
        RequestBody::Status => status(daemon),
        RequestBody::ListForks {
            project_root: _,
            cwd,
        } => {
            let (entries, warnings) =
                forksan_core::discovery::discover_forks(&cwd, Some(&daemon.user_forks_root()));
            let items = entries
                .into_iter()
                .map(|e| ForkInfo {
                    name: e.name,
                    path: e.path,
                    description: e.parsed.def.description.clone(),
                    triggers: e.parsed.def.run_on.iter().map(|r| r.label()).collect(),
                    delivery: match e.parsed.def.delivery {
                        forksan_core::frontmatter::ForkDelivery::Discard => "discard".into(),
                        forksan_core::frontmatter::ForkDelivery::NextTurn => "next_turn".into(),
                    },
                    throttle_secs: e.parsed.def.throttle_secs,
                    after: e.parsed.def.after.iter().map(|a| a.fork.clone()).collect(),
                    overlap: e.parsed.def.overlap,
                    model: e.parsed.def.model.clone(),
                    tags: e.parsed.def.tags.clone(),
                    warnings: e
                        .parsed
                        .warnings
                        .iter()
                        .chain(warnings.iter())
                        .cloned()
                        .collect(),
                })
                .collect();
            ResponseBody::ForkList { items }
        }
        RequestBody::Shutdown { drain } => {
            tracing::info!(drain, "shutdown requested");
            let daemon = daemon.clone();
            tokio::spawn(async move {
                daemon.request_shutdown(drain).await;
            });
            ResponseBody::Ack
        }
    }
}

async fn run_fork_manually(
    daemon: &Arc<Daemon>,
    name: String,
    project_root: std::path::PathBuf,
    cwd: std::path::PathBuf,
    session_id: Option<String>,
) -> ResponseBody {
    let session = {
        let store = daemon.store.lock().unwrap();
        match &session_id {
            Some(sid) => store.get_session(sid).ok().flatten(),
            None => store.most_recent_open_session(&project_root).ok().flatten(),
        }
    };
    let Some(session) = session else {
        return ResponseBody::Error {
            code: ErrorCode::NotFound,
            message: "no open session for this project (start a Claude Code session first)".into(),
        };
    };
    let (entries, _) =
        forksan_core::discovery::discover_forks(&cwd, Some(&daemon.user_forks_root()));
    let Some(entry) = entries.into_iter().find(|e| e.name == name) else {
        return ResponseBody::Error {
            code: ErrorCode::NotFound,
            message: format!("no fork named '{name}' visible from {}", cwd.display()),
        };
    };
    let sel = SelectedFork {
        name: entry.name.clone(),
        path: entry.path.clone(),
        def: entry.parsed.def.clone(),
        body: entry.parsed.body.clone(),
        trigger: "manual".into(),
    };
    let cfg = daemon.cfg_for(Some(&session.project_root));
    let daemon_ref = daemon.clone();
    let sid = session.session_id.clone();
    let proot = session.project_root.clone();
    let scwd = session.cwd.clone();
    {
        let store = daemon.store.lock().unwrap();
        let _ = store.queue_fork(&sid, &sel.name, &sel.path, now());
    }
    tokio::spawn(async move {
        crate::runner::run_one_fork(&daemon_ref, &cfg, &sid, &proot, &scwd, &sel, &[], None).await;
    });
    ResponseBody::RunStarted {
        fork: name,
        session_id: session.session_id,
    }
}

fn status(daemon: &Arc<Daemon>) -> ResponseBody {
    let store = daemon.store.lock().unwrap();
    let sessions = store
        .list_open_sessions()
        .unwrap_or_default()
        .into_iter()
        .map(|s| SessionInfo {
            session_id: s.session_id,
            project_root: s.project_root,
            status: s.status.as_str().to_string(),
            last_activity: s.last_activity,
            prompt_tokens: s.prompt_tokens,
        })
        .collect();
    let to_info = |r: forksan_core::store::RunRow| RunInfo {
        fork: r.fork_name,
        trigger: r.trigger_label,
        session_id: r.session_id,
        state: r.state,
        started_at: r.started_at,
        finished_at: r.finished_at,
        cost_usd: r.cost_usd,
        error: r.error,
    };
    let running = store
        .list_runs(&["running"], 50)
        .unwrap_or_default()
        .into_iter()
        .map(to_info)
        .collect();
    let recent_runs = store
        .list_runs(&["done", "failed", "interrupted"], 20)
        .unwrap_or_default()
        .into_iter()
        .map(to_info)
        .collect();
    ResponseBody::StatusInfo(StatusInfo {
        version: Daemon::version().to_string(),
        daemon_proto: PROTO_VERSION,
        pid: std::process::id(),
        sessions,
        running,
        recent_runs,
        queued_reports: store.count_pending_reports().unwrap_or(0),
    })
}
