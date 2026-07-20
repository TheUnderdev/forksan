//! User-facing commands: status, forks, run, logs, doctor.

use crate::client::Client;
use forksan_core::config::Paths;
use forksan_core::project::project_root;
use forksan_core::protocol::{RequestBody, ResponseBody};
use std::time::Duration;

fn connect(paths: &Paths) -> Result<Client, String> {
    Client::connect_or_spawn(paths, Duration::from_secs(5)).map_err(|e| e.to_string())
}

fn fmt_ago(now: i64, ts: i64) -> String {
    let d = (now - ts).max(0);
    match d {
        0..=59 => format!("{d}s ago"),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86400),
    }
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

pub fn status(paths: &Paths) -> Result<(), String> {
    let mut client = connect(paths)?;
    let ResponseBody::StatusInfo(info) = client
        .request(RequestBody::Status)
        .map_err(|e| e.to_string())?
    else {
        return Err("unexpected response".into());
    };
    let t = now();
    println!("forksan daemon v{} (pid {})", info.version, info.pid);
    println!(
        "sessions: {}   running forks: {}   queued reports: {}",
        info.sessions.len(),
        info.running.len(),
        info.queued_reports
    );
    for s in &info.sessions {
        let tokens = s
            .prompt_tokens
            .map(|n| format!(", ~{n} prompt tokens"))
            .unwrap_or_default();
        println!(
            "  session {} [{}] {} (active {}{tokens})",
            &s.session_id[..s.session_id.len().min(8)],
            s.status,
            s.project_root.display(),
            fmt_ago(t, s.last_activity),
        );
    }
    if !info.running.is_empty() {
        println!("running:");
        for r in &info.running {
            println!(
                "  {} ({}) started {}",
                r.fork,
                r.trigger,
                fmt_ago(t, r.started_at)
            );
        }
    }
    if !info.recent_runs.is_empty() {
        println!("recent runs:");
        for r in &info.recent_runs {
            let cost = r.cost_usd.map(|c| format!(" ${c:.4}")).unwrap_or_default();
            let err = r
                .error
                .as_deref()
                .map(|e| format!(" — {e}"))
                .unwrap_or_default();
            println!(
                "  {} ({}) [{}]{cost} {}{err}",
                r.fork,
                r.trigger,
                r.state,
                fmt_ago(t, r.finished_at.unwrap_or(r.started_at)),
            );
        }
    }
    Ok(())
}

pub fn list_forks(paths: &Paths, project: Option<std::path::PathBuf>) -> Result<(), String> {
    let cwd = project
        .or_else(|| std::env::current_dir().ok())
        .ok_or("cannot resolve cwd")?;
    let root = project_root(&cwd);
    let mut client = connect(paths)?;
    let ResponseBody::ForkList { items } = client
        .request(RequestBody::ListForks {
            project_root: root.clone(),
            cwd,
        })
        .map_err(|e| e.to_string())?
    else {
        return Err("unexpected response".into());
    };
    if items.is_empty() {
        println!("no forks discovered (looked for .forksan/forks/ up from here and user-level)");
        return Ok(());
    }
    println!("forks visible from {} :", root.display());
    for f in &items {
        println!(
            "  {} — {}",
            f.name,
            f.description.as_deref().unwrap_or("(no description)")
        );
        let mut details = vec![format!("runs on: {}", f.triggers.join(", "))];
        details.push(format!("delivery: {}", f.delivery));
        if let Some(t) = f.throttle_secs {
            details.push(format!("throttle: {t}s"));
        }
        if !f.after.is_empty() {
            details.push(format!("after: {}", f.after.join(", ")));
        }
        if f.overlap {
            details.push("overlap allowed".into());
        }
        if let Some(m) = &f.model {
            details.push(format!("model: {m}"));
        }
        println!("      {}", details.join(" | "));
        println!("      {}", f.path.display());
        for w in &f.warnings {
            println!("      warning: {w}");
        }
    }
    Ok(())
}

pub fn run_fork(paths: &Paths, name: String, session: Option<String>) -> Result<(), String> {
    let cwd = std::env::current_dir().map_err(|e| e.to_string())?;
    let root = project_root(&cwd);
    let mut client = connect(paths)?;
    match client
        .request(RequestBody::RunFork {
            name,
            project_root: root,
            cwd,
            session_id: session,
        })
        .map_err(|e| e.to_string())?
    {
        ResponseBody::RunStarted { fork, session_id } => {
            println!("fork '{fork}' started against session {session_id}");
            println!("watch it with: forksan status");
            Ok(())
        }
        ResponseBody::Error { message, .. } => Err(message),
        _ => Err("unexpected response".into()),
    }
}

