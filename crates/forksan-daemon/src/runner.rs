//! Fork execution: spawn a headless `claude -p --resume <sid> --fork-session`
//! against the parent (or a predecessor fork's) session and capture its
//! final report.
//!
//! By default (open isolation) a fork inherits the user's full config —
//! plugins, MCP servers, skills, CLAUDE.md — so its request prefix matches a
//! normal session and reuses the prompt cache. Recursion is severed at the
//! hook layer by the `FORKSAN_FORK` env guard rather than by stripping
//! config. `isolation = "hermetic"` restores the old bare fork: it avoids
//! `--bare` (which disables subscription OAuth) and instead drops settings
//! and plugins (`--setting-sources ""`), MCP servers (`--strict-mcp-config`),
//! and hooks (`disableAllHooks`).

use crate::daemon::Daemon;
use forksan_core::config::Config;
use forksan_core::frontmatter::ForkDelivery;
use forksan_core::prompt::build_fork_prompt;
use forksan_core::protocol::ReportKind;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// A fork selected to fire, carrying everything the runner needs.
#[derive(Clone)]
pub struct SelectedFork {
    pub name: String,
    pub path: std::path::PathBuf,
    pub def: forksan_core::frontmatter::ForkDef,
    pub body: String,
    pub trigger: String,
}

impl forksan_core::schedule::Selected for SelectedFork {
    fn name(&self) -> &str {
        &self.name
    }
    fn after(&self) -> Vec<&str> {
        self.def.after.iter().map(|a| a.fork.as_str()).collect()
    }
}

/// What a sequenced fork knows about the fork it ran after.
#[derive(Clone)]
pub struct Predecessor {
    pub name: String,
    pub reply: String,
    /// The predecessor fork's own session id (for `context: fork`).
    pub fork_session_id: Option<String>,
}

pub struct RunOutcome {
    pub reply: String,
    pub fork_session_id: Option<String>,
}

/// Retries for the transcript-flush race: a session-end fork can spawn while
/// the quitting parent is still finalizing a large transcript, so `--resume`
/// can't find the conversation yet.
const RESUME_RETRIES: u32 = 3;

