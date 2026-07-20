//! The daemon's SQLite state store. All timestamps are unix epoch seconds.
//!
//! Invariants:
//! - roster: queue-once per (session, fork); running never dequeues; cleared
//!   on session close.
//! - fires latch: context triggers fire at most once per (session, fork,
//!   trigger label).
//! - runs: since v0.5 a "run" is a wake issued to a session (state `issued`),
//!   recorded so per-tag throttles can find the last wake per tag. The daemon
//!   no longer observes fork completion.
//!
//! The schema is unchanged from v0.4 (v3): the `reports` table still exists
//! but is no longer written or read (report delivery is native in v0.5).

use rusqlite::{params, Connection, OptionalExtension};
use std::path::{Path, PathBuf};

const SCHEMA_VERSION: i32 = 4;

/// Split a comma-joined tag column back into a list (trimmed, empties
/// dropped). `NULL` (unset) stays `None`.
fn split_tags(s: Option<String>) -> Option<Vec<String>> {
    s.map(|s| {
        s.split(',')
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .collect()
    })
}

/// A tracked Claude Code session.
#[derive(Debug, Clone)]
pub struct SessionRow {
    pub session_id: String,
    pub project_root: PathBuf,
    pub cwd: PathBuf,
    pub transcript_path: Option<PathBuf>,
    pub status: SessionStatus,
    pub last_activity: i64,
    pub transcript_offset: u64,
    pub prompt_tokens: Option<u64>,
    pub model: Option<String>,
    pub created_at: i64,
    /// Per-session enable (whitelist) tag filter; `None` = unset.
    pub enable_tags: Option<Vec<String>>,
    /// Per-session disable (blocklist) tag filter; `None` = unset.
    pub disable_tags: Option<Vec<String>>,
    /// Advances only on genuine user activity (a real UserPromptSubmit). Idle
    /// forks latch per (fork, pause_epoch): once per pause.
    pub pause_epoch: i64,
    /// The Stop that began the current pause; idle deadlines are measured from
    /// here, so wake-turn Stops don't reset the clock. `None` until the first
    /// Stop of a pause.
    pub pause_started_at: Option<i64>,
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

/// A recorded wake (state `issued`).
#[derive(Debug, Clone)]
pub struct RunRow {
    pub id: i64,
    pub session_id: String,
    pub fork_name: String,
    pub trigger_label: String,
    pub state: String,
    pub started_at: i64,
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
        }
        if version < 2 {
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE sessions ADD COLUMN enable_tags TEXT;
                 ALTER TABLE sessions ADD COLUMN disable_tags TEXT;
                 COMMIT;",
            )?;
        }
        if version < 3 {
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE fork_runs ADD COLUMN tags TEXT;
                 COMMIT;",
            )?;
        }
        if version < 4 {
            // Per-session pause epoch (advanced only by genuine user activity)
            // and the pause baseline (first Stop of the current pause) — the
            // once-per-pause idle latch and idle-deadline timing key off these.
            conn.execute_batch(
                "BEGIN;
                 ALTER TABLE sessions ADD COLUMN pause_epoch INTEGER NOT NULL DEFAULT 0;
                 ALTER TABLE sessions ADD COLUMN pause_started_at INTEGER;
                 COMMIT;",
            )?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
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
        enable_tags: Option<&str>,
        disable_tags: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<()> {
        // `cwd` is pinned to the first event's value (first write wins): a
        // session's cwd drifts as its Bash tool `cd`s around, but the launch
        // directory is the stable per-session identity. `transcript_path` does
        // not drift, so COALESCE is fine. The per-session tag filter always
        // reflects the latest event, so it overwrites (a cleared env clears it).
        self.conn.execute(
            "INSERT INTO sessions (session_id, project_root, cwd, transcript_path, status,
                                   last_activity, created_at, model, enable_tags, disable_tags)
             VALUES (?1, ?2, ?3, ?4, 'open', ?5, ?5, ?6, ?7, ?8)
             ON CONFLICT(session_id) DO UPDATE SET
               project_root = excluded.project_root,
               transcript_path = COALESCE(excluded.transcript_path, transcript_path),
               model = COALESCE(excluded.model, model),
               enable_tags = excluded.enable_tags,
               disable_tags = excluded.disable_tags,
               status = 'open',
               last_activity = excluded.last_activity",
            params![
                session_id,
                project_root.to_string_lossy(),
                cwd.to_string_lossy(),
                transcript_path.map(|p| p.to_string_lossy().into_owned()),
                now,
                model,
                enable_tags,
                disable_tags,
            ],
        )?;
        Ok(())
    }

    pub fn get_session(&self, session_id: &str) -> rusqlite::Result<Option<SessionRow>> {
        self.conn
            .query_row(
                "SELECT session_id, project_root, cwd, transcript_path, status, last_activity,
                        transcript_offset, prompt_tokens, model, created_at,
                        enable_tags, disable_tags, pause_epoch, pause_started_at
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
            transcript_offset: row.get::<_, i64>(6)? as u64,
            prompt_tokens: row.get::<_, Option<i64>>(7)?.map(|n| n as u64),
            model: row.get(8)?,
            created_at: row.get(9)?,
            enable_tags: split_tags(row.get::<_, Option<String>>(10)?),
            disable_tags: split_tags(row.get::<_, Option<String>>(11)?),
            pause_epoch: row.get(12)?,
            pause_started_at: row.get(13)?,
        })
    }

    /// Advance the pause epoch and clear the pause baseline (genuine user
    /// activity begins a new pause).
    pub fn bump_pause_epoch(&self, session_id: &str) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET pause_epoch = pause_epoch + 1, pause_started_at = NULL
             WHERE session_id = ?1",
            params![session_id],
        )?;
        Ok(())
    }

    /// Set the pause baseline to `now` only if it is unset (the first Stop of
    /// the current pause). Wake-turn Stops leave the existing baseline.
    pub fn set_pause_started_at_if_unset(
        &self,
        session_id: &str,
        now: i64,
    ) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET pause_started_at = ?2
             WHERE session_id = ?1 AND pause_started_at IS NULL",
            params![session_id, now],
        )?;
        Ok(())
    }

    pub fn list_open_sessions(&self) -> rusqlite::Result<Vec<SessionRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT session_id, project_root, cwd, transcript_path, status, last_activity,
                    transcript_offset, prompt_tokens, model, created_at,
                    enable_tags, disable_tags, pause_epoch, pause_started_at
             FROM sessions WHERE status = 'open' ORDER BY last_activity DESC",
        )?;
        let rows = stmt.query_map([], Self::row_to_session)?;
        rows.collect()
    }

    pub fn set_last_activity(&self, session_id: &str, now: i64) -> rusqlite::Result<()> {
        self.conn.execute(
            "UPDATE sessions SET last_activity = ?2 WHERE session_id = ?1",
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

    /// Record that a rostered fork was woken (per-fork throttle bookkeeping;
    /// never dequeues).
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

    /// Whether a once-per-session trigger is already latched (read-only, used
    /// during selection so evaluation stays side-effect-free until issuance).
    pub fn is_latched(
        &self,
        session_id: &str,
        fork_name: &str,
        trigger_label: &str,
    ) -> rusqlite::Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM fork_fires
             WHERE session_id = ?1 AND fork_name = ?2 AND trigger_label = ?3",
            params![session_id, fork_name, trigger_label],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }

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

    // ---- runs (issued wakes) ----

    /// Record that a wake was issued for a fork (state `issued`). `tags` is the
    /// fork's comma-joined tags (NULL when untagged) so per-tag throttles can
    /// find the last wake per tag.
    pub fn record_issued_run(
        &self,
        session_id: &str,
        fork_name: &str,
        trigger_label: &str,
        tags: Option<&str>,
        now: i64,
    ) -> rusqlite::Result<i64> {
        self.conn.execute(
            "INSERT INTO fork_runs (session_id, fork_name, trigger_label, state, started_at,
                                    finished_at, tags)
             VALUES (?1, ?2, ?3, 'issued', ?4, ?4, ?5)",
            params![session_id, fork_name, trigger_label, now, tags],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// The most recent issued wake (across the project) of any fork carrying
    /// one of `tags`, for per-tag throttling. `None` when none exists.
    pub fn last_run_for_tags(
        &self,
        project_root: &Path,
        tags: &[String],
    ) -> rusqlite::Result<Option<i64>> {
        if tags.is_empty() {
            return Ok(None);
        }
        let mut stmt = self.conn.prepare(
            "SELECT r.started_at, r.tags FROM fork_runs r
             JOIN sessions s ON s.session_id = r.session_id
             WHERE s.project_root = ?1 AND r.tags IS NOT NULL
             ORDER BY r.started_at DESC",
        )?;
        let mut rows = stmt.query(params![project_root.to_string_lossy()])?;
        while let Some(row) = rows.next()? {
            let started_at: i64 = row.get(0)?;
            let row_tags: String = row.get(1)?;
            let hit = row_tags
                .split(',')
                .map(|t| t.trim())
                .filter(|t| !t.is_empty())
                .any(|rt| tags.iter().any(|t| t == rt));
            if hit {
                return Ok(Some(started_at));
            }
        }
        Ok(None)
    }

    pub fn list_runs(&self, states: &[&str], limit: usize) -> rusqlite::Result<Vec<RunRow>> {
        let placeholders = states.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT id, session_id, fork_name, trigger_label, state, started_at
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
            })
        })?;
        rows.collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().unwrap()
    }

    fn seed_session(s: &Store, sid: &str, root: &str, now: i64) {
        s.upsert_session(
            sid,
            Path::new(root),
            Path::new(root),
            None,
            None,
            None,
            None,
            now,
        )
        .unwrap();
    }

    #[test]
    fn roster_semantics() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        assert!(s.queue_fork("a", "j", Path::new("/p/j.md"), 100).unwrap());
        assert!(!s.queue_fork("a", "j", Path::new("/p/j.md"), 101).unwrap());
        assert!(s.queue_fork("a", "k", Path::new("/p/k.md"), 102).unwrap());
        seed_session(&s, "b", "/p", 100);
        assert!(s.queue_fork("b", "j", Path::new("/p/j.md"), 100).unwrap());

        s.touch_fork_ran("a", "j", 200).unwrap();
        let roster = s.roster("a").unwrap();
        assert_eq!(roster.len(), 2);
        assert_eq!(roster[0].fork_name, "j");
        assert_eq!(roster[0].ran_at, Some(200));
        assert_eq!(roster[1].ran_at, None);

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
    fn issued_runs_and_listing() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        s.record_issued_run("a", "f", "idle", Some("ci"), 100)
            .unwrap();
        let runs = s.list_runs(&["issued"], 10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].fork_name, "f");
        assert_eq!(runs[0].state, "issued");
    }

    #[test]
    fn last_run_for_tags_finds_latest_across_project() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        seed_session(&s, "b", "/q", 100);

        assert_eq!(
            s.last_run_for_tags(Path::new("/p"), &["ci".to_string()])
                .unwrap(),
            None
        );

        s.record_issued_run("a", "f", "manual", Some("ci,build"), 110)
            .unwrap();
        s.record_issued_run("a", "g", "manual", Some("review"), 120)
            .unwrap();
        s.record_issued_run("a", "h", "manual", None, 130).unwrap();

        assert_eq!(
            s.last_run_for_tags(Path::new("/p"), &["ci".to_string()])
                .unwrap(),
            Some(110)
        );
        assert_eq!(
            s.last_run_for_tags(Path::new("/p"), &["build".to_string()])
                .unwrap(),
            Some(110)
        );
        assert_eq!(
            s.last_run_for_tags(Path::new("/p"), &["bui".to_string()])
                .unwrap(),
            None
        );
        assert_eq!(
            s.last_run_for_tags(Path::new("/p"), &["ci".to_string(), "review".to_string()])
                .unwrap(),
            Some(120)
        );
        assert_eq!(
            s.last_run_for_tags(Path::new("/q"), &["ci".to_string()])
                .unwrap(),
            None
        );
        assert_eq!(s.last_run_for_tags(Path::new("/p"), &[]).unwrap(), None);
    }

    #[test]
    fn tag_filter_persists_and_latest_event_wins() {
        let s = store();
        s.upsert_session(
            "a",
            Path::new("/p"),
            Path::new("/p"),
            None,
            None,
            Some("ci,review"),
            Some("noisy"),
            100,
        )
        .unwrap();
        let row = s.get_session("a").unwrap().unwrap();
        assert_eq!(
            row.enable_tags,
            Some(vec!["ci".to_string(), "review".to_string()])
        );
        assert_eq!(row.disable_tags, Some(vec!["noisy".to_string()]));

        s.upsert_session(
            "a",
            Path::new("/p"),
            Path::new("/p"),
            None,
            None,
            None,
            None,
            101,
        )
        .unwrap();
        let row = s.get_session("a").unwrap().unwrap();
        assert_eq!(row.enable_tags, None);
        assert_eq!(row.disable_tags, None);
    }

    #[test]
    fn pause_epoch_and_baseline() {
        let s = store();
        seed_session(&s, "a", "/p", 100);
        let row = s.get_session("a").unwrap().unwrap();
        assert_eq!(row.pause_epoch, 0);
        assert_eq!(row.pause_started_at, None);

        // First Stop of a pause sets the baseline; later Stops keep it.
        s.set_pause_started_at_if_unset("a", 110).unwrap();
        s.set_pause_started_at_if_unset("a", 120).unwrap();
        assert_eq!(
            s.get_session("a").unwrap().unwrap().pause_started_at,
            Some(110)
        );

        // Genuine activity advances the epoch and clears the baseline.
        s.bump_pause_epoch("a").unwrap();
        let row = s.get_session("a").unwrap().unwrap();
        assert_eq!(row.pause_epoch, 1);
        assert_eq!(row.pause_started_at, None);
        s.set_pause_started_at_if_unset("a", 200).unwrap();
        assert_eq!(
            s.get_session("a").unwrap().unwrap().pause_started_at,
            Some(200)
        );
    }

    #[test]
    fn cwd_is_pinned_to_first_event() {
        let s = store();
        s.upsert_session(
            "a",
            Path::new("/home/proj"),
            Path::new("/home/proj"),
            None,
            None,
            None,
            None,
            100,
        )
        .unwrap();
        s.upsert_session(
            "a",
            Path::new("/home/proj"),
            Path::new("/home/proj/vendor/thing"),
            None,
            None,
            None,
            None,
            200,
        )
        .unwrap();
        let row = s.get_session("a").unwrap().unwrap();
        assert_eq!(row.cwd, PathBuf::from("/home/proj"));
        assert_eq!(row.last_activity, 200);
    }
}