pub fn logs(paths: &Paths, follow: bool) -> Result<(), String> {
    let path = paths.daemon_log();
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let tail: Vec<&str> = content.lines().rev().take(100).collect();
    for line in tail.iter().rev() {
        println!("{line}");
    }
    if follow {
        use std::io::{Read, Seek, SeekFrom};
        let mut file = std::fs::File::open(&path).map_err(|e| e.to_string())?;
        let mut pos = file.metadata().map_err(|e| e.to_string())?.len();
        loop {
            std::thread::sleep(Duration::from_millis(500));
            let len = file.metadata().map_err(|e| e.to_string())?.len();
            if len < pos {
                pos = 0;
            }
            if len > pos {
                file.seek(SeekFrom::Start(pos)).map_err(|e| e.to_string())?;
                let mut buf = String::new();
                file.read_to_string(&mut buf).map_err(|e| e.to_string())?;
                print!("{buf}");
                pos = len;
            }
        }
    }
    Ok(())
}

pub fn doctor(paths: &Paths, gc_fork_sessions: Option<String>) -> Result<(), String> {
    let mut problems = 0;
    let ok = |msg: &str| println!("  ok: {msg}");
    println!("forksan doctor");

    // Binaries.
    match std::env::current_exe() {
        Ok(exe) => {
            ok(&format!(
                "cli: {} (v{})",
                exe.display(),
                env!("CARGO_PKG_VERSION")
            ));
            let daemon_bin = exe.parent().map(|p| p.join("forksan-daemon"));
            match daemon_bin {
                Some(p) if p.is_file() => ok(&format!("daemon binary: {}", p.display())),
                _ => {
                    problems += 1;
                    println!("  PROBLEM: forksan-daemon not found next to the CLI");
                }
            }
        }
        Err(e) => {
            problems += 1;
            println!("  PROBLEM: cannot resolve current exe: {e}");
        }
    }

    // Daemon liveness + version.
    match Client::connect(paths, Duration::from_secs(2)) {
        Ok(mut client) => match client.request(RequestBody::Hello {
            version: env!("CARGO_PKG_VERSION").to_string(),
        }) {
            Ok(ResponseBody::HelloInfo { version }) => {
                ok(&format!(
                    "daemon answering at {} (v{version})",
                    paths.socket().display()
                ));
            }
            other => {
                problems += 1;
                println!("  PROBLEM: daemon handshake failed: {other:?}");
            }
        },
        Err(_) => {
            println!("  note: daemon not running (it auto-starts on the next hook event)");
        }
    }

    // State db.
    if paths.db().is_file() {
        ok(&format!("state db: {}", paths.db().display()));
    } else {
        println!("  note: no state db yet at {}", paths.db().display());
    }

    // The claude binary the forks will run with.
    let cfg = forksan_core::config::load_config_at(None, &paths.user_config()).0;
    let found = std::process::Command::new(&cfg.claude_bin)
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    if found {
        ok(&format!("claude binary: {}", cfg.claude_bin));
    } else {
        problems += 1;
        println!(
            "  PROBLEM: cannot run '{} --version' — forks will fail",
            cfg.claude_bin
        );
    }

    // Fork-session transcripts on disk (they accumulate; GC is opt-in).
    if let Some(ttl) = gc_fork_sessions {
        let secs = forksan_core::duration::parse_duration_str(&ttl)
            .ok_or_else(|| format!("invalid age '{ttl}' (try 30d)"))?;
        gc_transcripts(paths, secs)?;
    } else {
        println!("  note: fork runs leave headless session transcripts under ~/.claude/projects/;");
        println!("        prune old ones with: forksan doctor --gc-fork-sessions 30d");
    }

    if problems == 0 {
        println!("all good");
        Ok(())
    } else {
        Err(format!("{problems} problem(s) found"))
    }
}

/// Delete transcripts of *our own* fork sessions (ids recorded in fork_runs)
/// older than `age_secs`. Only exact `<session_id>.jsonl` files under
/// `~/.claude/projects/` are touched.
fn gc_transcripts(paths: &Paths, age_secs: u64) -> Result<(), String> {
    let store = forksan_core::store::Store::open(&paths.db()).map_err(|e| e.to_string())?;
    let cutoff = now() - age_secs as i64;
    let ids = store
        .fork_session_ids_before(cutoff)
        .map_err(|e| e.to_string())?;
    if ids.is_empty() {
        println!("  gc: no fork sessions older than the cutoff");
        return Ok(());
    }
    let home = std::env::var_os("HOME").ok_or("no HOME")?;
    let projects = std::path::PathBuf::from(home).join(".claude/projects");
    let mut removed = 0;
    if let Ok(dirs) = std::fs::read_dir(&projects) {
        for dir in dirs.flatten() {
            for id in &ids {
                let candidate = dir.path().join(format!("{id}.jsonl"));
                if candidate.is_file() && std::fs::remove_file(&candidate).is_ok() {
                    removed += 1;
                }
            }
        }
    }
    println!("  gc: removed {removed} fork-session transcript(s)");
    Ok(())
}
