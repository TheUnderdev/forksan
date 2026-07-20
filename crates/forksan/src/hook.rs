//! `forksan hook <event>`: the Claude Code hook entrypoint. Reads the hook
//! JSON from stdin, forwards it to the daemon, and (where the event supports
//! it) emits `hookSpecificOutput.additionalContext` JSON on stdout.
//!
//! Hooks must never break the user's session: every failure path exits 0.

use crate::client::{spawn_daemon_detached, Client};
use forksan_core::config::Paths;
use forksan_core::project::project_root;
use forksan_core::protocol::{
    Event, EventKind, ReportItem, ReportKind, RequestBody, ResponseBody, WaitMode,
};
use serde::Deserialize;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum HookKind {
    SessionStart,
    UserPromptSubmit,
    Stop,
    PreCompact,
    SessionEnd,
}

/// The subset of Claude Code hook stdin we consume. Unknown fields ignored.
#[derive(Debug, Deserialize)]
struct HookInput {
    session_id: String,
    #[serde(default)]
    transcript_path: Option<PathBuf>,
    #[serde(default)]
    cwd: Option<PathBuf>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    trigger: Option<String>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    model: Option<String>,
}

pub fn run_hook(kind: HookKind) {
    // Never break the session, whatever happens in here.
    let _ = run_hook_inner(kind);
}

fn run_hook_inner(kind: HookKind) -> Option<()> {
    // Recursion guard. A fork subprocess inherits the fork's environment,
    // which carries `FORKSAN_FORK` (and `FORKSAN_SESSION_ID`). In open
    // isolation the fork loads the user's full config, including forksan's own
    // plugin, so its hooks would re-enter the daemon and spawn forks of forks.
    // These vars are set ONLY by the runner and must NEVER be exported into a
    // real Claude Code session — their presence means "we are inside a fork",
    // so do nothing: no output, no daemon contact, no spawn.
    if std::env::var_os("FORKSAN_FORK").is_some()
        || std::env::var_os("FORKSAN_SESSION_ID").is_some()
    {
        return Some(());
    }
    let mut raw = String::new();
    use std::io::Read;
    std::io::stdin().read_to_string(&mut raw).ok()?;
    let input: HookInput = serde_json::from_str(&raw).ok()?;
    let paths = Paths::from_env()?;

    let cwd = input.cwd.clone().or_else(|| std::env::current_dir().ok())?;
    let root = project_root(&cwd);

    // Per-session tag filter, inherited from the Claude Code process env.
    let enable_tags = tags_from_env("FORKSAN_ENABLE_TAGS");
    let disable_tags = tags_from_env("FORKSAN_DISABLE_TAGS");

    let event = |ev: EventKind, wait: WaitMode| Event {
        event: ev,
        session_id: input.session_id.clone(),
        transcript_path: input.transcript_path.clone(),
        cwd: cwd.clone(),
        project_root: root.clone(),
        source: input.source.clone(),
        trigger: input.trigger.clone(),
        reason: input.reason.clone(),
        model: input.model.clone(),
        enable_tags: enable_tags.clone(),
        disable_tags: disable_tags.clone(),
        wait,
    };

    match kind {
        HookKind::SessionStart => {
            // SessionStart has slack: spawn-and-wait, retire outdated daemons.
            let client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            let mut client = client.ensure_current_version(&paths).ok()?;
            let _ = client.request(RequestBody::Event(event(
                EventKind::SessionStart,
                WaitMode::None,
            )));
            let budget = poll_budget(&paths, &root);
            if let Ok(ResponseBody::Reports { items }) = client.request(RequestBody::PollReports {
                session_id: input.session_id.clone(),
                project_root: root.clone(),
                budget_chars: budget,
            }) {
                emit_context("SessionStart", &items);
            }
        }
        HookKind::UserPromptSubmit => {
            // Hard 2s budget; never wait on a daemon spawn here.
            let Ok(mut client) = Client::connect(&paths, Duration::from_millis(1500)) else {
                spawn_daemon_detached(&paths);
                return Some(());
            };
            if let Ok(ResponseBody::Reports { items }) = client.request(RequestBody::Event(event(
                EventKind::PromptSubmit,
                WaitMode::None,
            ))) {
                emit_context("UserPromptSubmit", &items);
            }
        }
        HookKind::Stop => {
            // Runs async (Claude Code doesn't wait): fine to spawn + retire.
            let client = Client::connect_or_spawn(&paths, Duration::from_secs(10)).ok()?;
            let mut client = client.ensure_current_version(&paths).ok()?;
            let _ = client.request(RequestBody::Event(event(EventKind::Stop, WaitMode::None)));
        }
        HookKind::PreCompact => {
            let mut client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            // The daemon acks once compact forks are spawned (≤15s inside).
            let _ = client.request(RequestBody::Event(event(
                EventKind::PreCompact,
                WaitMode::ForksSpawned,
            )));
        }
        HookKind::SessionEnd => {
            let mut client = Client::connect_or_spawn(&paths, Duration::from_secs(5)).ok()?;
            let _ = client.request(RequestBody::Event(event(
                EventKind::SessionEnd,
                WaitMode::None,
            )));
        }
    }
    Some(())
}

/// Read a comma-separated tag env var into a normalized list (trimmed,
/// empties dropped, deduped). An unset or all-empty value yields `None` so the
/// daemon falls back to the config default.
fn tags_from_env(var: &str) -> Option<Vec<String>> {
    let raw = std::env::var(var).ok()?;
    let mut out: Vec<String> = Vec::new();
    for piece in raw.split(',') {
        let t = piece.trim();
        if !t.is_empty() && !out.iter().any(|e| e == t) {
            out.push(t.to_string());
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn poll_budget(paths: &Paths, root: &std::path::Path) -> usize {
    forksan_core::config::load_config_at(Some(root), &paths.user_config())
        .0
        .poll_budget_chars
}

/// Render the reports as frontmatter blocks and print the hook decision JSON.
fn emit_context(hook_event_name: &str, items: &[ReportItem]) {
    if items.is_empty() {
        return;
    }
    let blocks: Vec<String> = items
        .iter()
        .map(|item| {
            let event = match item.kind {
                ReportKind::Started => "fork_started",
                ReportKind::Response => "fork_response",
            };
            format!(
                "---\nsource: forksan\nfork: {}\ntrigger: {}\nevent: {}\n---\n{}",
                item.fork, item.trigger, event, item.body
            )
        })
        .collect();
    let payload = serde_json::json!({
        "hookSpecificOutput": {
            "hookEventName": hook_event_name,
            "additionalContext": blocks.join("\n\n"),
        }
    });
    println!("{payload}");
}
