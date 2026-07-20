//! End-to-end daemon tests: spawn the real daemon binary and drive it over the
//! unix socket with protocol frames. v0.5 forks are never subprocesses — the
//! daemon answers a parked `StopWait` long poll with a wake payload — so these
//! tests assert the *answers* (payload text and timing) rather than any spawn.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use forksan_core::protocol::{
    encode, Event, EventKind, Request, RequestBody, Response, ResponseBody,
};
use forksan_core::PROTO_VERSION;

struct Harness {
    _tmp: tempfile::TempDir,
    home: PathBuf,
    socket: PathBuf,
    project: PathBuf,
    daemon: Option<Child>,
}

impl Harness {
    fn new(idle_deadline: &str, wake_debounce: &str) -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        let home = base.join("fsan");
        let project = base.join("proj");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".forksan/forks")).unwrap();
        std::fs::write(
            home.join("config.toml"),
            format!(
                "default_idle_deadline = \"{idle_deadline}\"\nquiet_period = \"1h\"\nwake_debounce = \"{wake_debounce}\"\n",
            ),
        )
        .unwrap();
        Self {
            socket: base.join("d.sock"),
            _tmp: tmp,
            home,
            project,
            daemon: None,
        }
    }

    fn write_fork(&self, rel: &str, content: &str) {
        let path = self.project.join(".forksan/forks").join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn write_transcript(&self, tokens: u64) -> PathBuf {
        let path = self.project.join("transcript.jsonl");
        std::fs::write(
            &path,
            format!(
                "{{\"type\":\"assistant\",\"message\":{{\"model\":\"m\",\"usage\":{{\"input_tokens\":{tokens},\"cache_read_input_tokens\":0,\"cache_creation_input_tokens\":0}}}}}}\n"
            ),
        )
        .unwrap();
        path
    }

    fn start_daemon(&mut self) {
        let child = Command::new(env!("CARGO_BIN_EXE_forksan-daemon"))
            .env("FORKSAN_HOME", &self.home)
            .env("FORKSAN_SOCKET", &self.socket)
            .env("RUST_LOG", "debug")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        self.daemon = Some(child);
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

    fn event(&self, kind: EventKind, session: &str) -> Event {
        Event {
            event: kind,
            session_id: session.to_string(),
            transcript_path: None,
            cwd: self.project.clone(),
            project_root: self.project.clone(),
            source: None,
            model: None,
            enable_tags: None,
            disable_tags: None,
            waking: None,
        }
    }

    /// A waking (`Some(true)`) or non-waking (`Some(false)`) PromptSubmit.
    fn prompt_submit(&self, session: &str, waking: bool) -> Event {
        let mut ev = self.event(EventKind::PromptSubmit, session);
        ev.waking = Some(waking);
        ev
    }

    /// One-shot request/response over a fresh connection.
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

    fn send_event(&self, ev: Event) -> ResponseBody {
        self.request(RequestBody::Event(ev))
    }

    /// Park a StopWait on its own connection/thread; the result arrives on the
    /// returned channel once the daemon answers (Wake or Waited).
    fn park_stop_wait(&self, ev: Event) -> mpsc::Receiver<ResponseBody> {
        let socket = self.socket.clone();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let stream = UnixStream::connect(&socket).unwrap();
            stream
                .set_read_timeout(Some(Duration::from_secs(60)))
                .unwrap();
            let mut writer = stream.try_clone().unwrap();
            let req = Request {
                proto: PROTO_VERSION,
                id: 1,
                body: RequestBody::StopWait(ev),
            };
            writer.write_all(encode(&req).unwrap().as_bytes()).unwrap();
            let mut line = String::new();
            let body = match BufReader::new(stream).read_line(&mut line) {
                Ok(n) if n > 0 => serde_json::from_str::<Response>(line.trim()).unwrap().body,
                // Socket closed (e.g. daemon exited): model it as Waited.
                _ => ResponseBody::Waited,
            };
            let _ = tx.send(body);
        });
        rx
    }

    fn status_recent_runs(&self) -> usize {
        match self.request(RequestBody::Status) {
            ResponseBody::StatusInfo(info) => info.recent_runs.len(),
            other => panic!("unexpected: {other:?}"),
        }
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

fn wake_payload(body: ResponseBody) -> String {
    match body {
        ResponseBody::Wake { payload } => payload,
        other => panic!("expected a Wake, got {other:?}"),
    }
}

#[test]
fn idle_wake_names_the_fork() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "journal.md",
        "---\nfork: true\nrun_on: [idle]\n---\nwrite the journal now",
    );
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));

    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("source: forksan"));
    assert!(payload.contains("due: journal (trigger: idle)"));
    assert!(payload.contains("subagent_type \"fork\""));
    assert!(payload.contains("journal.md"));
    assert!(payload.contains("parent session s1"));
    assert!(payload.contains(&format!("project root {}", h.project.display())));
    assert!(payload.contains("Do not read that file yourself"));
    // overlap default false → skip-if-running line.
    assert!(payload.contains("skip spawning it"));
    // A wake was recorded (throttle stamp at issuance).
    assert_eq!(h.status_recent_runs(), 1);
}

