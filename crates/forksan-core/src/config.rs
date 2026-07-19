//! Configuration: `<project_root>/.forksan/config.toml` overrides
//! `~/.forksan/config.toml` overrides built-in defaults. Keys that only make
//! sense globally are ignored (with a warning) when set at project level.

use crate::duration::parse_duration_str;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Effective configuration after layering.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Idle deadline for bare `idle` triggers (seconds). 0 disables them
    /// (which also makes every session end count as a manual stop).
    pub default_idle_deadline_secs: u64,
    /// A session idle longer than this is closed by the boot sweep (seconds).
    pub session_timeout_secs: u64,
    /// The daemon exits after this long with nothing to do (seconds).
    pub quiet_period_secs: u64,
    /// Fork batch width after the leader ran alone.
    pub concurrency: usize,
    /// Kill a fork run after this long (seconds).
    pub fork_timeout_secs: u64,
    /// The Claude Code binary to run forks with.
    pub claude_bin: String,
    /// Assumed model context window for `context_used`/`context_left`.
    pub context_window: u64,
    /// Per-model context window overrides.
    pub models: BTreeMap<String, u64>,
    /// Undelivered reports are dropped after this long (seconds).
    pub report_ttl_secs: u64,
    /// additionalContext budget per delivery (characters).
    pub poll_budget_chars: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_idle_deadline_secs: 600,
            session_timeout_secs: 12 * 3600,
            quiet_period_secs: 20 * 60,
            concurrency: 4,
            fork_timeout_secs: 600,
            claude_bin: "claude".into(),
            context_window: 200_000,
            models: BTreeMap::new(),
            report_ttl_secs: 7 * 86400,
            poll_budget_chars: 24_000,
        }
    }
}

impl Config {
    /// The context window for a model name, honoring per-model overrides.
    pub fn window_for(&self, model: Option<&str>) -> u64 {
        model
            .and_then(|m| self.models.get(m).copied())
            .unwrap_or(self.context_window)
    }
}

/// Raw TOML shape; every key optional so layers can be sparse.
#[derive(Debug, Clone, Default, Deserialize)]
struct RawConfig {
    default_idle_deadline: Option<toml::Value>,
    session_timeout: Option<toml::Value>,
    quiet_period: Option<toml::Value>,
    concurrency: Option<usize>,
    fork_timeout: Option<toml::Value>,
    claude_bin: Option<String>,
    context_window: Option<u64>,
    #[serde(default)]
    models: BTreeMap<String, u64>,
    report_ttl: Option<toml::Value>,
    poll_budget_chars: Option<usize>,
}

/// Keys ignored at project level (global-only concerns).
const GLOBAL_ONLY: &[&str] = &["quiet_period", "claude_bin"];

fn parse_toml_duration(v: &toml::Value, key: &str, warnings: &mut Vec<String>) -> Option<u64> {
    let parsed = match v {
        toml::Value::Integer(n) if *n >= 0 => Some(*n as u64),
        toml::Value::String(s) => parse_duration_str(s),
        _ => None,
    };
    if parsed.is_none() {
        warnings.push(format!("invalid duration for '{key}', ignoring"));
    }
    parsed
}

