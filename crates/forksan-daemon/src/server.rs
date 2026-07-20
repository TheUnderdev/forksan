//! Unix-socket JSONL server: one request line in, one response line out.
//! A `StopWait` request may block for a long time (the asyncRewake Stop hook's
//! long poll) before its single response is written.

use crate::daemon::Daemon;
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
        RequestBody::StopWait(ev) => daemon.handle_stop_wait(ev).await,
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
                    throttle_secs: e.parsed.def.throttle_secs,
                    after: e.parsed.def.after.clone(),
                    overlap: e.parsed.def.overlap,
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
    let recent_runs = store
        .list_runs(&["issued"], 20)
        .unwrap_or_default()
        .into_iter()
        .map(|r| RunInfo {
            fork: r.fork_name,
            trigger: r.trigger_label,
            session_id: r.session_id,
            state: r.state,
            started_at: r.started_at,
        })
        .collect();
    ResponseBody::StatusInfo(StatusInfo {
        version: Daemon::version().to_string(),
        daemon_proto: PROTO_VERSION,
        pid: std::process::id(),
        sessions,
        recent_runs,
    })
}
