//! The fork definition format: a markdown file with YAML frontmatter.
//!
//! Top-level keys only (`description`, `run_on`, `delivery`, `throttle`,
//! `after`, `model`); unknown keys are ignored for forward compatibility,
//! and invalid values warn and fall back rather than dropping the fork.
//! There is deliberately no RAG surface in this format.

use crate::duration::parse_duration_yaml;
use serde::Deserialize;

/// A parsed fork definition (frontmatter only; the body is the prompt).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkDef {
    /// Human documentation only — forksan never feeds this to a model or an
    /// embedding index.
    pub description: Option<String>,
    /// The fork moments this fork fires at.
    pub run_on: Vec<ForkRunOn>,
    /// Where the fork's boundary events (start marker + final report) land
    /// in the parent session.
    pub delivery: ForkDelivery,
    /// Minimum seconds between two runs of this fork within a session.
    pub throttle_secs: Option<u64>,
    /// Sequencing: run after another fork finishes at the same moment.
    pub after: Option<ForkAfter>,
    /// Optional model override for the fork run.
    pub model: Option<String>,
}

impl Default for ForkDef {
    fn default() -> Self {
        Self {
            description: None,
            run_on: default_run_on(),
            delivery: ForkDelivery::default(),
            throttle_secs: None,
            after: None,
            model: None,
        }
    }
}

/// The default fork moments: every idle pause (at the default idle deadline)
/// and context compaction.
pub fn default_run_on() -> Vec<ForkRunOn> {
    vec![ForkRunOn::Idle { after_secs: None }, ForkRunOn::Compact]
}

/// Where a fork's boundary events land in the parent session.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ForkDelivery {
    /// No boundary events: the report is thrown away; the fork runs for its
    /// tool side effects only.
    Discard,
    /// Both events are queued and injected as context on the parent's next
    /// turn (or the next session in the same project if the parent is gone).
    #[default]
    NextTurn,
}

/// Sequencing config: run this fork after another one finishes at the same
/// fork moment. If the referenced fork does not fire at that moment, the
/// dependent simply runs independently.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ForkAfter {
    /// The fork to wait for.
    pub fork: String,
    /// What context the dependent fork gets.
    pub context: ForkAfterContext,
}

/// The context a sequenced fork runs in.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ForkAfterContext {
    /// Fork the parent session as usual; the predecessor's final report is
    /// piped into this fork's prompt.
    #[default]
    Parent,
    /// Fork the *predecessor fork's* resulting session ("fork a fork"): this
    /// fork sees everything the predecessor saw and did.
    Fork,
}

/// A fork moment a fork fires at (`run_on`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkRunOn {
    /// An idle pause. With `after_secs` unset, fires at the configured
    /// default idle deadline; with it set (`- idle: 20m`), fires once the
    /// session has been idle that long.
    Idle { after_secs: Option<u64> },
    /// Context compaction (auto or manual).
    Compact,
    /// A new session starting.
    SessionStart,
    /// The session ending, for any reason.
    SessionEnd,
    /// The session ending *while recently active* (idle time below the
    /// default idle deadline) — closed mid-conversation rather than timed
    /// out.
    ManualStop,
    /// Daemon startup with this session still tracked (owed-fork sweep).
    Boot,
    /// The session's prompt token count reached this absolute value.
    ContextTokens(u64),
    /// The session used at least this percentage of the model's context
    /// window (requires a configured context window).
    ContextUsedPct(u8),
    /// At most this many tokens remain in the model's context window.
    ContextLeft(u64),
}

impl ForkRunOn {
    /// Stable label used in fire frontmatter and (for context triggers) as
    /// the once-per-session latch key.
    pub fn label(&self) -> String {
        match self {
            ForkRunOn::Idle { after_secs: None } => "idle".into(),
            ForkRunOn::Idle {
                after_secs: Some(s),
            } => format!("idle:{s}"),
            ForkRunOn::Compact => "compact".into(),
            ForkRunOn::SessionStart => "session_start".into(),
            ForkRunOn::SessionEnd => "session_end".into(),
            ForkRunOn::ManualStop => "manual_stop".into(),
            ForkRunOn::Boot => "boot".into(),
            ForkRunOn::ContextTokens(n) => format!("context_tokens:{n}"),
            ForkRunOn::ContextUsedPct(p) => format!("context_used:{p}%"),
            ForkRunOn::ContextLeft(n) => format!("context_left:{n}"),
        }
    }
}

fn parse_percent(v: &serde_yaml::Value) -> Option<u8> {
    let n = match v {
        serde_yaml::Value::Number(n) => n.as_u64()?,
        serde_yaml::Value::String(s) => s.trim().trim_end_matches('%').trim().parse().ok()?,
        _ => return None,
    };
    (1..=100).contains(&n).then_some(n as u8)
}

