//! The daemon's SQLite state store. All timestamps are unix epoch seconds.
//!
//! Invariants:
//! - roster: queue-once per (session, fork); running never dequeues; cleared
//!   on session close.
//! - fires latch: context triggers fire at most once per (session, fork,
//!   trigger label).
//! - `forks_ran_at` vs `last_activity` decides the boot sweep's owed forks.
//! - reports: delivered to the origin session while it lives; to any session
//!   in the same project once the origin is closed.

use crate::protocol::{ReportItem, ReportKind};
use crate::{truncate_chars, REPORT_MAX_CHARS};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: i32 = 1;

/// A tracked Claude Code session.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session_id: String,
    pub project_root: PathBuf,
    pub cwd: PathBuf,
    pub transcript_path: Option<PathBuf>,
    pub status: SessionStatus,
    pub last_activity: i64,
    pub forks_ran_at: Option<i64>,
    pub transcript_offset: u64,
    pub prompt_tokens: Option<u64>,
    pub model: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionStatus {
    Open,
    Closed,
}

impl SessionStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionStatus::Open => "open",
            SessionStatus::Closed => "closed",
        }
    }
}

/// A rostered fork for a session.
#[derive(Debug, Clone)]
pub struct RosterEntry {
    pub fork_name: String,
    pub fork_path: PathBuf,
    pub queued_at: i64,
    pub ran_at: Option<i64>,
}

