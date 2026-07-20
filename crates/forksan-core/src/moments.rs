//! Fork moments and trigger matching.
//!
//! Since v0.5 the only moments that fire are idle deadlines and context
//! thresholds — the wake-and-spawn model has no compact/session/boot hooks.
//! Unsupported `run_on` triggers simply never match any moment.

use crate::frontmatter::{ForkDef, ForkRunOn};

/// The context window assumed for `context_used` / `context_left` when the
/// model's real window is unknown. (v0.5 dropped the configurable window; the
/// fork inherits the session's model, whose window we approximate here.)
pub const DEFAULT_CONTEXT_WINDOW: u64 = 200_000;

/// The 1M context window Claude Code marks with a `[1m]` suffix on the
/// session's model id (e.g. `claude-opus-4-8[1m]`).
pub const CONTEXT_WINDOW_1M: u64 = 1_000_000;

/// The context window for a session, resolved from the model id Claude Code
/// reports in hook input and the observed gauge. The hook-side model string
/// keeps the `[1m]` marker (the transcript's `message.model` strips it), so
/// marked sessions get the 1M window. Fable/Mythos-family models are 1M
/// unconditionally — their window has no 200k variant, so the bare id is
/// enough. A gauge that already exceeds the resolved window proves it wrong —
/// the window bumps to the 1M tier (belt for sessions whose events never
/// carried a model), and beyond that to the gauge itself so `context_used`
/// saturates at 100% instead of overshooting.
pub fn resolve_context_window(model: Option<&str>, prompt_tokens: Option<u64>) -> u64 {
    let mut window = match model {
        Some(m) if m.contains("[1m]") || m.contains("fable") || m.contains("mythos") => {
            CONTEXT_WINDOW_1M
        }
        _ => DEFAULT_CONTEXT_WINDOW,
    };
    if let Some(pt) = prompt_tokens {
        if pt > window {
            window = CONTEXT_WINDOW_1M.max(pt);
        }
    }
    window
}

/// A fork moment: an event at which rostered forks may fire (matched against
/// each fork's `run_on` config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkMoment {
    /// The session has been idle for exactly this deadline (seconds).
    Idle { deadline_secs: u64 },
    /// End-of-turn context gauge (prompt tokens of the last turn plus the
    /// model's context window, if known).
    Context {
        prompt_tokens: u64,
        max_tokens: Option<u64>,
    },
}

/// The first `run_on` trigger of `fork` matched by any of `moments`.
/// Forks without an explicit `after_secs` on their idle trigger fire at the
/// default idle deadline (`default_idle_secs`).
pub fn match_moments(
    fork: &ForkDef,
    moments: &[ForkMoment],
    default_idle_secs: u64,
) -> Option<ForkRunOn> {
    for moment in moments {
        for trigger in &fork.run_on {
            let hit = match (moment, trigger) {
                (ForkMoment::Idle { deadline_secs }, ForkRunOn::Idle { after_secs }) => {
                    let effective = after_secs.unwrap_or(default_idle_secs);
                    effective > 0 && effective == *deadline_secs
                }
                (ForkMoment::Context { prompt_tokens, .. }, ForkRunOn::ContextTokens(n)) => {
                    prompt_tokens >= n
                }
                (
                    ForkMoment::Context {
                        prompt_tokens,
                        max_tokens,
                    },
                    ForkRunOn::ContextUsedPct(p),
                ) => max_tokens.is_some_and(|max| {
                    prompt_tokens.saturating_mul(100) >= max.saturating_mul(*p as u64)
                }),
                (
                    ForkMoment::Context {
                        prompt_tokens,
                        max_tokens,
                    },
                    ForkRunOn::ContextLeft(n),
                ) => max_tokens.is_some_and(|max| max.saturating_sub(*prompt_tokens) <= *n),
                _ => false,
            };
            if hit {
                return Some(*trigger);
            }
        }
    }
    None
}

