//! The CLI ↔ daemon wire protocol: newline-delimited JSON, one request line
//! per response line. Every frame carries `proto` and `id`.
//!
//! Compatibility rule: the `shutdown` request shape is frozen forever at
//! proto 1, so any future CLI can always retire any past daemon.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A request frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    pub proto: u32,
    pub id: u64,
    #[serde(flatten)]
    pub body: RequestBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestBody {
    /// Version handshake.
    Hello { version: String },
    /// A Claude Code lifecycle event, forwarded from a hook.
    Event(Event),
    /// Fetch pending fork reports for delivery as additionalContext.
    PollReports {
        session_id: String,
        project_root: PathBuf,
        budget_chars: usize,
    },
    /// Manually fire one fork (the `forksan run` command).
    RunFork {
        name: String,
        project_root: PathBuf,
        cwd: PathBuf,
        /// Target session; defaults to the project's most recent open one.
        session_id: Option<String>,
    },
    /// Daemon + session + run status (the `forksan status` command).
    Status,
    /// Discovered forks for a project (the `forksan forks list` command).
    ListForks { project_root: PathBuf, cwd: PathBuf },
    /// Ask the daemon to exit. With `drain`, it finishes in-flight fork runs
    /// first. Frozen shape — never change.
    Shutdown { drain: bool },
}

/// A Claude Code lifecycle event.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub event: EventKind,
    pub session_id: String,
    pub transcript_path: Option<PathBuf>,
    pub cwd: PathBuf,
    pub project_root: PathBuf,
    /// SessionStart source (`startup`/`resume`/`clear`/`compact`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// PreCompact trigger (`manual`/`auto`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub trigger: Option<String>,
    /// SessionEnd reason (`clear`/`logout`/`prompt_input_exit`/`other`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    /// The session's model id (SessionStart provides it), for per-model
    /// context-window lookup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// What the hook wants to wait for before the daemon acks.
    #[serde(default)]
    pub wait: WaitMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    SessionStart,
    PromptSubmit,
    Stop,
    PreCompact,
    SessionEnd,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitMode {
    /// Ack as soon as the event is recorded.
    #[default]
    None,
    /// Ack once every matching fork's subprocess has been spawned (used by
    /// PreCompact so forks snapshot pre-compaction context).
    ForksSpawned,
}

/// A response frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub proto: u32,
    pub id: u64,
    #[serde(flatten)]
    pub body: ResponseBody,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResponseBody {
    Ack,
    HelloInfo { version: String },
    Reports { items: Vec<ReportItem> },
    StatusInfo(StatusInfo),
    ForkList { items: Vec<ForkInfo> },
    RunStarted { fork: String, session_id: String },
    Error { code: ErrorCode, message: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    ProtoMismatch,
    BadRequest,
    NotFound,
    Internal,
}

/// One queued fork report ready for injection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReportItem {
    pub fork: String,
    pub trigger: String,
    pub kind: ReportKind,
    pub body: String,
    /// Unix epoch seconds.
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReportKind {
    Started,
    Response,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusInfo {
    pub version: String,
    /// The daemon's protocol version (the frame's own `proto` field is the
    /// transport's; keep the names distinct — this struct is flattened).
    pub daemon_proto: u32,
    pub pid: u32,
    pub sessions: Vec<SessionInfo>,
    pub running: Vec<RunInfo>,
    pub recent_runs: Vec<RunInfo>,
    pub queued_reports: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    pub session_id: String,
    pub project_root: PathBuf,
    pub status: String,
    /// Unix epoch seconds.
    pub last_activity: i64,
    pub prompt_tokens: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunInfo {
    pub fork: String,
    pub trigger: String,
    pub session_id: String,
    pub state: String,
    /// Unix epoch seconds.
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ForkInfo {
    pub name: String,
    pub path: PathBuf,
    pub description: Option<String>,
    pub triggers: Vec<String>,
    pub delivery: String,
    pub throttle_secs: Option<u64>,
    #[serde(default)]
    pub after: Vec<String>,
    #[serde(default)]
    pub overlap: bool,
    pub model: Option<String>,
    pub warnings: Vec<String>,
}

/// Serialize a frame as one JSONL line (with trailing newline).
pub fn encode<T: Serialize>(frame: &T) -> Result<String, serde_json::Error> {
    let mut s = serde_json::to_string(frame)?;
    s.push('\n');
    Ok(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_event() {
        let req = Request {
            proto: crate::PROTO_VERSION,
            id: 7,
            body: RequestBody::Event(Event {
                event: EventKind::PreCompact,
                session_id: "abc".into(),
                transcript_path: Some("/t.jsonl".into()),
                cwd: "/p".into(),
                project_root: "/p".into(),
                source: None,
                trigger: Some("auto".into()),
                reason: None,
                model: None,
                wait: WaitMode::ForksSpawned,
            }),
        };
        let line = encode(&req).unwrap();
        assert!(line.ends_with('\n'));
        let back: Request = serde_json::from_str(line.trim()).unwrap();
        assert_eq!(back.id, 7);
        match back.body {
            RequestBody::Event(e) => {
                assert_eq!(e.event, EventKind::PreCompact);
                assert_eq!(e.wait, WaitMode::ForksSpawned);
            }
            _ => panic!("wrong body"),
        }
    }

    #[test]
    fn shutdown_shape_is_frozen() {
        // Guard: this exact JSON must parse forever.
        let line = r#"{"proto":1,"id":1,"type":"shutdown","drain":true}"#;
        let req: Request = serde_json::from_str(line).unwrap();
        match req.body {
            RequestBody::Shutdown { drain } => assert!(drain),
            _ => panic!("wrong body"),
        }
    }
}