#[test]
fn wake_debounce_zero_is_immediate() {
    let mut h = Harness::new("1s", "0");
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let start = Instant::now();
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(5)).unwrap());
    assert!(payload.contains("due: j"));
    // ~1s idle + no debounce; comfortably under 4s.
    assert!(
        start.elapsed() < Duration::from_secs(4),
        "too slow: {:?}",
        start.elapsed()
    );
}

#[test]
fn prompt_submit_cancels_parked_wait_without_stamping() {
    let mut h = Harness::new("1s", "3");
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();

    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    // Let the fork come due (1s) and enter the 3s debounce, then prompt.
    std::thread::sleep(Duration::from_millis(1500));
    assert_ack(h.send_event(h.event(EventKind::PromptSubmit, "s1")));

    let body = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(body, ResponseBody::Waited),
        "expected Waited, got {body:?}"
    );
    // Cancellation during debounce must not stamp the throttle.
    assert_eq!(h.status_recent_runs(), 0, "throttle stamped despite cancel");
}

#[test]
fn shutdown_resolves_parked_wait() {
    // Long idle so the wait stays parked with nothing due.
    let mut h = Harness::new("1h", "0");
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(300));
    assert_ack(h.request(RequestBody::Shutdown { drain: false }));
    let body = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(body, ResponseBody::Waited),
        "expected Waited, got {body:?}"
    );
}

#[test]
fn disable_tag_filters_fork_but_untagged_wakes() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "tagged.md",
        "---\nfork: true\nrun_on: [idle]\ntags: [ci]\n---\nTAGGED",
    );
    h.write_fork("plain.md", "---\nfork: true\nrun_on: [idle]\n---\nPLAIN");
    h.start_daemon();

    let mut start_ev = h.event(EventKind::SessionStart, "s1");
    start_ev.disable_tags = Some(vec!["ci".into()]);
    assert_ack(h.send_event(start_ev));
    let mut stop = h.event(EventKind::Stop, "s1");
    stop.disable_tags = Some(vec!["ci".into()]);
    let rx = h.park_stop_wait(stop);

    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: plain"));
    assert!(
        !payload.contains("due: tagged"),
        "disabled fork leaked: {payload}"
    );
}

#[test]
fn enable_list_excludes_untagged_fork() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "tagged.md",
        "---\nfork: true\nrun_on: [idle]\ntags: [ci]\n---\nTAGGED",
    );
    h.write_fork("plain.md", "---\nfork: true\nrun_on: [idle]\n---\nPLAIN");
    h.start_daemon();

    let mut start_ev = h.event(EventKind::SessionStart, "s1");
    start_ev.enable_tags = Some(vec!["ci".into()]);
    assert_ack(h.send_event(start_ev));
    let mut stop = h.event(EventKind::Stop, "s1");
    stop.enable_tags = Some(vec!["ci".into()]);
    let rx = h.park_stop_wait(stop);

    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: tagged"));
    assert!(
        !payload.contains("due: plain"),
        "untagged fork ran despite whitelist: {payload}"
    );
}

#[test]
fn throttle_suppresses_second_wake() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "j.md",
        "---\nfork: true\nrun_on: [idle]\nthrottle: 1h\n---\nbody",
    );
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // First turn wakes and stamps the throttle.
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let _ = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());

    // Second turn: within the throttle window, nothing is due; the wait parks,
    // then a prompt cancels it (Waited).
    let rx2 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(1500));
    assert_ack(h.send_event(h.event(EventKind::PromptSubmit, "s1")));
    let body = rx2.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(body, ResponseBody::Waited),
        "throttled fork woke again: {body:?}"
    );
}

