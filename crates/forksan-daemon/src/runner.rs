//! Fork execution: spawn a headless `claude -p --resume <sid> --fork-session`
//! against the parent (or a predecessor fork's) session and capture its
//! final report.
//!
//! Isolation deliberately avoids `--bare` (it disables subscription OAuth):
//! `--setting-sources ""` drops user/project settings (and with them hooks
//! and plugins), `--strict-mcp-config` drops MCP servers, and
//! `disableAllHooks` belts-and-suspenders the hook recursion.

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

    let mut cmd = tokio::process::Command::new(&cfg.claude_bin);
    cmd.arg("-p")
        .arg("--resume")
        .arg(&resume_target)
        .arg("--fork-session")
        .arg("--output-format")
        .arg("json")
        .arg("--setting-sources")
        .arg("")
        .arg("--strict-mcp-config")
        .arg("--settings")
        .arg(r#"{"disableAllHooks":true}"#)
        .current_dir(cwd)
        .env("CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC", "1")
        .env("DISABLE_AUTOUPDATER", "1")
        .env("DISABLE_TELEMETRY", "1")
        .env("DISABLE_ERROR_REPORTING", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .process_group(0);
    if let Some(model) = &sel.def.model {
        cmd.arg("--model").arg(model);
    }

    daemon
        .active_runs
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let result = run_subprocess(daemon, cfg, cmd, &prompt, spawned).await;
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
