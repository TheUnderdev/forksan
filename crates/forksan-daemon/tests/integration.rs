//! End-to-end daemon tests: spawn the real daemon binary against a stub
//! `claude` script and drive it over the unix socket with protocol frames.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use forksan_core::protocol::{
    encode, Event, EventKind, ReportItem, ReportKind, Request, RequestBody, Response, ResponseBody,
    WaitMode,
};
use forksan_core::PROTO_VERSION;

/// The stub claude: records its stdin, argv, FORKSAN_* env, and cwd under
/// $STUB_DIR, honors FAIL/SLOW markers embedded in the prompt, then prints a
/// result JSON. `STUB_FAIL_RESUME_<N>` fails (exit 1, "No conversation found"
/// on stderr) for the first N invocations and then succeeds — a stand-in for
/// the transcript-flush race.
const STUB: &str = r#"#!/bin/sh
INPUT=$(cat)
N=$(date +%s%N)-$$
mkdir -p "$STUB_DIR"
printf '%s' "$INPUT" > "$STUB_DIR/prompt-$N.txt"
printf '%s\n' "$@" > "$STUB_DIR/args-$N.txt"
printf 'session_id=%s\nfork_name=%s\ntrigger=%s\nproject_root=%s\npwd=%s\n' "$FORKSAN_SESSION_ID" "$FORKSAN_FORK_NAME" "$FORKSAN_TRIGGER" "$FORKSAN_PROJECT_ROOT" "$(pwd)" > "$STUB_DIR/env-$N.txt"
: > "$STUB_DIR/start-$N"
case "$INPUT" in
  *STUB_FAIL_RESUME_*)
    NFAIL=$(printf '%s' "$INPUT" | sed -n 's/.*STUB_FAIL_RESUME_\([0-9][0-9]*\).*/\1/p')
    COUNT=$(cat "$STUB_DIR/resume-count" 2>/dev/null || echo 0)
    if [ "${COUNT:-0}" -lt "${NFAIL:-0}" ]; then
      echo $((COUNT + 1)) > "$STUB_DIR/resume-count"
      echo "No conversation found with session ID: x" >&2
      exit 1
    fi
    printf '{"result":"stub report %s","session_id":"stub-%s","total_cost_usd":0.01,"is_error":false}\n' "$N" "$N"
    exit 0
    ;;
esac
case "$INPUT" in
  *STUB_SLOW*) sleep 2 ;;
  *) sleep 0.1 ;;
esac
: > "$STUB_DIR/done-$N"
case "$INPUT" in
  *STUB_FAIL*)
    printf '{"result":"boom","session_id":"stub-%s","is_error":true}\n' "$N" ;;
  *)
    printf '{"result":"stub report %s","session_id":"stub-%s","total_cost_usd":0.01,"is_error":false}\n' "$N" "$N" ;;
esac
"#;

/// One captured fork subprocess invocation.
struct Invocation {
    prompt: String,
    args: Vec<String>,
    env: String,
}

struct Harness {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    socket: PathBuf,
    stub_dir: PathBuf,
    project: PathBuf,
    daemon: Option<Child>,
}