/// The distinct idle deadlines (seconds, ascending) the given forks need
/// serviced: the default deadline when any fork uses a bare `idle`, plus
/// every explicit `idle: <dur>`. Zero-duration deadlines are dropped.
pub fn idle_deadlines<'a>(
    forks: impl Iterator<Item = &'a ForkDef>,
    default_idle_secs: u64,
) -> Vec<u64> {
    let mut out: Vec<u64> = Vec::new();
    for fork in forks {
        for trigger in &fork.run_on {
            if let ForkRunOn::Idle { after_secs } = trigger {
                let d = after_secs.unwrap_or(default_idle_secs);
                if d > 0 && !out.contains(&d) {
                    out.push(d);
                }
            }
        }
    }
    out.sort_unstable();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::{default_run_on, ForkDef, ForkRunOn};

    fn fork(run_on: Vec<ForkRunOn>) -> ForkDef {
        ForkDef {
            run_on,
            ..ForkDef::default()
        }
    }

    #[test]
    fn default_run_on_matches_default_idle() {
        let f = fork(default_run_on());
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 240 }], 240),
            Some(ForkRunOn::Idle { after_secs: None })
        );
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 600 }], 240),
            None
        );
    }

    #[test]
    fn explicit_idle_deadline_is_exclusive() {
        let f = fork(vec![ForkRunOn::Idle {
            after_secs: Some(1200),
        }]);
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Idle {
                    deadline_secs: 1200
                }],
                240
            ),
            Some(ForkRunOn::Idle {
                after_secs: Some(1200)
            })
        );
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 240 }], 240),
            None
        );
    }

    #[test]
    fn zero_default_idle_never_fires() {
        let f = fork(vec![ForkRunOn::Idle { after_secs: None }]);
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 0 }], 0),
            None
        );
    }

    #[test]
    fn unsupported_triggers_never_match() {
        let f = fork(vec![
            ForkRunOn::Compact,
            ForkRunOn::SessionEnd,
            ForkRunOn::Boot,
        ]);
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 240 }], 240),
            None
        );
        assert_eq!(
            match_moments(
                &f,
                &[ForkMoment::Context {
                    prompt_tokens: 999_999,
                    max_tokens: Some(200_000)
                }],
                240
            ),
            None
        );
    }

    #[test]
    fn context_thresholds() {
        let tokens = fork(vec![ForkRunOn::ContextTokens(100_000)]);
        let used = fork(vec![ForkRunOn::ContextUsedPct(80)]);
        let left = fork(vec![ForkRunOn::ContextLeft(50_000)]);

        let low = [ForkMoment::Context {
            prompt_tokens: 90_000,
            max_tokens: Some(200_000),
        }];
        let high = [ForkMoment::Context {
            prompt_tokens: 170_000,
            max_tokens: Some(200_000),
        }];
        let no_max = [ForkMoment::Context {
            prompt_tokens: 170_000,
            max_tokens: None,
        }];

        assert_eq!(match_moments(&tokens, &low, 240), None);
        assert_eq!(
            match_moments(&tokens, &high, 240),
            Some(ForkRunOn::ContextTokens(100_000))
        );
        assert_eq!(
            match_moments(&tokens, &no_max, 240),
            Some(ForkRunOn::ContextTokens(100_000))
        );

        assert_eq!(match_moments(&used, &low, 240), None);
        assert_eq!(
            match_moments(&used, &high, 240),
            Some(ForkRunOn::ContextUsedPct(80))
        );
        assert_eq!(match_moments(&used, &no_max, 240), None);

        assert_eq!(match_moments(&left, &low, 240), None);
        assert_eq!(
            match_moments(&left, &high, 240),
            Some(ForkRunOn::ContextLeft(50_000))
        );
        assert_eq!(match_moments(&left, &no_max, 240), None);
    }

    #[test]
    fn window_resolution() {
        // No model, small gauge: the default window.
        assert_eq!(resolve_context_window(None, Some(90_000)), 200_000);
        assert_eq!(
            resolve_context_window(Some("claude-opus-4-8"), None),
            200_000
        );
        // The [1m] marker selects the 1M window regardless of gauge.
        assert_eq!(
            resolve_context_window(Some("claude-opus-4-8[1m]"), Some(150_000)),
            1_000_000
        );
        // Fable/Mythos models are always 1M — no marker needed.
        assert_eq!(
            resolve_context_window(Some("claude-fable-5"), Some(50_000)),
            1_000_000
        );
        assert_eq!(
            resolve_context_window(Some("claude-mythos-5"), None),
            1_000_000
        );
        // A gauge over the assumed window bumps to the 1M tier (model unknown
        // or unmarked), and past 1M the gauge itself becomes the window.
        assert_eq!(resolve_context_window(None, Some(397_929)), 1_000_000);
        assert_eq!(
            resolve_context_window(Some("claude-opus-4-8"), Some(250_000)),
            1_000_000
        );
        assert_eq!(
            resolve_context_window(Some("m[1m]"), Some(1_200_000)),
            1_200_000
        );
    }

    #[test]
    fn used_pct_respects_1m_window() {
        // The exact regression: 75% of a 1M session must not fire at 150k.
        let used = fork(vec![ForkRunOn::ContextUsedPct(75)]);
        let at_150k = [ForkMoment::Context {
            prompt_tokens: 150_000,
            max_tokens: Some(resolve_context_window(
                Some("claude-opus-4-8[1m]"),
                Some(150_000),
            )),
        }];
        assert_eq!(match_moments(&used, &at_150k, 240), None);
        let at_800k = [ForkMoment::Context {
            prompt_tokens: 800_000,
            max_tokens: Some(resolve_context_window(
                Some("claude-opus-4-8[1m]"),
                Some(800_000),
            )),
        }];
        assert_eq!(
            match_moments(&used, &at_800k, 240),
            Some(ForkRunOn::ContextUsedPct(75))
        );
    }

    #[test]
    fn idle_deadline_collection() {
        let forks = [
            fork(vec![ForkRunOn::Idle { after_secs: None }]),
            fork(vec![ForkRunOn::Idle {
                after_secs: Some(1200),
            }]),
            fork(vec![
                ForkRunOn::Idle {
                    after_secs: Some(600),
                },
                ForkRunOn::Compact,
            ]),
            fork(vec![ForkRunOn::Idle {
                after_secs: Some(600),
            }]),
            fork(vec![ForkRunOn::Compact]),
        ];
        assert_eq!(idle_deadlines(forks.iter(), 240), vec![240, 600, 1200]);
        assert_eq!(idle_deadlines(forks.iter(), 0), vec![600, 1200]);
    }
}