fn apply_layer(cfg: &mut Config, raw: RawConfig, project_level: bool, warnings: &mut Vec<String>) {
    let mut dur = |v: &Option<toml::Value>, key: &str| -> Option<u64> {
        if project_level && GLOBAL_ONLY.contains(&key) && v.is_some() {
            warnings.push(format!("'{key}' is global-only; ignoring project value"));
            return None;
        }
        v.as_ref()
            .and_then(|v| parse_toml_duration(v, key, warnings))
    };
    if let Some(s) = dur(&raw.default_idle_deadline, "default_idle_deadline") {
        cfg.default_idle_deadline_secs = s;
    }
    if let Some(s) = dur(&raw.session_timeout, "session_timeout") {
        cfg.session_timeout_secs = s;
    }
    if let Some(s) = dur(&raw.quiet_period, "quiet_period") {
        cfg.quiet_period_secs = s;
    }
    if let Some(s) = dur(&raw.fork_timeout, "fork_timeout") {
        cfg.fork_timeout_secs = s;
    }
    if let Some(s) = dur(&raw.report_ttl, "report_ttl") {
        cfg.report_ttl_secs = s;
    }
    if let Some(n) = raw.concurrency {
        cfg.concurrency = n.max(1);
    }
    if let Some(b) = raw.claude_bin {
        if project_level {
            warnings.push("'claude_bin' is global-only; ignoring project value".into());
        } else {
            cfg.claude_bin = b;
        }
    }
    if let Some(n) = raw.context_window {
        cfg.context_window = n;
    }
    cfg.models.extend(raw.models);
    if let Some(n) = raw.poll_budget_chars {
        cfg.poll_budget_chars = n;
    }
}

fn load_layer(path: &Path, warnings: &mut Vec<String>) -> Option<RawConfig> {
    let content = std::fs::read_to_string(path).ok()?;
    match toml::from_str(&content) {
        Ok(raw) => Some(raw),
        Err(e) => {
            warnings.push(format!("invalid config at {}: {e}", path.display()));
            None
        }
    }
}

/// Load the effective config for a project. `home` is the user's home dir.
pub fn load_config(project_root: Option<&Path>, home: Option<&Path>) -> (Config, Vec<String>) {
    let mut cfg = Config::default();
    let mut warnings = Vec::new();
    if let Some(home) = home {
        if let Some(raw) = load_layer(&home.join(".forksan/config.toml"), &mut warnings) {
            apply_layer(&mut cfg, raw, false, &mut warnings);
        }
    }
    if let Some(root) = project_root {
        let path = root.join(".forksan/config.toml");
        // Don't double-apply when the "project" root is the home dir itself.
        if home.map(|h| h.join(".forksan/config.toml")) != Some(path.clone()) {
            if let Some(raw) = load_layer(&path, &mut warnings) {
                apply_layer(&mut cfg, raw, true, &mut warnings);
            }
        }
    }
    for w in &warnings {
        tracing::warn!("config: {w}");
    }
    (cfg, warnings)
}

/// The user's home directory, honoring the `FORKSAN_HOME` test/dev override
/// (which replaces the *`~/.forksan`* base, not `$HOME` itself).
pub fn forksan_home_from_env() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FORKSAN_HOME") {
        let dir = PathBuf::from(dir);
        if dir.is_absolute() {
            return Some(dir);
        }
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".forksan"))
}

/// The daemon's home base: `~/.forksan`.
pub fn forksan_home(home: &Path) -> PathBuf {
    home.join(".forksan")
}

/// Paths under the forksan base dir (`~/.forksan` or `$FORKSAN_HOME`).
pub struct Paths {
    pub base: PathBuf,
}

impl Paths {
    pub fn new(base: PathBuf) -> Self {
        Self { base }
    }

    /// From the environment (`FORKSAN_HOME` override, else `$HOME/.forksan`).
    pub fn from_env() -> Option<Self> {
        forksan_home_from_env().map(Self::new)
    }