/// Base backoff (ms) between resume retries; doubles each attempt (2s, 4s, 8s
/// by default). Overridable via `FORKSAN_RETRY_BASE_MS` (tests short-circuit
/// the sleeps this way).
fn retry_base_ms() -> u64 {
    std::env::var("FORKSAN_RETRY_BASE_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2000)
}

/// The one error class worth retrying: the resume target isn't on disk yet.
fn is_resume_not_ready(err: &str) -> bool {
    err.contains("No conversation found")
}

/// The parsed shape of `claude -p --output-format json` stdout.
#[derive(Debug, Default)]
struct ClaudeResult {
    result: String,
    session_id: Option<String>,
    total_cost_usd: Option<f64>,
    is_error: bool,
}

fn parse_claude_output(stdout: &str) -> Option<ClaudeResult> {
    // The output is a single JSON object; be tolerant of leading noise by
    // also trying the last line.
    let candidates = [stdout.trim(), stdout.trim().lines().last().unwrap_or("")];
    for c in candidates {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(c) {
            return Some(ClaudeResult {
                result: v
                    .get("result")
                    .and_then(|r| r.as_str())
                    .unwrap_or_default()
                    .to_string(),
                session_id: v
                    .get("session_id")
                    .and_then(|s| s.as_str())
                    .map(String::from),
                total_cost_usd: v.get("total_cost_usd").and_then(|c| c.as_f64()),
                is_error: v.get("is_error").and_then(|e| e.as_bool()).unwrap_or(false),
            });
        }
    }
    None
}

/// Run one fork. Inserts the run + boundary reports, spawns the subprocess
/// (signalling `spawned` as soon as the child exists), enforces the timeout,
/// and returns the outcome for `after` sequencing. Failures produce a
/// failure report (unless delivery is `discard`) and return `None`.
///
/// Unless the fork opted into `overlap`, the fire is skipped entirely
/// (returning `None`) when a run of the same fork is still in flight for
/// this project — the fork fires again at its next moment, whose context
/// will include this run's delivered report.
#[allow(clippy::too_many_arguments)]
pub async fn run_one_fork(
    daemon: &Arc<Daemon>,
    cfg: &Config,
    session_id: &str,
    project_root: &Path,
    cwd: &Path,
    sel: &SelectedFork,
    predecessors: &[Predecessor],
    spawned: Option<tokio::sync::oneshot::Sender<()>>,
) -> Option<RunOutcome> {
    let mut spawned = spawned;
    let _gate = if sel.def.overlap {
        None
    } else {
        match daemon.run_gates.try_start(project_root, &sel.name) {
            Some(token) => Some(token),
            None => {
                // A run of this fork is still in flight: cancel this fire;
                // the next moment's fire will see its delivered report.
                tracing::info!(fork = %sel.name, "previous run still in flight, skipping this fire");
                if let Some(tx) = spawned.take() {
                    let _ = tx.send(());
                }
                return None;
            }
        }
    };

    let now = crate::daemon::now();
    let deliver = sel.def.delivery != ForkDelivery::Discard;

    // The run records its fork's tags (comma-joined) so per-tag throttles can
    // find the last run per tag.
    let tags_joined = (!sel.def.tags.is_empty()).then(|| sel.def.tags.join(","));
    let run_id = {
        let store = daemon.store.lock().unwrap();
        let run_id = store
            .insert_run(
                session_id,
                &sel.name,
                &sel.trigger,
                tags_joined.as_deref(),
                now,
            )
            .ok()?;
        let _ = store.touch_fork_ran(session_id, &sel.name, now);
        if deliver {
            let _ = store.insert_report(
                Some(run_id),
                session_id,
                project_root,
                &sel.name,
                &sel.trigger,
                ReportKind::Started,
                &format!("Fork '{}' started (trigger: {}).", sel.name, sel.trigger),
                now,
            );
        }
        run_id
    };

    // `context: fork` sequencing: resume that predecessor fork's session
    // when available, else fall back to the parent. Its report is not piped
    // (the dependent sees its whole context); every other predecessor's
    // report is.
    let fork_ctx_dep = sel
        .def
        .after
        .iter()
        .find(|a| a.context == forksan_core::frontmatter::ForkAfterContext::Fork);
    let resume_target = fork_ctx_dep
        .and_then(|a| predecessors.iter().find(|p| p.name == a.fork))
        .and_then(|p| p.fork_session_id.clone())
        .unwrap_or_else(|| session_id.to_string());
    let piped: Vec<(String, String)> = predecessors
        .iter()
        .filter(|p| fork_ctx_dep.is_none_or(|a| a.fork != p.name))
        .map(|p| (p.name.clone(), p.reply.clone()))
        .collect();

    let prompt = build_fork_prompt(
        &sel.name,
        &sel.path.to_string_lossy(),
        &sel.body,
        &sel.trigger,
        sel.def.delivery,
        &piped,
    );

    // A fresh command per attempt (a retry consumes the previous one).
    let build_cmd = || {
        let mut cmd = tokio::process::Command::new(&cfg.claude_bin);
        cmd.arg("-p")
            .arg("--resume")
            .arg(&resume_target)
            .arg("--fork-session")
            .arg("--output-format")
            .arg("json")
            .current_dir(cwd)
            .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
            .env("DISABLE_AUTOUPDATER", "1")
            .env("DISABLE_TELEMETRY", "1")
            .env("DISABLE_ERROR_REPORTING", "1")
            // The parent session's identity, so a fork can key per-session
            // state on disk deterministically (cwd alone is not unique).
            .env("FORKSAN_SESSION_ID", session_id)
            .env("FORKSAN_FORK_NAME", &sel.name)
            .env("FORKSAN_TRIGGER", &sel.trigger)
            .env("FORKSAN_PROJECT_ROOT", project_root)
            // Recursion guard: the fork loads the user's full config in open
            // mode (including forksan's own plugin), so its inherited hooks
            // would re-enter the daemon. `FORKSAN_FORK` makes every forksan
            // hook inside the fork bail out immediately.
            .env("FORKSAN_FORK", "1")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .process_group(0);
        // Hermetic isolation (opt-in) strips the user's plugins, MCP servers,
        // and settings-derived hooks so the fork runs bare. Open mode (the
        // default) keeps them for prompt-cache reuse; recursion is severed by
        // the `FORKSAN_FORK` guard above instead.
        if cfg.isolation == forksan_core::config::Isolation::Hermetic {
            cmd.arg("--setting-sources")
                .arg("")
                .arg("--strict-mcp-config")
                .arg("--settings")
                .arg(r#"{"disableAllHooks":true}"#);
        }
        if let Some(model) = &sel.def.model {
            cmd.arg("--model").arg(model);
        }
        // Fork permissions: a headless fork can't answer prompts, so grant it
        // the rules it needs up front. `--allowedTools` is variadic.
        if !sel.def.allowed_tools.is_empty() {
            cmd.arg("--allowedTools").args(&sel.def.allowed_tools);
        }
        // Permission mode: the fork's own value wins, else the config default.
        if let Some(mode) = sel
            .def
            .permission_mode
            .as_deref()
            .or(cfg.permission_mode.as_deref())
        {
            cmd.arg("--permission-mode").arg(mode);
        }
        cmd
    };

    daemon
        .active_runs
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    // A session-end fork can outrun the parent's transcript flush: the resume
    // target isn't on disk yet and `claude` exits with "No conversation
    // found". Retry just that error class, with backoff, while the parent
    // finishes writing — one logical run throughout (the run row, tags, and
    // started marker were already recorded once above).
    let retry_base = retry_base_ms();
    let mut attempt: u32 = 0;
    let result = loop {
        let r = run_subprocess(daemon, cfg, build_cmd(), &prompt, spawned.take()).await;
        match &r {
            Err(e) if is_resume_not_ready(e) && attempt < RESUME_RETRIES => {
                let delay = retry_base.saturating_mul(1u64 << attempt);
                tracing::info!(
                    fork = %sel.name,
                    attempt = attempt + 1,
                    delay_ms = delay,
                    "resume target not ready yet (No conversation found), retrying"
                );
                tokio::time::sleep(Duration::from_millis(delay)).await;
                attempt += 1;
            }
            _ => break r,
        }
    };
    daemon
        .active_runs
        .fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
    daemon.touch_busy();

    let finished = crate::daemon::now();
    match result {
        Ok(parsed) if !parsed.is_error => {
            {
                let store = daemon.store.lock().unwrap();
                let _ = store.finish_run(
                    run_id,
                    "done",
                    parsed.session_id.as_deref(),
                    parsed.total_cost_usd,
                    None,
                    finished,
                );
                if deliver {
                    let _ = store.insert_report(
                        Some(run_id),
                        session_id,
                        project_root,
                        &sel.name,
                        &sel.trigger,
                        ReportKind::Response,
                        &parsed.result,
                        finished,
                    );
                }
            }
            tracing::info!(fork = %sel.name, trigger = %sel.trigger, cost = ?parsed.total_cost_usd, "fork run done");
            Some(RunOutcome {
                reply: parsed.result,
                fork_session_id: parsed.session_id,
            })
        }
        Ok(parsed) => {
            fail_run(
                daemon,
                run_id,
                session_id,
                project_root,
                sel,
                deliver,
                "model reported an error",
                &parsed.result,
                finished,
            );
            None
        }
        Err(err) => {
            fail_run(
                daemon,
                run_id,
                session_id,
                project_root,
                sel,
                deliver,
                &err,
                "",
                finished,
            );
            None
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn fail_run(
    daemon: &Arc<Daemon>,
    run_id: i64,
    session_id: &str,
    project_root: &Path,
    sel: &SelectedFork,
    deliver: bool,
    err: &str,
    detail: &str,
    finished: i64,
) {
    tracing::warn!(fork = %sel.name, error = %err, "fork run failed");
    let store = daemon.store.lock().unwrap();
    let _ = store.finish_run(run_id, "failed", None, None, Some(err), finished);
    if deliver {
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!(" Details: {detail}")
        };
        let _ = store.insert_report(
            Some(run_id),
            session_id,
            project_root,
            &sel.name,
            &sel.trigger,
            ReportKind::Response,
            &format!("Fork '{}' failed: {err}.{detail}", sel.name),
            finished,
        );
    }
}

async fn run_subprocess(
    daemon: &Arc<Daemon>,
    cfg: &Config,
    mut cmd: tokio::process::Command,
    prompt: &str,
    spawned: Option<tokio::sync::oneshot::Sender<()>>,
) -> Result<ClaudeResult, String> {
    let mut child = cmd
        .spawn()
        .map_err(|e| format!("spawn {}: {e}", cfg.claude_bin))?;
    if let Some(tx) = spawned {
        let _ = tx.send(());
    }
    daemon.touch_busy();

    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    stdin
        .write_all(prompt.as_bytes())
        .await
        .map_err(|e| format!("write prompt: {e}"))?;
    drop(stdin);

    let output = tokio::time::timeout(
        Duration::from_secs(cfg.fork_timeout_secs),
        child.wait_with_output(),
    )
    .await;

    match output {
        Err(_) => Err(format!("timed out after {}s", cfg.fork_timeout_secs)),
        Ok(Err(e)) => Err(format!("wait: {e}")),
        Ok(Ok(out)) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if !out.status.success() {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let head: String = stderr.chars().take(500).collect();
                return Err(format!("exit {}: {head}", out.status));
            }
            parse_claude_output(&stdout).ok_or_else(|| "unparsable output".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_result_json() {
        let out = r#"{"type":"result","subtype":"success","result":"the report","session_id":"abc","total_cost_usd":0.42,"is_error":false}"#;
        let p = parse_claude_output(out).unwrap();
        assert_eq!(p.result, "the report");
        assert_eq!(p.session_id.as_deref(), Some("abc"));
        assert_eq!(p.total_cost_usd, Some(0.42));
        assert!(!p.is_error);
        assert!(parse_claude_output("garbage").is_none());
        // Tolerates leading noise lines.
        let noisy = format!("warning: something\n{out}");
        assert_eq!(parse_claude_output(&noisy).unwrap().result, "the report");
    }
}