#[test]
fn tag_throttle_suppresses_group_but_other_tag_wakes() {
    let mut h = Harness::new("1s", "0");
    // A ci-throttle of 1h; two ci forks and one docs fork.
    std::fs::write(
        h.home.join("config.toml"),
        "default_idle_deadline = \"1s\"\nquiet_period = \"1h\"\nwake_debounce = \"0\"\n[tag_throttles]\nci = \"1h\"\n",
    )
    .unwrap();
    h.write_fork(
        "a.md",
        "---\nfork: true\nrun_on: [idle]\ntags: [ci]\n---\nA",
    );
    h.write_fork(
        "b.md",
        "---\nfork: true\nrun_on: [idle]\ntags: [ci]\n---\nB",
    );
    h.write_fork(
        "c.md",
        "---\nfork: true\nrun_on: [idle]\ntags: [docs]\n---\nC",
    );
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // First wake: all three fire (no prior ci run yet), stamping the ci group.
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: a") && payload.contains("due: b") && payload.contains("due: c"));

    // A new pause (real user activity) releases the once-per-pause latches, so
    // the second turn is decided by the tag throttle alone: the ci group is
    // still suppressed (throttle holds across pauses); the docs fork (c) wakes.
    assert_ack(h.send_event(h.prompt_submit("s1", true)));
    let rx2 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx2.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(
        payload.contains("due: c"),
        "docs fork should still wake: {payload}"
    );
    assert!(
        !payload.contains("due: a"),
        "ci fork a not throttled: {payload}"
    );
    assert!(
        !payload.contains("due: b"),
        "ci fork b not throttled: {payload}"
    );
}

#[test]
fn after_chain_phrased_as_dependency() {
    let mut h = Harness::new("1s", "0");
    h.write_fork("alpha.md", "---\nfork: true\nrun_on: [idle]\n---\nALPHA");
    h.write_fork(
        "beta.md",
        "---\nfork: true\nrun_on: [idle]\nafter: alpha\n---\nBETA",
    );
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());

    // alpha spawns now; beta is deferred until alpha's completion.
    let alpha_pos = payload.find("due: alpha").unwrap();
    let beta_defer = payload.find("Do not spawn fork 'beta' yet").unwrap();
    assert!(
        alpha_pos < beta_defer,
        "root must precede dependent: {payload}"
    );
    assert!(payload.contains("After 'alpha' finish"));
    assert!(payload.contains("include the report(s) they returned"));
}

#[test]
fn context_threshold_wakes_and_latches_once() {
    let mut h = Harness::new("1h", "0"); // long idle: only context can fire
    h.write_fork(
        "ctx.md",
        "---\nfork: true\nrun_on:\n  - context_tokens: 1000\n---\ncontext filling",
    );
    h.start_daemon();
    let transcript = h.write_transcript(2000);

    let mut start = h.event(EventKind::SessionStart, "s1");
    start.transcript_path = Some(transcript.clone());
    assert_ack(h.send_event(start));

    let mut stop = h.event(EventKind::Stop, "s1");
    stop.transcript_path = Some(transcript.clone());
    let rx = h.park_stop_wait(stop);
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(
        payload.contains("due: ctx (trigger: context_tokens:1000)"),
        "{payload}"
    );

    // Second turn: latched, must not re-fire → parks (cancelled by a prompt).
    let mut stop2 = h.event(EventKind::Stop, "s1");
    stop2.transcript_path = Some(transcript);
    let rx2 = h.park_stop_wait(stop2);
    std::thread::sleep(Duration::from_millis(400));
    assert_ack(h.send_event(h.event(EventKind::PromptSubmit, "s1")));
    let body = rx2.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        matches!(body, ResponseBody::Waited),
        "context re-fired: {body:?}"
    );
}