fn parse_token_count(v: &serde_yaml::Value) -> Option<u64> {
    match v {
        serde_yaml::Value::Number(n) => n.as_u64(),
        serde_yaml::Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Parse one `run_on` entry: a plain string (`idle`, `compact`, …) or a
/// single-key map (`idle: 20m`, `context_tokens: 150000`, …).
fn parse_run_on_entry(v: &serde_yaml::Value, warnings: &mut Vec<String>) -> Option<ForkRunOn> {
    if let serde_yaml::Value::String(s) = v {
        return match s.as_str() {
            "idle" => Some(ForkRunOn::Idle { after_secs: None }),
            "compact" | "compaction" => Some(ForkRunOn::Compact),
            "session_start" => Some(ForkRunOn::SessionStart),
            "session_end" => Some(ForkRunOn::SessionEnd),
            "manual_stop" => Some(ForkRunOn::ManualStop),
            "boot" => Some(ForkRunOn::Boot),
            other => {
                warnings.push(format!("unknown run_on trigger '{other}', skipping"));
                None
            }
        };
    }
    if let serde_yaml::Value::Mapping(m) = v {
        if m.len() == 1 {
            if let Some((serde_yaml::Value::String(key), val)) = m.iter().next() {
                let parsed = match key.as_str() {
                    "idle" => parse_duration_yaml(val).map(|s| ForkRunOn::Idle {
                        after_secs: Some(s),
                    }),
                    "context_tokens" => parse_token_count(val).map(ForkRunOn::ContextTokens),
                    "context_used" => parse_percent(val).map(ForkRunOn::ContextUsedPct),
                    "context_left" => parse_token_count(val).map(ForkRunOn::ContextLeft),
                    other => {
                        warnings.push(format!("unknown run_on trigger '{other}', skipping"));
                        return None;
                    }
                };
                if parsed.is_none() {
                    warnings.push(format!("invalid run_on value for '{key}', skipping"));
                }
                return parsed;
            }
        }
    }
    warnings.push("malformed run_on entry, skipping".into());
    None
}

/// Parse `after`: a plain fork-name string, or a map
/// `{fork: <name>, context: parent|fork}` (`skill:` accepted as an alias for
/// the key, for definitions shared with other tools).
fn parse_after(v: &serde_yaml::Value, name: &str, warnings: &mut Vec<String>) -> Option<ForkAfter> {
    match v {
        serde_yaml::Value::String(s) if !s.trim().is_empty() => Some(ForkAfter {
            fork: s.trim().to_string(),
            context: ForkAfterContext::Parent,
        }),
        serde_yaml::Value::Mapping(m) => {
            let get = |k: &str| {
                m.get(serde_yaml::Value::String(k.into()))
                    .and_then(|v| v.as_str())
            };
            let dep = get("fork")
                .or_else(|| get("skill"))
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty());
            let Some(dep) = dep else {
                warnings.push(format!("fork '{name}': after is missing 'fork', ignoring"));
                return None;
            };
            let context = match get("context") {
                Some("fork") => ForkAfterContext::Fork,
                Some("parent") | None => ForkAfterContext::Parent,
                Some(other) => {
                    warnings.push(format!(
                        "fork '{name}': unknown after context '{other}'; using 'parent'"
                    ));
                    ForkAfterContext::Parent
                }
            };
            Some(ForkAfter { fork: dep, context })
        }
        _ => {
            warnings.push(format!("fork '{name}': malformed after value, ignoring"));
            None
        }
    }
}

#[derive(Deserialize, Default)]
struct RawFork {
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    run_on: Option<Vec<serde_yaml::Value>>,
    #[serde(default)]
    delivery: Option<String>,
    #[serde(default)]
    throttle: Option<serde_yaml::Value>,
    #[serde(default)]
    after: Option<serde_yaml::Value>,
    #[serde(default)]
    model: Option<String>,
    // Recognized-and-rejected: the format has no RAG surface.
    #[serde(default)]
    rag: Option<serde_yaml::Value>,
    #[serde(default)]
    triggers: Option<serde_yaml::Value>,
}

/// The result of parsing a fork file: the validated definition, its prompt
/// body, and any warnings produced by fallbacks.
#[derive(Debug, Clone)]
pub struct ParsedFork {
    pub def: ForkDef,
    pub body: String,
    pub warnings: Vec<String>,
}

/// Split a markdown file into its `---`-delimited YAML frontmatter and body.
/// Files without frontmatter are valid: an all-defaults fork whose whole
/// content is the body.
fn split_frontmatter(content: &str) -> (Option<&str>, &str) {
    let Some(rest) = content.strip_prefix("---") else {
        return (None, content);
    };
    let Some(rest) = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
    else {
        return (None, content);
    };
    let mut search_from = 0;
    while let Some(pos) = rest[search_from..].find("\n---") {
        let abs = search_from + pos;
        let front = rest[..abs].strip_suffix('\r').unwrap_or(&rest[..abs]);
        let tail = &rest[abs + 4..];
        let tail = tail.strip_prefix('\r').unwrap_or(tail);
        if tail.is_empty() {
            return (Some(front), "");
        }
        if let Some(body) = tail.strip_prefix('\n') {
            return (Some(front), body);
        }
        search_from = abs + 4;
    }
    (None, content)
}

/// Parse a fork file's full content. `name` is used only for warning text.
/// Returns `None` when the frontmatter block exists but is not valid YAML —
/// the one unrecoverable shape.
pub fn parse_fork_file(name: &str, content: &str) -> Option<ParsedFork> {
    let mut warnings = Vec::new();
    let (front, body) = split_frontmatter(content);
    let raw: RawFork = match front {
        None => RawFork::default(),
        Some(yaml) => match serde_yaml::from_str(yaml) {
            Ok(raw) => raw,
            Err(e) => {
                tracing::warn!(fork = name, error = %e, "Invalid fork frontmatter YAML, skipping fork");
                return None;
            }
        },
    };

    if raw.rag.is_some() || raw.triggers.is_some() {
        warnings.push(format!(
            "fork '{name}': RAG keys are not part of the fork format, ignoring"
        ));
    }

    let delivery = match raw.delivery.as_deref() {
        Some("discard") => ForkDelivery::Discard,
        Some("next_turn") | None => ForkDelivery::NextTurn,
        Some("immediate") => {
            warnings.push(format!(
                "fork '{name}': delivery 'immediate' is not supported here; using 'next_turn'"
            ));
            ForkDelivery::NextTurn
        }
        Some(other) => {
            warnings.push(format!(
                "fork '{name}': unknown delivery '{other}'; using 'next_turn'"
            ));
            ForkDelivery::NextTurn
        }
    };

    let throttle_secs = match &raw.throttle {
        None => None,
        Some(v) => {
            let parsed = parse_duration_yaml(v).filter(|s| *s > 0);
            if parsed.is_none() {
                warnings.push(format!("fork '{name}': invalid throttle value, ignoring"));
            }
            parsed
        }
    };

    let run_on = match raw.run_on {
        None => default_run_on(),
        Some(entries) => {
            let parsed: Vec<ForkRunOn> = entries
                .iter()
                .filter_map(|e| parse_run_on_entry(e, &mut warnings))
                .collect();
            if parsed.is_empty() {
                warnings.push(format!(
                    "fork '{name}': run_on has no valid triggers; using the defaults"
                ));
                default_run_on()
            } else {
                parsed
            }
        }
    };

    let mut after = raw
        .after
        .as_ref()
        .and_then(|v| parse_after(v, name, &mut warnings));
    if after.as_ref().is_some_and(|a| a.fork == name) {
        warnings.push(format!("fork '{name}': after references itself, ignoring"));
        after = None;
    }

    for w in &warnings {
        tracing::warn!(fork = name, "{w}");
    }

    Some(ParsedFork {
        def: ForkDef {
            description: raw.description.filter(|d| !d.trim().is_empty()),
            run_on,
            delivery,
            throttle_secs,
            after,
            model: raw.model.filter(|m| !m.trim().is_empty()),
        },
        body: body.to_string(),
        warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(content: &str) -> ParsedFork {
        parse_fork_file("test", content).expect("parse")
    }

    #[test]
    fn defaults_without_frontmatter() {
        let p = parse("just a body\n");
        assert_eq!(p.def, ForkDef::default());
        assert_eq!(p.body, "just a body\n");
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn defaults_with_empty_frontmatter() {
        let p = parse("---\ndescription: hi\n---\nbody");
        assert_eq!(p.def.description.as_deref(), Some("hi"));
        assert_eq!(p.def.run_on, default_run_on());
        assert_eq!(p.def.delivery, ForkDelivery::NextTurn);
        assert_eq!(p.body, "body");
    }

    #[test]
    fn full_config() {
        let p = parse(
            "---\n\
             description: d\n\
             run_on:\n  - idle: 10m\n  - compact\n  - session_end\n\
             delivery: discard\n\
             throttle: 30m\n\
             after: journal\n\
             model: haiku\n\
             ---\nbody",
        );
        assert_eq!(
            p.def.run_on,
            vec![
                ForkRunOn::Idle {
                    after_secs: Some(600)
                },
                ForkRunOn::Compact,
                ForkRunOn::SessionEnd
            ]
        );
        assert_eq!(p.def.delivery, ForkDelivery::Discard);
        assert_eq!(p.def.throttle_secs, Some(1800));
        assert_eq!(
            p.def.after,
            Some(ForkAfter {
                fork: "journal".into(),
                context: ForkAfterContext::Parent
            })
        );
        assert_eq!(p.def.model.as_deref(), Some("haiku"));
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn run_on_all_string_forms() {
        let p = parse(
            "---\nrun_on: [idle, compact, compaction, session_start, session_end, manual_stop, boot]\n---\n",
        );
        assert_eq!(
            p.def.run_on,
            vec![
                ForkRunOn::Idle { after_secs: None },
                ForkRunOn::Compact,
                ForkRunOn::Compact,
                ForkRunOn::SessionStart,
                ForkRunOn::SessionEnd,
                ForkRunOn::ManualStop,
                ForkRunOn::Boot,
            ]
        );
    }

    #[test]
    fn run_on_context_thresholds() {
        let p = parse(
            "---\nrun_on:\n  - context_tokens: 150000\n  - context_used: 80%\n  - context_left: 20000\n---\n",
        );
        assert_eq!(
            p.def.run_on,
            vec![
                ForkRunOn::ContextTokens(150000),
                ForkRunOn::ContextUsedPct(80),
                ForkRunOn::ContextLeft(20000),
            ]
        );
        assert_eq!(p.def.run_on[1].label(), "context_used:80%");
    }

    #[test]
    fn unknown_run_on_falls_back_to_defaults() {
        let p = parse("---\nrun_on: [flarp]\n---\n");
        assert_eq!(p.def.run_on, default_run_on());
        assert_eq!(p.warnings.len(), 2); // unknown trigger + empty fallback
    }

    #[test]
    fn partial_run_on_keeps_valid_entries() {
        let p = parse("---\nrun_on: [flarp, boot]\n---\n");
        assert_eq!(p.def.run_on, vec![ForkRunOn::Boot]);
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn immediate_delivery_downgrades_with_warning() {
        let p = parse("---\ndelivery: immediate\nthrottle: 1h\n---\n");
        assert_eq!(p.def.delivery, ForkDelivery::NextTurn);
        assert!(p.warnings.iter().any(|w| w.contains("immediate")));
    }

    #[test]
    fn unknown_delivery_falls_back() {
        let p = parse("---\ndelivery: pigeon\n---\n");
        assert_eq!(p.def.delivery, ForkDelivery::NextTurn);
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn invalid_throttle_ignored() {
        let p = parse("---\nthrottle: soon\n---\n");
        assert_eq!(p.def.throttle_secs, None);
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn after_map_forms() {
        let p = parse("---\nafter: {fork: journal, context: fork}\n---\n");
        assert_eq!(
            p.def.after,
            Some(ForkAfter {
                fork: "journal".into(),
                context: ForkAfterContext::Fork
            })
        );
        // `skill:` alias for definitions shared with other tools.
        let p = parse("---\nafter: {skill: journal}\n---\n");
        assert_eq!(
            p.def.after,
            Some(ForkAfter {
                fork: "journal".into(),
                context: ForkAfterContext::Parent
            })
        );
        let p = parse("---\nafter: {context: fork}\n---\n");
        assert_eq!(p.def.after, None);
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn after_self_reference_dropped() {
        let p = parse_fork_file("me", "---\nafter: me\n---\n").unwrap();
        assert_eq!(p.def.after, None);
        assert_eq!(p.warnings.len(), 1);
    }

    #[test]
    fn rag_keys_rejected_with_warning() {
        let p = parse("---\nrag:\n  triggers: [x]\n---\n");
        assert!(p.warnings.iter().any(|w| w.contains("RAG")));
        let p = parse("---\ntriggers: [x]\n---\n");
        assert!(p.warnings.iter().any(|w| w.contains("RAG")));
    }

    #[test]
    fn unknown_keys_ignored() {
        let p = parse("---\nfuture_key: whatever\ndescription: d\n---\nb");
        assert!(p.warnings.is_empty());
        assert_eq!(p.def.description.as_deref(), Some("d"));
    }

    #[test]
    fn invalid_yaml_drops_fork() {
        assert!(parse_fork_file("x", "---\n: [unclosed\n---\nb").is_none());
    }

    #[test]
    fn frontmatter_closed_at_eof() {
        let p = parse("---\ndescription: d\n---");
        assert_eq!(p.def.description.as_deref(), Some("d"));
        assert_eq!(p.body, "");
    }
}