impl Harness {
    fn new(idle_deadline: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let home = base.join("fsan");
        let stub_dir = base.join("stub");
        let project = base.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&stub_dir).unwrap();
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
                "default_idle_deadline = \"{idle}\"\nquiet_period = \"1h\"\nfork_timeout = 10\nclaude_bin = \"{bin}\"\n",
                idle = idle_deadline,
                bin = stub.display()
            ),
        )
        .unwrap();

        Self {
            socket: base.join("d.sock"),
            _tmp: tmp,
            home,
            stub_dir,
            project,
            daemon: None,
        }
    }

    fn append_config(&self, extra: &str) {
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(self.home.join("config.toml"))
            .unwrap();
        writeln!(f, "{extra}").unwrap();
    }

    fn write_fork(&self, rel: &str, content: &str) {
        let path = self.project.join(".forksan/forks").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn start_daemon(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_forksan-daemon"))
            .env("FORKSAN_HOME", &self.home)
            .env("FORKSAN_SOCKET", &self.socket)
            .env("STUB_DIR", &self.stub_dir)
            // Short-circuit the resume-retry backoff so tests don't wait 14s.
            .env("FORKSAN_RETRY_BASE_MS", "20")
            .env("RUST_LOG", "debug")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.daemon = Some(child);
        // Wait for the socket to accept.
        let start = Instant::now();
        loop {
            if UnixStream::connect(&self.socket).is_ok() {
                return;
            }
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "daemon never came up"
            );
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn kill_daemon(&mut self) {
        if let Some(mut child) = self.daemon.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn request(&self, body: RequestBody) -> ResponseBody {
        let stream = UnixStream::connect(&self.socket).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let req = Request {
            proto: PROTO_VERSION,
            id: 1,
            body,
        };
        writer.write_all(encode(&req).unwrap().as_bytes()).unwrap();
        let mut line = String::new();
        BufReader::new(stream).read_line(&mut line).unwrap();
        serde_json::from_str::<Response>(line.trim()).unwrap().body
    }

    fn event(&self, kind: EventKind, session: &str) -> Event {
        Event {
            event: kind,
            session_id: session.to_string(),
            transcript_path: None,
            cwd: self.project.clone(),
            project_root: self.project.clone(),
            source: None,
            trigger: None,
            reason: None,
            model: None,
            enable_tags: None,
            disable_tags: None,
            wait: WaitMode::None,
        }
    }

    fn send_event(&self, ev: Event) -> ResponseBody {
        self.request(RequestBody::Event(ev))
    }

    fn poll(&self, session: &str) -> Vec<ReportItem> {
        match self.request(RequestBody::PollReports {
            session_id: session.to_string(),
            project_root: self.project.clone(),
            budget_chars: 1_000_000,
        }) {
            ResponseBody::Reports { items } => items,
            other => panic!("unexpected response: {other:?}"),
        }
    }

    /// Poll until at least one `response` report appears (or time out).
    fn wait_for_responses(&self, session: &str, timeout: Duration) -> Vec<ReportItem> {
        let start = Instant::now();
        let mut collected = Vec::new();
        loop {
            collected.extend(self.poll(session));
            if collected.iter().any(|r| r.kind == ReportKind::Response) {
                return collected;
            }
            assert!(
                start.elapsed() < timeout,
                "no response report within {timeout:?}; got {collected:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Every fork invocation the stub captured, correlating the stdin prompt,
    /// the argv, and the FORKSAN_* env by their shared filename suffix.
    fn invocations(&self) -> Vec<Invocation> {
        let mut out = Vec::new();
        if let Ok(read) = std::fs::read_dir(&self.stub_dir) {
            for entry in read.flatten() {
                let fname = entry.file_name().to_string_lossy().into_owned();
                let Some(n) = fname
                    .strip_prefix("prompt-")
                    .and_then(|s| s.strip_suffix(".txt"))
                else {
                    continue;
                };
                let prompt = std::fs::read_to_string(entry.path()).unwrap_or_default();
                let args = std::fs::read_to_string(self.stub_dir.join(format!("args-{n}.txt")))
                    .unwrap_or_default()
                    .lines()
                    .map(String::from)
                    .collect();
                let env = std::fs::read_to_string(self.stub_dir.join(format!("env-{n}.txt")))
                    .unwrap_or_default();
                out.push(Invocation { prompt, args, env });
            }
        }
        out
    }

    fn stub_prompts(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Ok(read) = std::fs::read_dir(&self.stub_dir) {
            for entry in read.flatten() {
                let is_prompt = entry
                    .file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with("prompt-"));
                if is_prompt {
                    out.push(std::fs::read_to_string(entry.path()).unwrap_or_default());
                }
            }
        }
        out
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.kill_daemon();
    }
}

fn assert_ack(body: ResponseBody) {
    assert!(
        matches!(body, ResponseBody::Ack),
        "expected ack, got {body:?}"
    );
}

#[test]
fn idle_fork_fires_and_report_delivers_next_turn() {
    let mut h = Harness::new("1s");
    h.write_fork(
        "journal.md",
        "---\ndescription: test journal\nrun_on: [idle]\n---\nwrite the journal now",
    );
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    assert_ack(h.send_event(h.event(EventKind::Stop, "s1")));

    // Idle deadline is 1s; wait for the run to happen and fully finish
    // WITHOUT polling, so the first poll sees both boundary events pending.
    let start = Instant::now();
    while h.stub_prompts().is_empty() {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "idle fork never ran"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_millis(700));

    let reports = h.poll("s1");
    let response = reports
        .iter()
        .find(|r| r.kind == ReportKind::Response)
        .unwrap();
    assert_eq!(response.fork, "journal");
    assert_eq!(response.trigger, "idle");
    assert!(response.body.contains("stub report"));
    // Started marker collapsed away since the response was already pending.
    assert!(reports.iter().all(|r| r.kind != ReportKind::Started));

    // The fork saw the prompt with framing + body.
    let prompts = h.stub_prompts();
    assert_eq!(prompts.len(), 1);
    assert!(prompts[0].contains("source: forksan"));
    assert!(prompts[0].contains("write the journal now"));

    // Reports deliver exactly once.
    assert!(h.poll("s1").is_empty());
}

#[test]
fn disable_tag_filters_fork_but_untagged_runs() {
    let mut h = Harness::new("1s");
    h.write_fork(
        "tagged.md",
        "---\nrun_on: [idle]\ntags: [ci]\n---\nTAGGED WORK",
    );
    h.write_fork("plain.md", "---\nrun_on: [idle]\n---\nPLAIN WORK");
    h.start_daemon();

    let mut start_ev = h.event(EventKind::SessionStart, "s1");
    start_ev.disable_tags = Some(vec!["ci".into()]);
    assert_ack(h.send_event(start_ev));
    let mut stop_ev = h.event(EventKind::Stop, "s1");
    stop_ev.disable_tags = Some(vec!["ci".into()]);
    assert_ack(h.send_event(stop_ev));

    // The untagged fork runs at the 1s idle deadline.
    let start = Instant::now();
    loop {
        if h.stub_prompts().iter().any(|p| p.contains("PLAIN WORK")) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "untagged fork never ran"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    // Both forks share the idle moment, so the tagged one was already
    // evaluated (and skipped) by now; a short grace confirms it stays out.
    std::thread::sleep(Duration::from_millis(500));
    assert!(
        h.stub_prompts().iter().all(|p| !p.contains("TAGGED WORK")),
        "disabled tagged fork ran anyway"
    );
}

#[test]
fn enable_list_excludes_untagged_fork() {
    let mut h = Harness::new("1s");
    h.write_fork(
        "tagged.md",
        "---\nrun_on: [idle]\ntags: [ci]\n---\nTAGGED WORK",
    );
    h.write_fork("plain.md", "---\nrun_on: [idle]\n---\nPLAIN WORK");
    h.start_daemon();

    let mut start_ev = h.event(EventKind::SessionStart, "s1");
    start_ev.enable_tags = Some(vec!["ci".into()]);
    assert_ack(h.send_event(start_ev));
    let mut stop_ev = h.event(EventKind::Stop, "s1");
    stop_ev.enable_tags = Some(vec!["ci".into()]);
    assert_ack(h.send_event(stop_ev));

    // The whitelisted tagged fork runs.
    let start = Instant::now();
    loop {
        if h.stub_prompts().iter().any(|p| p.contains("TAGGED WORK")) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "whitelisted fork never ran"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(500));
    // The untagged fork is excluded by the whitelist.
    assert!(
        h.stub_prompts().iter().all(|p| !p.contains("PLAIN WORK")),
        "untagged fork ran despite an enable whitelist"
    );
}

#[test]
fn run_by_tag_fires_all_matching_forks() {
    // Long idle so nothing auto-fires; only the manual tag run does.
    let mut h = Harness::new("1h");
    h.write_fork("a.md", "---\nrun_on: [idle]\ntags: [ci]\n---\nA WORK");
    h.write_fork("b.md", "---\nrun_on: [idle]\ntags: [ci]\n---\nB WORK");
    h.write_fork("c.md", "---\nrun_on: [idle]\ntags: [docs]\n---\nC WORK");
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    match h.request(RequestBody::RunFork {
        name: None,
        project_root: h.project.clone(),
        cwd: h.project.clone(),
        session_id: None,
        tag: Some("ci".into()),
    }) {
        ResponseBody::RunStartedMany { mut forks, .. } => {
            forks.sort();
            assert_eq!(forks, vec!["a".to_string(), "b".to_string()]);
        }
        other => panic!("unexpected: {other:?}"),
    }

    // Both ci-tagged forks run; the docs-tagged fork is untouched.
    let start = Instant::now();
    loop {
        let prompts = h.stub_prompts();
        if prompts.iter().any(|p| p.contains("A WORK"))
            && prompts.iter().any(|p| p.contains("B WORK"))
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "tagged forks never ran: {prompts:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(300));
    assert!(
        h.stub_prompts().iter().all(|p| !p.contains("C WORK")),
        "non-matching fork ran on a tag run"
    );

    // An unknown tag is an error, not a silent no-op.
    match h.request(RequestBody::RunFork {
        name: None,
        project_root: h.project.clone(),
        cwd: h.project.clone(),
        session_id: None,
        tag: Some("nope".into()),
    }) {
        ResponseBody::Error { message, .. } => {
            assert!(message.contains("no forks with tag 'nope'"), "{message}");
        }
        other => panic!("unexpected: {other:?}"),
    }
}

#[test]
fn tag_throttle_suppresses_group_but_other_tag_runs() {
    let mut h = Harness::new("1s");
    h.append_config("[tag_throttles]\nci = \"1h\"");
    h.write_fork(
        "a.md",
        "---\nrun_on: [session_start]\ntags: [ci]\n---\nA WORK",
    );
    h.write_fork("b.md", "---\nrun_on: [idle]\ntags: [ci]\n---\nB WORK");
    h.write_fork("c.md", "---\nrun_on: [idle]\ntags: [docs]\n---\nC WORK");
    h.start_daemon();

    // forkA fires on session_start and stamps a `ci` run.
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let start = Instant::now();
    while !h.stub_prompts().iter().any(|p| p.contains("A WORK")) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "session_start fork never ran"
        );
        std::thread::sleep(Duration::from_millis(50));
    }

    // Arm the 1s idle clock; forkC (docs) runs, forkB (ci) is tag-throttled.
    assert_ack(h.send_event(h.event(EventKind::Stop, "s1")));
    let start = Instant::now();
    while !h.stub_prompts().iter().any(|p| p.contains("C WORK")) {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "differently-tagged fork never ran"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
    std::thread::sleep(Duration::from_millis(400));
    assert!(
        h.stub_prompts().iter().all(|p| !p.contains("B WORK")),
        "same-tag fork ran despite the tag throttle"
    );
}

#[test]
fn fork_permission_flags_and_session_env_passed() {
    let mut h = Harness::new("1h");
    h.write_fork(
        "a.md",
        "---\nrun_on: [session_start]\nallowed_tools: [Write, 'Bash(git add:*)']\npermission_mode: acceptEdits\n---\nPERM WORK",
    );
    h.write_fork("b.md", "---\nrun_on: [session_start]\n---\nPLAIN WORK");
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // Wait until both forks have been invoked.
    let start = Instant::now();
    let invs = loop {
        let invs = h.invocations();
        if invs.iter().any(|i| i.prompt.contains("PERM WORK"))
            && invs.iter().any(|i| i.prompt.contains("PLAIN WORK"))
        {
            break invs;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "forks never ran: {:?}",
            invs.iter().map(|i| &i.prompt).collect::<Vec<_>>()
        );
        std::thread::sleep(Duration::from_millis(100));
    };

    // The permissioned fork carries both flags and every allowed_tools entry.
    let perm = invs
        .iter()
        .find(|i| i.prompt.contains("PERM WORK"))
        .unwrap();
    assert!(perm.args.iter().any(|a| a == "--allowedTools"));
    assert!(perm.args.iter().any(|a| a == "Write"));
    assert!(perm.args.iter().any(|a| a == "Bash(git add:*)"));
    assert!(perm.args.iter().any(|a| a == "--permission-mode"));
    assert!(perm.args.iter().any(|a| a == "acceptEdits"));

    // The plain fork gets neither permission flag.
    let plain = invs
        .iter()
        .find(|i| i.prompt.contains("PLAIN WORK"))
        .unwrap();
    assert!(!plain.args.iter().any(|a| a == "--allowedTools"));
    assert!(!plain.args.iter().any(|a| a == "--permission-mode"));

    // Both invocations receive the parent session identity in the env.
    assert!(perm.env.contains("session_id=s1"));
    assert!(perm.env.contains("fork_name=a"));
    assert!(perm.env.contains("trigger=session_start"));
    assert!(perm
        .env
        .contains(&format!("project_root={}", h.project.display())));
    assert!(plain.env.contains("fork_name=b"));
    assert!(plain.env.contains("session_id=s1"));
}

#[test]
fn config_permission_mode_applies_and_fork_overrides() {
    let mut h = Harness::new("1h");
    h.append_config("permission_mode = \"bypassPermissions\"");
    h.write_fork(
        "over.md",
        "---\nrun_on: [session_start]\npermission_mode: acceptEdits\n---\nOVERRIDE WORK",
    );
    h.write_fork(
        "inherit.md",
        "---\nrun_on: [session_start]\n---\nINHERIT WORK",
    );
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    let start = Instant::now();
    let invs = loop {
        let invs = h.invocations();
        if invs.iter().any(|i| i.prompt.contains("OVERRIDE WORK"))
            && invs.iter().any(|i| i.prompt.contains("INHERIT WORK"))
        {
            break invs;
        }
        assert!(start.elapsed() < Duration::from_secs(10), "forks never ran");
        std::thread::sleep(Duration::from_millis(100));
    };

    // The fork's own mode wins over the config default.
    let over = invs
        .iter()
        .find(|i| i.prompt.contains("OVERRIDE WORK"))
        .unwrap();
    let mode_after = |i: &Invocation| -> Option<String> {
        i.args
            .iter()
            .position(|a| a == "--permission-mode")
            .and_then(|p| i.args.get(p + 1).cloned())
    };
    assert_eq!(mode_after(over).as_deref(), Some("acceptEdits"));

    // The fork without its own mode inherits the config default.
    let inherit = invs
        .iter()
        .find(|i| i.prompt.contains("INHERIT WORK"))
        .unwrap();
    assert_eq!(mode_after(inherit).as_deref(), Some("bypassPermissions"));
}

#[test]
fn resume_race_retries_until_transcript_ready() {
    let mut h = Harness::new("1h");
    h.write_fork(
        "retry.md",
        "---\nrun_on: [session_start]\n---\nSTUB_FAIL_RESUME_2 do the work",
    );
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // The first two resume attempts fail ("No conversation found"); the fork
    // retries and ultimately delivers a success response.
    let reports = h.wait_for_responses("s1", Duration::from_secs(10));
    let resp = reports
        .iter()
        .find(|r| r.fork == "retry" && r.kind == ReportKind::Response)
        .expect("a response for the retry fork");
    assert!(
        resp.body.contains("stub report"),
        "fork did not ultimately succeed: {resp:?}"
    );

    // Exactly N+1 = 3 invocations: two failed resumes and one success.
    std::thread::sleep(Duration::from_millis(200));
    let attempts = h
        .stub_prompts()
        .iter()
        .filter(|p| p.contains("STUB_FAIL_RESUME_2"))
        .count();
    assert_eq!(attempts, 3, "expected 2 retries then success");
}

#[test]
fn fork_runs_in_pinned_launch_cwd_not_drifted() {
    let mut h = Harness::new("1h");
    h.write_fork("end.md", "---\nrun_on: [session_end]\n---\nDRIFT WORK");
    h.start_daemon();

    // Session launches in the project dir (cwd A).
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // It later `cd`'d into a subdirectory; a Stop event reports the drifted
    // cwd B, which must NOT overwrite the pinned launch cwd.
    let drifted = h.project.join("drifted");
    std::fs::create_dir_all(&drifted).unwrap();
    let mut stop = h.event(EventKind::Stop, "s1");
    stop.cwd = drifted.clone();
    assert_ack(h.send_event(stop));

    // SessionEnd fires the session_end fork, which must run in cwd A.
    assert_ack(h.send_event(h.event(EventKind::SessionEnd, "s1")));

    let start = Instant::now();
    let inv = loop {
        if let Some(inv) = h
            .invocations()
            .into_iter()
            .find(|i| i.prompt.contains("DRIFT WORK"))
        {
            break inv;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "session_end fork never ran"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    let pwd = inv
        .env
        .lines()
        .find_map(|l| l.strip_prefix("pwd="))
        .expect("stub recorded its cwd");
    assert_eq!(
        std::path::Path::new(pwd)
            .file_name()
            .and_then(|f| f.to_str()),
        Some("proj"),
        "fork ran in the drifted cwd instead of the pinned launch dir: {pwd}"
    );
}

#[test]
fn prompt_submit_resets_idle_clock() {
    let mut h = Harness::new("2s");
    h.write_fork("j.md", "---\nrun_on: [idle]\n---\nbody");
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    assert_ack(h.send_event(h.event(EventKind::Stop, "s1")));
    std::thread::sleep(Duration::from_millis(1200));
    // Waking activity before the 2s deadline: no fork may fire.
    let items = match h.send_event(h.event(EventKind::PromptSubmit, "s1")) {
        ResponseBody::Reports { items } => items,
        other => panic!("unexpected: {other:?}"),
    };
    assert!(items.is_empty());
    std::thread::sleep(Duration::from_millis(1500));
    // Turn still in flight (no Stop): timer must not be armed.
    assert!(h.poll("s1").is_empty());
    assert!(h.stub_prompts().is_empty());
}

#[test]
fn after_chain_pipes_predecessor_report() {
    let mut h = Harness::new("1s");
    h.write_fork("alpha.md", "---\nrun_on: [idle]\n---\nALPHA WORK");
    h.write_fork(
        "beta.md",
        "---\nrun_on: [idle]\nafter: alpha\n---\nBETA WORK",
    );
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    assert_ack(h.send_event(h.event(EventKind::Stop, "s1")));

    let start = Instant::now();
    loop {
        let prompts = h.stub_prompts();
        let beta = prompts.iter().find(|p| p.contains("BETA WORK"));
        if let Some(beta) = beta {
            assert!(beta.contains("<predecessor fork=\"alpha\">"));
            assert!(beta.contains("stub report"));
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "beta never ran: {prompts:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn pre_compact_acks_after_spawn_not_completion() {
    let mut h = Harness::new("1h");
    h.write_fork(
        "snap.md",
        "---\nrun_on: [compact]\n---\nSTUB_SLOW snapshot work",
    );
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let mut ev = h.event(EventKind::PreCompact, "s1");
    ev.trigger = Some("auto".into());
    ev.wait = WaitMode::ForksSpawned;
    let start = Instant::now();
    assert_ack(h.send_event(ev));
    let ack_latency = start.elapsed();
    // The stub sleeps 2s; the ack must arrive as soon as it spawned.
    assert!(
        ack_latency < Duration::from_millis(1800),
        "ack waited for completion: {ack_latency:?}"
    );
    // And the run itself completes afterward.
    let reports = h.wait_for_responses("s1", Duration::from_secs(10));
    assert!(reports
        .iter()
        .any(|r| r.fork == "snap" && r.kind == ReportKind::Response));
}

#[test]
fn context_threshold_latches_once_per_session() {
    let mut h = Harness::new("1h");
    h.write_fork(
        "ctx.md",
        "---\nrun_on:\n  - context_tokens: 1000\n---\ncontext is filling up",
    );
    h.start_daemon();

    let transcript = h.project.join("transcript.jsonl");
    std::fs::write(
        &transcript,
        r#"{"type":"assistant","message":{"model":"m","usage":{"input_tokens":2000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}
"#,
    )
    .unwrap();

    let mut ev = h.event(EventKind::SessionStart, "s1");
    ev.transcript_path = Some(transcript.clone());
    assert_ack(h.send_event(ev));

    let mut stop = h.event(EventKind::Stop, "s1");
    stop.transcript_path = Some(transcript.clone());
    assert_ack(h.send_event(stop.clone()));

    let reports = h.wait_for_responses("s1", Duration::from_secs(10));
    assert!(reports.iter().any(|r| r.fork == "ctx"));

    // Append more usage and stop again: latched, must not re-fire.
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(&transcript)
        .unwrap();
    writeln!(
        f,
        r#"{{"type":"assistant","message":{{"model":"m","usage":{{"input_tokens":5000,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}}}}}}"#
    )
    .unwrap();
    assert_ack(h.send_event(stop));
    std::thread::sleep(Duration::from_millis(700));
    assert!(h.poll("s1").is_empty());
    assert_eq!(h.stub_prompts().len(), 1);
}

#[test]
fn manual_stop_fires_only_on_active_close() {
    let mut h = Harness::new("1s");
    h.write_fork("bye.md", "---\nrun_on: [manual_stop]\n---\nsay goodbye");
    h.start_daemon();

    // Session closed 100ms after activity → manual stop.
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    std::thread::sleep(Duration::from_millis(100));
    assert_ack(h.send_event(h.event(EventKind::SessionEnd, "s1")));
    let start = Instant::now();
    loop {
        if h.stub_prompts().iter().any(|p| p.contains("say goodbye")) {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "manual_stop fork never ran"
        );
        std::thread::sleep(Duration::from_millis(100));
    }

    // Session closed after idling past the deadline → not manual.
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s2")));
    std::thread::sleep(Duration::from_millis(1600));
    let before = h.stub_prompts().len();
    assert_ack(h.send_event(h.event(EventKind::SessionEnd, "s2")));
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        h.stub_prompts().len(),
        before,
        "manual_stop fired on a timed-out close"
    );
}

#[test]
fn dead_session_reports_flow_to_next_session_in_project() {
    let mut h = Harness::new("1h");
    h.write_fork("bye.md", "---\nrun_on: [session_end]\n---\nwrap up");
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    assert_ack(h.send_event(h.event(EventKind::SessionEnd, "s1")));

    // The session_end fork's report lands with s1 closed; a fresh session in
    // the same project picks it up.
    let start = Instant::now();
    loop {
        assert_ack(h.send_event(h.event(EventKind::SessionStart, "s2")));
        let items = h.poll("s2");
        if items
            .iter()
            .any(|r| r.fork == "bye" && r.kind == ReportKind::Response)
        {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "report never crossed sessions"
        );
        std::thread::sleep(Duration::from_millis(150));
    }
}

#[test]
fn boot_sweep_runs_owed_idle_forks() {
    let mut h = Harness::new("1s");
    h.write_fork("owed.md", "---\nrun_on: [idle]\n---\nowed work");
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    assert_ack(h.send_event(h.event(EventKind::Stop, "s1")));
    // Kill before the 1s idle deadline can fire.
    std::thread::sleep(Duration::from_millis(200));
    h.kill_daemon();
    assert!(h.stub_prompts().is_empty(), "fork ran before the kill");

    // Past the deadline while dead; the new daemon owes the idle fork.
    std::thread::sleep(Duration::from_millis(1200));
    h.start_daemon();
    let reports = h.wait_for_responses("s1", Duration::from_secs(10));
    assert!(reports.iter().any(|r| r.fork == "owed"));

    // A second restart must not double-run it (forks_ran_at advanced).
    let count = h.stub_prompts().len();
    h.kill_daemon();
    h.start_daemon();
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        h.stub_prompts().len(),
        count,
        "owed fork double-ran after restart"
    );
}

#[test]
fn failed_fork_reports_failure_and_dependents_still_run() {
    let mut h = Harness::new("1s");
    h.write_fork("bad.md", "---\nrun_on: [idle]\n---\nSTUB_FAIL do bad work");
    h.write_fork("dep.md", "---\nrun_on: [idle]\nafter: bad\n---\nDEP WORK");
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    assert_ack(h.send_event(h.event(EventKind::Stop, "s1")));

    let reports = h.wait_for_responses("s1", Duration::from_secs(10));
    let start = Instant::now();
    let mut all = reports;
    loop {
        let bad = all
            .iter()
            .any(|r| r.fork == "bad" && r.body.contains("failed"));
        let dep_ran = h.stub_prompts().iter().any(|p| p.contains("DEP WORK"));
        if bad && dep_ran {
            // Dependent ran without a predecessor block (bad failed).
            let dep_prompt = h
                .stub_prompts()
                .into_iter()
                .find(|p| p.contains("DEP WORK"))
                .unwrap();
            assert!(!dep_prompt.contains("<predecessor"));
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "missing failure/dep: {all:?}"
        );
        std::thread::sleep(Duration::from_millis(150));
        all.extend(h.poll("s1"));
    }
}

#[test]
fn status_and_list_forks_and_shutdown() {
    let mut h = Harness::new("1h");
    h.write_fork(
        "info/FORK.md",
        "---\ndescription: nested fork\nrun_on: [boot]\nthrottle: 5m\n---\nbody",
    );
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    match h.request(RequestBody::Status) {
        ResponseBody::StatusInfo(info) => {
            assert_eq!(info.daemon_proto, PROTO_VERSION);
            assert_eq!(info.sessions.len(), 1);
        }
        other => panic!("unexpected: {other:?}"),
    }

    match h.request(RequestBody::ListForks {
        project_root: h.project.clone(),
        cwd: h.project.clone(),
    }) {
        ResponseBody::ForkList { items } => {
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].name, "info");
            assert_eq!(items[0].description.as_deref(), Some("nested fork"));
            assert_eq!(items[0].triggers, vec!["boot"]);
            assert_eq!(items[0].throttle_secs, Some(300));
        }
        other => panic!("unexpected: {other:?}"),
    }

    // Old-CLI-compatible shutdown retires the daemon.
    assert_ack(h.request(RequestBody::Shutdown { drain: true }));
    let start = Instant::now();
    loop {
        if UnixStream::connect(&h.socket).is_err() {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "daemon didn't exit"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    if let Some(mut child) = h.daemon.take() {
        let _ = child.wait();
    }
}

#[test]
fn in_flight_fork_skips_new_fires_until_next_moment() {
    let mut h = Harness::new("1h");
    // STUB_SLOW sleeps 2s; session_start fires the fork on each new-session event.
    h.write_fork(
        "serial.md",
        "---\nrun_on: [session_start]\n---\nSTUB_SLOW serial work",
    );
    h.start_daemon();

    // Three fires in quick succession: run 1 starts; fires 2 and 3 are
    // cancelled outright (a run is in flight).
    for sid in ["s1", "s2", "s3"] {
        assert_ack(h.send_event(h.event(EventKind::SessionStart, sid)));
    }
    let start = Instant::now();
    loop {
        let names: Vec<String> = std::fs::read_dir(&h.stub_dir)
            .map(|d| {
                d.flatten()
                    .filter_map(|e| e.file_name().to_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let done = names.iter().filter(|n| n.starts_with("done-")).count();
        if done == 1 {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "first run never finished: {names:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // Give any (wrong) parked run a moment to appear.
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(
        h.stub_prompts().len(),
        1,
        "in-flight fires were not cancelled"
    );

    // The next occurrence of the moment fires normally again.
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s4")));
    let start = Instant::now();
    while h.stub_prompts().len() < 2 {
        assert!(
            start.elapsed() < Duration::from_secs(15),
            "fork did not fire at its next moment"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    // No continuity block: the next run inherits the parent context as-is.
    assert!(h
        .stub_prompts()
        .iter()
        .all(|p| !p.contains("<previous_run")));
}

#[test]
fn overlap_true_allows_concurrent_runs() {
    let mut h = Harness::new("1h");
    h.write_fork(
        "para.md",
        "---\nrun_on: [session_start]\noverlap: true\n---\nSTUB_SLOW parallel work",
    );
    h.start_daemon();

    for sid in ["s1", "s2", "s3"] {
        assert_ack(h.send_event(h.event(EventKind::SessionStart, sid)));
    }
    let start = Instant::now();
    loop {
        let names: Vec<String> = std::fs::read_dir(&h.stub_dir)
            .map(|d| {
                d.flatten()
                    .filter_map(|e| e.file_name().to_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let started = names.iter().filter(|n| n.starts_with("start-")).count();
        let done = names.iter().filter(|n| n.starts_with("done-")).count();
        if started >= 2 && done == 0 {
            return; // two subprocesses alive at once — overlap honored
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "runs never overlapped despite overlap: true (started={started} done={done})"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[test]
fn multi_dependency_gets_all_predecessor_reports() {
    let mut h = Harness::new("1s");
    h.write_fork("alpha.md", "---\nrun_on: [idle]\n---\nALPHA WORK");
    h.write_fork("beta.md", "---\nrun_on: [idle]\n---\nBETA WORK");
    h.write_fork(
        "gamma.md",
        "---\nrun_on: [idle]\nafter: [alpha, beta]\n---\nGAMMA WORK",
    );
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    assert_ack(h.send_event(h.event(EventKind::Stop, "s1")));

    let start = Instant::now();
    loop {
        let prompts = h.stub_prompts();
        if let Some(gamma) = prompts.iter().find(|p| p.contains("GAMMA WORK")) {
            assert!(gamma.contains("<predecessor fork=\"alpha\">"));
            assert!(gamma.contains("<predecessor fork=\"beta\">"));
            // And gamma ran after both finished.
            assert_eq!(prompts.len(), 3);
            return;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "gamma never ran: {prompts:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}