    /// The daemon socket path: `$FORKSAN_SOCKET` override, else
    /// `$XDG_RUNTIME_DIR/forksan.sock` when set (kept short — the platform
    /// caps `sun_path` around 100 bytes), else `<base>/run/daemon.sock`.
    pub fn socket(&self) -> PathBuf {
        if let Some(p) = std::env::var_os("FORKSAN_SOCKET") {
            let p = PathBuf::from(p);
            if p.is_absolute() {
                return p;
            }
        }
        if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
            let dir = PathBuf::from(dir);
            if dir.is_absolute() {
                return dir.join("forksan.sock");
            }
        }
        self.base.join("run/daemon.sock")
    }

    /// Held (flocked) by the daemon for its whole life; acquirable = dead.
    pub fn daemon_lock(&self) -> PathBuf {
        self.base.join("run/daemon.lock")
    }

    /// Serializes daemon auto-spawn between racing CLIs.
    pub fn spawn_lock(&self) -> PathBuf {
        self.base.join("run/spawn.lock")
    }

    pub fn db(&self) -> PathBuf {
        self.base.join("state.db")
    }

    pub fn daemon_log(&self) -> PathBuf {
        self.base.join("logs/daemon.log")
    }

    /// The user-level config layer lives at `<base>/config.toml`; the
    /// `load_config` home parameter expects the directory *containing*
    /// `.forksan`, so this exposes the base for direct loading.
    pub fn user_config(&self) -> PathBuf {
        self.base.join("config.toml")
    }
}

/// Load the effective config from explicit layer paths (daemon-side variant
/// of `load_config` that works with a `Paths` base).
pub fn load_config_at(project_root: Option<&Path>, user_config: &Path) -> (Config, Vec<String>) {
    let mut cfg = Config::default();
    let mut warnings = Vec::new();
    if let Some(raw) = load_layer(user_config, &mut warnings) {
        apply_layer(&mut cfg, raw, false, &mut warnings);
    }
    if let Some(root) = project_root {
        let path = root.join(".forksan/config.toml");
        if path != user_config {
            if let Some(raw) = load_layer(&path, &mut warnings) {
                apply_layer(&mut cfg, raw, true, &mut warnings);
            }
        }
    }
    for w in &warnings {
        tracing::warn!("config: {w}");
    }
    (cfg, warnings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn layering_and_global_only() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home");
        let proj = tmp.path().join("proj");
        fs::create_dir_all(home.join(".forksan")).unwrap();
        fs::create_dir_all(proj.join(".forksan")).unwrap();
        fs::write(
            home.join(".forksan/config.toml"),
            "default_idle_deadline = \"5m\"\nquiet_period = \"30m\"\nclaude_bin = \"claude-x\"\n[models]\n\"m1\" = 100\n",
        )
        .unwrap();
        fs::write(
            proj.join(".forksan/config.toml"),
            "default_idle_deadline = 120\nquiet_period = \"1h\"\nconcurrency = 2\n[models]\n\"m2\" = 200\n",
        )
        .unwrap();

        let (cfg, warnings) = load_config(Some(&proj), Some(&home));
        assert_eq!(cfg.default_idle_deadline_secs, 120);
        assert_eq!(cfg.quiet_period_secs, 1800); // project-level ignored
        assert_eq!(cfg.claude_bin, "claude-x");
        assert_eq!(cfg.concurrency, 2);
        assert_eq!(cfg.window_for(Some("m1")), 100);
        assert_eq!(cfg.window_for(Some("m2")), 200);
        assert_eq!(cfg.window_for(Some("other")), 200_000);
        assert_eq!(cfg.window_for(None), 200_000);
        assert!(warnings.iter().any(|w| w.contains("quiet_period")));
    }

    #[test]
    fn defaults_when_nothing_exists() {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg, warnings) = load_config(Some(tmp.path()), Some(tmp.path()));
        assert_eq!(cfg, Config::default());
        assert!(warnings.is_empty());
    }

    #[test]
    fn invalid_values_warn_and_keep_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path();
        fs::create_dir_all(home.join(".forksan")).unwrap();
        fs::write(
            home.join(".forksan/config.toml"),
            "default_idle_deadline = \"soon\"\nconcurrency = 0\n",
        )
        .unwrap();
        let (cfg, warnings) = load_config(None, Some(home));
        assert_eq!(cfg.default_idle_deadline_secs, 600);
        assert_eq!(cfg.concurrency, 1); // clamped
        assert_eq!(warnings.len(), 1);
    }
}