/// The terminal state of a fork run.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: i64,
    pub session_id: String,
    pub fork_name: String,
    pub trigger_label: String,
    pub state: String,
    pub started_at: i64,
    pub finished_at: Option<i64>,
    pub fork_session_id: Option<String>,
    pub cost_usd: Option<f64>,
    pub error: Option<String>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    /// Open (creating/migrating as needed) the store at `path`.
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = Connection::open(path)?;
        Self::init(conn)
    }

    /// An in-memory store (tests).
    pub fn open_in_memory() -> rusqlite::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> rusqlite::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "busy_timeout", 5000)?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let version: i32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version < 1 {
            conn.execute_batch(
                "BEGIN;
                 CREATE TABLE IF NOT EXISTS sessions (
                   session_id        TEXT PRIMARY KEY,
                   project_root      TEXT NOT NULL,
                   cwd               TEXT NOT NULL,
                   transcript_path   TEXT,
                   status            TEXT NOT NULL CHECK(status IN ('open','closed')),
                   last_activity     INTEGER NOT NULL,
                   forks_ran_at      INTEGER,
                   transcript_offset INTEGER NOT NULL DEFAULT 0,
                   prompt_tokens     INTEGER,
                   model             TEXT,
                   created_at        INTEGER NOT NULL
                 );
                 CREATE TABLE IF NOT EXISTS fork_roster (
                   session_id TEXT NOT NULL,
                   fork_name  TEXT NOT NULL,
                   fork_path  TEXT NOT NULL,
                   queued_at  INTEGER NOT NULL,
                   ran_at     INTEGER,
                   PRIMARY KEY (session_id, fork_name)
                 );
                 CREATE TABLE IF NOT EXISTS fork_fires (
                   session_id    TEXT NOT NULL,
                   fork_name     TEXT NOT NULL,
                   trigger_label TEXT NOT NULL,
                   fired_at      INTEGER NOT NULL,
                   PRIMARY KEY (session_id, fork_name, trigger_label)
                 );
                 CREATE TABLE IF NOT EXISTS fork_runs (
                   id              INTEGER PRIMARY KEY AUTOINCREMENT,
                   session_id      TEXT NOT NULL,
                   fork_name       TEXT NOT NULL,
                   trigger_label   TEXT NOT NULL,
                   state           TEXT NOT NULL,
                   started_at      INTEGER NOT NULL,
                   finished_at     INTEGER,
                   fork_session_id TEXT,
                   cost_usd        REAL,
                   error           TEXT
                 );
                 CREATE TABLE IF NOT EXISTS reports (
                   id                   INTEGER PRIMARY KEY AUTOINCREMENT,
                   run_id               INTEGER,
                   origin_session_id    TEXT NOT NULL,
                   project_root         TEXT NOT NULL,
                   fork_name            TEXT NOT NULL,
                   trigger_label        TEXT NOT NULL,
                   kind                 TEXT NOT NULL CHECK(kind IN ('started','response')),
                   body                 TEXT NOT NULL,
                   created_at           INTEGER NOT NULL,
                   delivered_at         INTEGER,
                   delivered_to_session TEXT
                 );
                 CREATE INDEX IF NOT EXISTS idx_reports_pending
                   ON reports (project_root, delivered_at);
                 CREATE INDEX IF NOT EXISTS idx_runs_session ON fork_runs (session_id);
                 COMMIT;",
            )?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        }
        Ok(Self { conn })
    }

    // ---- sessions ----

    /// Register (or re-touch) a session as open.
    #[allow(clippy::too_many_arguments)]
    pub fn upsert_session(
        &self,
        session_id: &str,
        project_root: &Path,
        cwd: &Path,
        transcript_path: Option<&Path>,
        model: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "INSERT INTO sessions (session_id, project_root, cwd, transcript_path, status,
                                   last_activity, created_at, model)
             VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?5, ?6)
             ON CONFLICT(session_id) DO UPDATE SET
               project_root = excluded.project_root,
               cwd = excluded.cwd,
               transcript_path = COALESCE(excluded.transcript_path, transcript_path),
               model = COALESCE(excluded.model, model),
               status = 'open',
               last_activity = excluded.last_activity",
            params![
                session_id,
                project_root.to_string_lossy(),
                cwd.to_string_lossy(),
                transcript_path.map(|p| p.to_string_lossy().into_owned()),
                now,
                model,
            ],
        )?;
        Ok(())
    }

    pub fn get_session(&self, session_id: &str) -> rusqlite::Result<Option<SessionRow>> {
        self.conn
            .query_row(
                "SELECT session_id, project_root, cwd, transcript_path, status, last_activity,
                        forks_ran_at, transcript_offset, prompt_tokens, model, created_at
                 FROM sessions WHERE session_id = ?1",
                params![session_id],
                Self::row_to_session,
            )
            .optional()
    }

    fn row_to_session(row: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
        Ok(SessionRow {
            session_id: row.get(0)?,
            project_root: PathBuf::from(row.get::<_, String>(1)?),
            cwd: PathBuf::from(row.get::<_, String>(2)?),
            transcript_path: row.get::<_, Option<String>>(3)?.map(PathBuf::from),
            status: if row.get::<_, String>(4)? == "open" {
                SessionStatus::Open
            } else {
                SessionStatus::Closed
            },
            last_activity: row.get(5)?,
            forks_ran_at: row.get(6)?,
            transcript_offset: row.get::<_, i64>(7)? as u64,
            prompt_tokens: row.get::<_, Option<i64>>(8)?.map(|n| n as u64),
            model: row.get(9)?,
            created_at: row.get(10)?,
        })
    }

    pub fn list_open_sessions(&self) -> rusqlite::Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, project_root, cwd, transcript_path, status, last_activity,
                    forks_ran_at, transcript_offset, prompt_tokens, model, created_at
             FROM sessions WHERE status = 'open' ORDER BY last_activity DESC",
        )?;
        let rows = stmt.query_map([], Self::row_to_session)?;
        rows.collect()
    }

    /// The most recently active open session in a project.
    pub fn most_recent_open_session(
        &self,
        project_root: &Path,
    ) -> rusqlite::Result<Option<SessionRow>> {
        self.conn
            .query_row(
                "SELECT session_id, project_root, cwd, transcript_path, status, last_activity,
                        forks_ran_at, transcript_offset, prompt_tokens, model, created_at
                 FROM sessions WHERE status = 'open' AND project_root = ?1
                 ORDER BY last_activity DESC LIMIT 1",
                params![project_root.to_string_lossy()],
                Self::row_to_session,
            )
            .optional()
    }

    pub fn set_last_activity(&self, session_id: &str, now: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_activity = ?2 WHERE session_id = ?1",
            params![session_id, now],
        )?;
        Ok(())
    }

    pub fn set_forks_ran_at(&self, session_id: &str, now: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET forks_ran_at = ?2 WHERE session_id = ?1",
            params![session_id, now],
        )?;
        Ok(())
    }

    pub fn set_transcript_gauge(
        &self,
        session_id: &str,
        offset: u64,
        prompt_tokens: Option<u64>,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET transcript_offset = ?2,
                                 prompt_tokens = COALESCE(?3, prompt_tokens)
             WHERE session_id = ?1",
            params![session_id, offset as i64, prompt_tokens.map(|n| n as i64)],
        )?;
        Ok(())
    }

    /// Close a session and clear its roster + latches.
    pub fn close_session(&self, session_id: &str) -> rusqlite::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE sessions SET status = 'closed' WHERE session_id = ?1",
            params![session_id],
        )?;
        tx.execute(
            "DELETE FROM fork_roster WHERE session_id = ?1",
            params![session_id],
        )?;
        tx.execute(
            "DELETE FROM fork_fires WHERE session_id = ?1",
            params![session_id],
        )?;
        tx.commit()
    }

    // ---- roster ----

    /// Queue a fork onto a session's roster. Returns true if newly queued.
    pub fn queue_fork(
        &self,
        session_id: &str,
        fork_name: &str,
        fork_path: &Path,
        now: i64,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO fork_roster (session_id, fork_name, fork_path, queued_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, fork_name, fork_path.to_string_lossy(), now],
        )?;
        Ok(n > 0)
    }

    /// The session's roster, oldest-queued first.
    pub fn roster(&self, session_id: &str) -> rusqlite::Result<Vec<RosterEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT fork_name, fork_path, queued_at, ran_at FROM fork_roster
             WHERE session_id = ?1 ORDER BY queued_at, fork_name",
        )?;
        let rows = stmt.query_map(params![session_id], |row| {
            Ok(RosterEntry {
                fork_name: row.get(0)?,
                fork_path: PathBuf::from(row.get::<_, String>(1)?),
                queued_at: row.get(2)?,
                ran_at: row.get(3)?,
            })
        })?;
        rows.collect()
    }

    /// Record that a rostered fork ran (throttle bookkeeping; never dequeues).
    pub fn touch_fork_ran(
        &self,
        session_id: &str,
        fork_name: &str,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE fork_roster SET ran_at = ?3 WHERE session_id = ?1 AND fork_name = ?2",
            params![session_id, fork_name, now],
        )?;
        Ok(())
    }

    // ---- fires latch ----

    /// Latch a once-per-session trigger. Returns true if newly latched
    /// (i.e. the caller should fire).
    pub fn try_latch_fire(
        &self,
        session_id: &str,
        fork_name: &str,
        trigger_label: &str,
        now: i64,
    ) -> rusqlite::Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO fork_fires (session_id, fork_name, trigger_label, fired_at)
             VALUES (?1, ?2, ?3, ?4)",
            params![session_id, fork_name, trigger_label, now],
        )?;
        Ok(n > 0)
    }

    // ---- runs ----

    pub fn insert_run(
        &self,
        session_id: &str,
        fork_name: &str,
        trigger_label: &str,
        now: i64,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO fork_runs (session_id, fork_name, trigger_label, state, started_at)
             VALUES (?1, ?2, ?3, 'running', ?4)",
            params![session_id, fork_name, trigger_label, now],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    pub fn finish_run(
        &self,
        run_id: i64,
        state: &str,
        fork_session_id: Option<&str>,
        cost_usd: Option<f64>,
        error: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE fork_runs SET state = ?2, finished_at = ?3, fork_session_id = ?4,
                                  cost_usd = ?5, error = ?6
             WHERE id = ?1",
            params![run_id, state, now, fork_session_id, cost_usd, error],
        )?;
        Ok(())
    }

    /// Mark runs left 'running' by a dead daemon as interrupted.
    pub fn mark_stale_runs_interrupted(&self, now: i64) -> rusqlite::Result<usize> {
        self.conn.execute(
            "UPDATE fork_runs SET state = 'interrupted', finished_at = ?1
             WHERE state = 'running'",
            params![now],
        )
    }

    pub fn list_runs(&self, states: &[&str], limit: usize) -> rusqlite::Result<Vec<RunRow>> {
        let placeholders = states.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, session_id, fork_name, trigger_label, state, started_at, finished_at,
                    fork_session_id, cost_usd, error
             FROM fork_runs WHERE state IN ({placeholders})
             ORDER BY started_at DESC, id DESC LIMIT {limit}"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map(rusqlite::params_from_iter(states.iter()), |row| {
            Ok(RunRow {
                id: row.get(0)?,
                session_id: row.get(1)?,
                fork_name: row.get(2)?,
                trigger_label: row.get(3)?,
                state: row.get(4)?,
                started_at: row.get(5)?,
                finished_at: row.get(6)?,
                fork_session_id: row.get(7)?,
                cost_usd: row.get(8)?,
                error: row.get(9)?,
            })
        })?;
        rows.collect()
    }

    /// Session ids of fork runs older than `cutoff` (the GC candidates).
    pub fn fork_session_ids_before(&self, cutoff: i64) -> rusqlite::Result<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT fork_session_id FROM fork_runs
             WHERE fork_session_id IS NOT NULL AND started_at < ?1",
        )?;
        let rows = stmt.query_map(params![cutoff], |row| row.get(0))?;
        rows.collect()
    }

    // ---- reports ----

    #[allow(clippy::too_many_arguments)]
    pub fn insert_report(
        &self,
        run_id: Option<i64>,
        origin_session_id: &str,
        project_root: &Path,
        fork_name: &str,
        trigger_label: &str,
        kind: ReportKind,
        body: &str,
        now: i64,
    ) -> rusqlite::Result<()> {
        let kind = match kind {
            ReportKind::Started => "started",
            ReportKind::Response => "response",
        };
        self.conn.execute(
            "INSERT INTO reports (run_id, origin_session_id, project_root, fork_name,
                                  trigger_label, kind, body, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                run_id,
                origin_session_id,
                project_root.to_string_lossy(),
                fork_name,
                trigger_label,
                kind,
                truncate_chars(body, REPORT_MAX_CHARS),
                now
            ],
        )?;
        Ok(())
    }

    /// Fetch pending reports for a session and mark them delivered, in one
    /// transaction. Eligible: reports from this session, plus reports from
    /// closed sessions in the same project. A `started` marker whose run
    /// already has a pending `response` is collapsed (skipped and marked
    /// delivered). Items past `budget_chars` stay queued for the next poll.
    pub fn poll_reports(
        &self,
        session_id: &str,
        project_root: &Path,
        budget_chars: usize,
        now: i64,
    ) -> rusqlite::Result<Vec<ReportItem>> {
        type Candidate = (i64, Option<i64>, String, String, String, String, i64);
        let tx = self.conn.unchecked_transaction()?;
        let candidates: Vec<Candidate> = {
            let mut stmt = tx.prepare(
                "SELECT r.id, r.run_id, r.fork_name, r.trigger_label, r.kind, r.body, r.created_at
                 FROM reports r
                 WHERE r.delivered_at IS NULL
                   AND (r.origin_session_id = ?1
                        OR (r.project_root = ?2
                            AND NOT EXISTS (SELECT 1 FROM sessions s
                                            WHERE s.session_id = r.origin_session_id
                                              AND s.status = 'open')))
                 ORDER BY r.created_at, r.id",
            )?;
            let rows =
                stmt.query_map(params![session_id, project_root.to_string_lossy()], |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                })?;
            rows.collect::<Result<_, _>>()?
        };

        // Runs with a pending response: their started markers collapse.
        let responded: std::collections::HashSet<i64> = candidates
            .iter()
            .filter(|(_, run_id, _, _, kind, _, _)| kind == "response" && run_id.is_some())
            .map(|(_, run_id, _, _, _, _, _)| run_id.unwrap())
            .collect();

        let mut items = Vec::new();
        let mut delivered_ids = Vec::new();
        let mut used = 0usize;
        for (id, run_id, fork, trigger, kind, body, created_at) in candidates {
            let collapse = kind == "started" && run_id.is_some_and(|r| responded.contains(&r));
            if collapse {
                delivered_ids.push(id);
                continue;
            }
            let cost = body.chars().count();
            if !items.is_empty() && used + cost > budget_chars {
                break;
            }
            used += cost;
            delivered_ids.push(id);
            items.push(ReportItem {
                fork,
                trigger,
                kind: if kind == "started" {
                    ReportKind::Started
                } else {
                    ReportKind::Response
                },
                body,
                created_at,
            });
        }

        for id in &delivered_ids {
            tx.execute(
                "UPDATE reports SET delivered_at = ?2, delivered_to_session = ?3 WHERE id = ?1",
                params![id, now, session_id],
            )?;
        }
        tx.commit()?;
        Ok(items)
    }

    pub fn count_pending_reports(&self) -> rusqlite::Result<u64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM reports WHERE delivered_at IS NULL",
            [],
            |r| r.get::<_, i64>(0).map(|n| n as u64),
        )
    }

    /// Drop expired reports: undelivered past `undelivered_ttl_secs`,
    /// delivered past 48h.
    pub fn expire_reports(&self, now: i64, undelivered_ttl_secs: u64) -> rusqlite::Result<usize> {
        let n1 = self.conn.execute(
            "DELETE FROM reports WHERE delivered_at IS NULL AND created_at < ?1",
            params![now - undelivered_ttl_secs as i64],
        )?;
        let n2 = self.conn.execute(
            "DELETE FROM reports WHERE delivered_at IS NOT NULL AND delivered_at < ?1",
            params![now - 48 * 3600],
        )?;
        Ok(n1 + n2)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn seed_session(s: &Store, sid: &str, root: &str, now: i64) {
        s.upsert_session(sid, Path::new(root), Path::new(root), None, None, now)
            .unwrap();
    }

    #[test]
    fn roster_semantics() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        assert!(s.queue_fork("a", "j", Path::new("/p/j.md"), 100).unwrap());
        assert!(!s.queue_fork("a", "j", Path::new("/p/j.md"), 101).unwrap());
        assert!(s.queue_fork("a", "k", Path::new("/p/k.md"), 102).unwrap());
        // Per-session isolation.
        seed_session(&s, "b", "/p", 100);
        assert!(s.queue_fork("b", "j", Path::new("/p/j.md"), 100).unwrap());

        // Running never dequeues.
        s.touch_fork_ran("a", "j", 200).unwrap();
        let roster = s.roster("a").unwrap();
        assert_eq!(roster.len(), 2);
        assert_eq!(roster[0].fork_name, "j");
        assert_eq!(roster[0].ran_at, Some(200));
        assert_eq!(roster[1].ran_at, None);

        // Close clears roster + latches, and re-queue works after reopen.
        assert!(s.try_latch_fire("a", "j", "context_tokens:5", 200).unwrap());
        s.close_session("a").unwrap();
        assert!(s.roster("a").unwrap().is_empty());
        assert_eq!(
            s.get_session("a").unwrap().unwrap().status,
            SessionStatus::Closed
        );
        seed_session(&s, "a", "/p", 300);
        assert!(s.queue_fork("a", "j", Path::new("/p/j.md"), 300).unwrap());
        assert!(s.try_latch_fire("a", "j", "context_tokens:5", 300).unwrap());
    }

    #[test]
    fn fire_latch_is_once_per_session_per_trigger() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        assert!(s.try_latch_fire("a", "f", "context_used:80%", 100).unwrap());
        assert!(!s.try_latch_fire("a", "f", "context_used:80%", 101).unwrap());
        assert!(s
            .try_latch_fire("a", "f", "context_left:1000", 102)
            .unwrap());
        assert!(s.try_latch_fire("a", "g", "context_used:80%", 103).unwrap());
    }

    #[test]
    fn report_delivery_scoping() {
        let s = store();
        seed_session(&s, "live", "/p", 100);
        seed_session(&s, "dead", "/p", 100);
        seed_session(&s, "other-proj", "/q", 100);

        s.insert_report(
            None,
            "live",
            Path::new("/p"),
            "f",
            "idle",
            ReportKind::Response,
            "own",
            110,
        )
        .unwrap();
        s.insert_report(
            None,
            "dead",
            Path::new("/p"),
            "g",
            "idle",
            ReportKind::Response,
            "dead-own",
            111,
        )
        .unwrap();
        s.insert_report(
            None,
            "other-proj",
            Path::new("/q"),
            "h",
            "idle",
            ReportKind::Response,
            "q",
            112,
        )
        .unwrap();

        // While 'dead' is open, 'live' only sees its own.
        let items = s
            .poll_reports("live", Path::new("/p"), 100_000, 200)
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].body, "own");

        // Once 'dead' closes, its report flows to 'live' too.
        s.close_session("dead").unwrap();
        let items = s
            .poll_reports("live", Path::new("/p"), 100_000, 201)
            .unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].body, "dead-own");

        // Nothing left; /q report untouched.
        assert!(s
            .poll_reports("live", Path::new("/p"), 100_000, 202)
            .unwrap()
            .is_empty());
        assert_eq!(s.count_pending_reports().unwrap(), 1);
    }

    #[test]
    fn started_collapses_when_response_pending() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        let run = s.insert_run("a", "f", "idle", 100).unwrap();
        s.insert_report(
            Some(run),
            "a",
            Path::new("/p"),
            "f",
            "idle",
            ReportKind::Started,
            "started f",
            100,
        )
        .unwrap();
        // Another run still in flight: its started marker must survive.
        let run2 = s.insert_run("a", "g", "idle", 100).unwrap();
        s.insert_report(
            Some(run2),
            "a",
            Path::new("/p"),
            "g",
            "idle",
            ReportKind::Started,
            "started g",
            101,
        )
        .unwrap();
        s.insert_report(
            Some(run),
            "a",
            Path::new("/p"),
            "f",
            "idle",
            ReportKind::Response,
            "done f",
            105,
        )
        .unwrap();

        let items = s.poll_reports("a", Path::new("/p"), 100_000, 200).unwrap();
        let kinds: Vec<(String, ReportKind)> =
            items.iter().map(|i| (i.fork.clone(), i.kind)).collect();
        assert_eq!(
            kinds,
            vec![
                ("g".to_string(), ReportKind::Started),
                ("f".to_string(), ReportKind::Response)
            ]
        );
        // Collapsed marker is gone for good.
        assert!(s
            .poll_reports("a", Path::new("/p"), 100_000, 201)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn budget_cutoff_keeps_remainder_queued() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        for i in 0..3 {
            s.insert_report(
                None,
                "a",
                Path::new("/p"),
                "f",
                "idle",
                ReportKind::Response,
                &"x".repeat(1000),
                100 + i,
            )
            .unwrap();
        }
        let items = s.poll_reports("a", Path::new("/p"), 2500, 200).unwrap();
        assert_eq!(items.len(), 2);
        let items = s.poll_reports("a", Path::new("/p"), 2500, 201).unwrap();
        assert_eq!(items.len(), 1);
        // A single oversized item still delivers (never wedges).
        s.insert_report(
            None,
            "a",
            Path::new("/p"),
            "f",
            "idle",
            ReportKind::Response,
            &"y".repeat(5000),
            300,
        )
        .unwrap();
        let items = s.poll_reports("a", Path::new("/p"), 100, 400).unwrap();
        assert_eq!(items.len(), 1);
    }

    #[test]
    fn report_body_capped_and_ttl() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        s.insert_report(
            None,
            "a",
            Path::new("/p"),
            "f",
            "idle",
            ReportKind::Response,
            &"z".repeat(REPORT_MAX_CHARS + 500),
            100,
        )
        .unwrap();
        let items = s
            .poll_reports("a", Path::new("/p"), usize::MAX, 200)
            .unwrap();
        assert!(items[0].body.ends_with("…(truncated)"));
        assert!(items[0].body.chars().count() <= REPORT_MAX_CHARS + 20);

        // TTL: undelivered dropped after ttl, delivered after 48h.
        s.insert_report(
            None,
            "a",
            Path::new("/p"),
            "f",
            "idle",
            ReportKind::Response,
            "old",
            100,
        )
        .unwrap();
        let dropped = s.expire_reports(100 + 7 * 86400 + 1, 7 * 86400).unwrap();
        assert_eq!(dropped, 2); // the old undelivered + the delivered one past 48h
        assert_eq!(s.count_pending_reports().unwrap(), 0);
    }

    #[test]
    fn runs_lifecycle_and_stale_marking() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        let id = s.insert_run("a", "f", "idle", 100).unwrap();
        assert_eq!(s.list_runs(&["running"], 10).unwrap().len(), 1);
        s.finish_run(id, "done", Some("fork-sid"), Some(0.01), None, 150)
            .unwrap();
        assert!(s.list_runs(&["running"], 10).unwrap().is_empty());
        let done = s.list_runs(&["done"], 10).unwrap();
        assert_eq!(done[0].fork_session_id.as_deref(), Some("fork-sid"));

        let id2 = s.insert_run("a", "g", "idle", 160).unwrap();
        let _ = id2;
        assert_eq!(s.mark_stale_runs_interrupted(200).unwrap(), 1);
        assert_eq!(s.list_runs(&["interrupted"], 10).unwrap().len(), 1);
        assert_eq!(
            s.fork_session_ids_before(1000).unwrap(),
            vec!["fork-sid".to_string()]
        );
    }

    #[test]
    fn most_recent_open_session_per_project() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        seed_session(&s, "b", "/p", 200);
        seed_session(&s, "c", "/q", 300);
        assert_eq!(
            s.most_recent_open_session(Path::new("/p"))
                .unwrap()
                .unwrap()
                .session_id,
            "b"
        );
        s.close_session("b").unwrap();
        assert_eq!(
            s.most_recent_open_session(Path::new("/p"))
                .unwrap()
                .unwrap()
                .session_id,
            "a"
        );
    }
}
