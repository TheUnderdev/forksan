//! CLI hook-path tests: the `forksan hook <event>` entrypoint end-to-end,
//! including daemon auto-spawn and the spawn race.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const STUB: &str = r#"#!/bin/sh
INPUT=$(cat)
mkdir -p "$STUB_DIR"
printf '%s' "$INPUT" > "$STUB_DIR/prompt-$$.txt"
printf '{"result":"cli stub report","session_id":"stub-x","total_cost_usd":0.01,"is_error":false}\n'
"#;

struct Env {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    socket: PathBuf,
    stub_dir: PathBuf,
    project: PathBuf,
}

impl Env {
    fn new(idle: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let home = base.join("fsan");
        let stub_dir = base.join("stub");
        let project = base.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".forksan/forks")).unwrap();
        let stub = base.join("claude-stub");
        std::fs::write(&stub, STUB).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        std::fs::write(
            home.join("config.toml"),
            format!(
                "default_idle_deadline = \"{idle}\"\nquiet_period = \"1h\"\nfork_timeout = 10\nclaude_bin = \"{}\"\n",
                stub.display()
            ),
        )
        .unwrap();
        Self {
            socket: base.join("d.sock"),
            _tmp: tmp,
            home,
            stub_dir,
            project,
        }
    }

    fn hook(&self, event: &str, stdin_json: &serde_json::Value) -> (String, String) {
        let mut child = Command::new(env!("CARGO_BIN_EXE_forksan"))
            .args(["hook", event])
            .env("FORKSAN_HOME", &self.home)
            .env("FORKSAN_SOCKET", &self.socket)
            .env("STUB_DIR", &self.stub_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(stdin_json.to_string().as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hook exited nonzero: {out:?}");
        (
            String::from_utf8_lossy(&out.stdout).to_string(),
            String::from_utf8_lossy(&out.stderr).to_string(),
        )
    }

    fn hook_input(&self, session: &str) -> serde_json::Value {
        serde_json::json!({
            "session_id": session,
            "transcript_path": self.project.join("t.jsonl"),
            "cwd": self.project,
            "hook_event_name": "whatever",
        })
    }

    fn stub_ran(&self) -> bool {
        std::fs::read_dir(&self.stub_dir)
            .map(|d| d.count() > 0)
            .unwrap_or(false)
    }

    fn kill_daemon(&self) {
        // Retire via the frozen shutdown frame so tempdirs can be dropped.
        use std::os::unix::net::UnixStream;
        if let Ok(mut s) = UnixStream::connect(&self.socket) {
            let _ = s.write_all(b"{\"proto\":1,\"id\":1,\"type\":\"shutdown\",\"drain\":false}\n");
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

impl Drop for Env {
    fn drop(&mut self) {
        self.kill_daemon();
    }
}

#[test]
fn full_hook_cycle_delivers_report_via_additional_context() {
    let env = Env::new("1s");
    std::fs::write(
        env.project.join(".forksan/forks/journal.md"),
        "---\nrun_on: [idle]\n---\nJOURNAL BODY",
    )
    .unwrap();

    // session-start auto-spawns the daemon.
    let (out, _) = env.hook("session-start", &env.hook_input("s1"));
    assert!(out.trim().is_empty(), "no reports yet: {out}");

    // stop arms the idle clock; the fork fires at 1s.
    env.hook("stop", &env.hook_input("s1"));
    let start = Instant::now();
    while !env.stub_ran() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "idle fork never ran"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));

    // The next prompt gets the report as additionalContext.
    let (out, _) = env.hook("user-prompt-submit", &env.hook_input("s1"));
    let payload: serde_json::Value = serde_json::from_str(out.trim()).expect("json output");
    let ctx = payload["hookSpecificOutput"]["additionalContext"]
        .as_str()
        .unwrap();
    assert_eq!(
        payload["hookSpecificOutput"]["hookEventName"],
        "UserPromptSubmit"
    );
    assert!(ctx.contains("source: forksan"));
    assert!(ctx.contains("fork: journal"));
    assert!(ctx.contains("event: fork_response"));
    assert!(ctx.contains("cli stub report"));

    // Delivered once.
    let (out, _) = env.hook("user-prompt-submit", &env.hook_input("s1"));
    assert!(out.trim().is_empty(), "double delivery: {out}");
}

#[test]
fn disable_tags_env_filters_fork() {
    let env = Env::new("1s");
    std::fs::write(
        env.project.join(".forksan/forks/tagged.md"),
        "---\nrun_on: [idle]\ntags: [ci]\n---\nTAGGED BODY",
    )
    .unwrap();

    // The hook inherits FORKSAN_DISABLE_TAGS=ci from the Claude Code env and
    // carries it on the wire; the tagged fork must be filtered out.
    let run_hook = |event: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_forksan"))
            .args(["hook", event])
            .env("FORKSAN_HOME", &env.home)
            .env("FORKSAN_SOCKET", &env.socket)
            .env("STUB_DIR", &env.stub_dir)
            .env("FORKSAN_DISABLE_TAGS", "ci")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(env.hook_input("s1").to_string().as_bytes())
            .unwrap();
        assert!(child.wait().unwrap().success());
    };

    run_hook("session-start");
    run_hook("stop");
    // Well past the 1s idle deadline: the disabled fork must not have run.
    std::thread::sleep(Duration::from_millis(1800));
    assert!(
        !env.stub_ran(),
        "disabled fork ran despite FORKSAN_DISABLE_TAGS"
    );
}

#[test]
fn concurrent_hooks_race_to_one_daemon() {
    let env = Env::new("1h");
    // Several session-starts at once: exactly one daemon must win.
    let mut children = Vec::new();
    for i in 0..5 {
        let mut child = Command::new(env!("CARGO_BIN_EXE_forksan"))
            .args(["hook", "session-start"])
            .env("FORKSAN_HOME", &env.home)
            .env("FORKSAN_SOCKET", &env.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(env.hook_input(&format!("s{i}")).to_string().as_bytes())
            .unwrap();
        children.push(child);
    }
    for mut c in children {
        assert!(c.wait().unwrap().success());
    }
    // All five sessions landed in one daemon.
    let out = Command::new(env!("CARGO_BIN_EXE_forksan"))
        .arg("status")
        .env("FORKSAN_HOME", &env.home)
        .env("FORKSAN_SOCKET", &env.socket)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(stdout.contains("sessions: 5"), "status was: {stdout}");
}

#[test]
fn hook_never_fails_on_garbage_stdin() {
    let env = Env::new("1h");
    for event in [
        "session-start",
        "user-prompt-submit",
        "stop",
        "pre-compact",
        "session-end",
    ] {
        let mut child = Command::new(env!("CARGO_BIN_EXE_forksan"))
            .args(["hook", event])
            .env("FORKSAN_HOME", &env.home)
            .env("FORKSAN_SOCKET", &env.socket)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(b"not json").unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success(), "hook {event} broke on garbage stdin");
    }
    let _ = Path::new("x");
}