#[test]
fn debounce_batches_forks_across_the_window() {
    // Two idle deadlines 1s apart; a 2s debounce that both land inside.
    let mut h = Harness::new("1s", "2");
    h.write_fork("a.md", "---\nfork: true\nrun_on:\n  - idle: 1\n---\nA");
    h.write_fork("b.md", "---\nfork: true\nrun_on:\n  - idle: 2\n---\nB");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    // Both forks in ONE answer, with a single acknowledgment line.
    assert!(payload.contains("due: a (trigger: idle:1)"), "{payload}");
    assert!(payload.contains("due: b (trigger: idle:2)"), "{payload}");
    assert_eq!(payload.matches("After spawning all forks above").count(), 1);
    // Two wakes stamped in one issuance.
    assert_eq!(h.status_recent_runs(), 2);
}

#[test]
fn idle_fork_fires_at_most_once_per_pause() {
    let mut h = Harness::new("1s", "0");
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // Pause 1: the idle deadline wakes fork j.
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(payload.contains("due: j"));
    assert_eq!(h.status_recent_runs(), 1);

    // The wake turn runs and ends: a non-waking continuation prompt, then its
    // own Stop re-parks. j is latched for this pause — no second wake, even
    // after the idle deadline elapses again.
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    let rx2 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        rx2.try_recv().is_err(),
        "fork re-fired within the same pause"
    );
    // Cancel the still-parked wait (another non-waking prompt).
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    assert!(matches!(
        rx2.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
    // Only the single first wake was ever issued.
    assert_eq!(h.status_recent_runs(), 1);

    // Genuine user activity starts a new pause: j is due again.
    assert_ack(h.send_event(h.prompt_submit("s1", true)));
    let rx3 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let payload = wake_payload(rx3.recv_timeout(Duration::from_secs(10)).unwrap());
    assert!(
        payload.contains("due: j"),
        "new pause did not re-arm the fork"
    );
    assert_eq!(h.status_recent_runs(), 2);
}

#[test]
fn ambiguous_prompt_within_grace_is_treated_as_continuation() {
    // The daemon-side belt: a PromptSubmit with no `waking` flag arriving right
    // after a wake is assumed to be a continuation (no epoch advance).
    let mut h = Harness::new("1s", "0");
    h.write_fork("j.md", "---\nfork: true\nrun_on: [idle]\n---\nbody");
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let _ = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());

    // Ambiguous prompt (waking = None) inside the grace window → non-waking.
    assert_ack(h.send_event(h.event(EventKind::PromptSubmit, "s1")));

    let rx2 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        rx2.try_recv().is_err(),
        "belt failed: ambiguous prompt advanced the pause"
    );
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    assert!(matches!(
        rx2.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
}

#[test]
fn throttle_holds_across_pauses() {
    let mut h = Harness::new("1s", "0");
    h.write_fork(
        "j.md",
        "---\nfork: true\nrun_on: [idle]\nthrottle: 1h\n---\nbody",
    );
    h.start_daemon();
    assert_ack(h.send_event(h.event(EventKind::SessionStart, "s1")));

    // Pause 1: wake, stamping the 1h throttle.
    let rx = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    let _ = wake_payload(rx.recv_timeout(Duration::from_secs(10)).unwrap());

    // A real user prompt starts a fresh pause — but the throttle still holds.
    assert_ack(h.send_event(h.prompt_submit("s1", true)));
    let rx2 = h.park_stop_wait(h.event(EventKind::Stop, "s1"));
    std::thread::sleep(Duration::from_millis(1500));
    assert!(
        rx2.try_recv().is_err(),
        "throttle didn't hold across pauses"
    );
    assert_ack(h.send_event(h.prompt_submit("s1", false)));
    assert!(matches!(
        rx2.recv_timeout(Duration::from_secs(5)).unwrap(),
        ResponseBody::Waited
    ));
}

#[test]
fn list_forks_marks_only_marked_files_and_status_and_shutdown() {
    let mut h = Harness::new("1h", "0");
    h.write_fork(
        "info/FORK.md",
        "---\nfork: true\ndescription: nested fork\nrun_on: [idle]\nthrottle: 5m\n---\nbody",
    );
    // A companion note with fork-like keys but no marker → warned, not a fork.
    h.write_fork("oops.md", "---\nrun_on: [idle]\n---\nnope");
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
            assert_eq!(items[0].throttle_secs, Some(300));
            // The unmarked companion produces a migration warning somewhere.
            let has_warn = items
                .iter()
                .any(|f| f.warnings.iter().any(|w| w.contains("no `fork: true`")));
            assert!(has_warn, "missing fork-like warning: {items:?}");
        }
        other => panic!("unexpected: {other:?}"),
    }

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
