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

/// The stub claude: records its stdin under $STUB_DIR, honors FAIL/SLOW
/// markers embedded in the prompt, then prints a result JSON.
const STUB: &str = r#"#!/bin/sh
INPUT=$(cat)
N=$(date +%s%N)-$$
mkdir -p "$STUB_DIR"
printf '%s' "$INPUT" > "$STUB_DIR/prompt-$N.txt"
: > "$STUB_DIR/start-$N"
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
fn same_fork_runs_never_overlap_and_extra_fires_coalesce() {
    let mut h = Harness::new("1h");
    // STUB_SLOW sleeps 2s; session_start fires the fork on each new-session event.
    h.write_fork(
        "serial.md",
        "---\nrun_on: [session_start]\n---\nSTUB_SLOW serial work",
    );
    h.start_daemon();

    // Three fires in quick succession (distinct sessions, same project):
    // run 1 starts; fire 2 parks; fire 3 coalesces away.
    for sid in ["s1", "s2", "s3"] {
        assert_ack(h.send_event(h.event(EventKind::SessionStart, sid)));
    }

    // While runs are in flight, at most one subprocess may exist at a time.
    let start = Instant::now();
    let mut max_in_flight = 0usize;
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
        max_in_flight = max_in_flight.max(started - done);
        if started == 2 && done == 2 {
            break;
        }
        assert!(
            start.elapsed() < Duration::from_secs(20),
            "expected exactly 2 serialized runs; saw started={started} done={done}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(max_in_flight, 1, "runs of the same fork overlapped");
    // Give a moment for any (wrong) third run to appear.
    std::thread::sleep(Duration::from_millis(700));
    let prompts = h.stub_prompts();
    assert_eq!(prompts.len(), 2, "coalesced fire still ran");

    // Gated continuity: the waiting run's prompt carries the first run's
    // report (it was never delivered to the parent in between).
    let with_previous: Vec<&String> = prompts
        .iter()
        .filter(|p| p.contains("<previous_run fork=\"serial\">"))
        .collect();
    assert_eq!(
        with_previous.len(),
        1,
        "exactly the second run sees the first's report"
    );
    assert!(with_previous[0].contains("stub report"));
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
