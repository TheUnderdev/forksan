//! Fork moments and trigger matching.

use crate::frontmatter::{ForkDef, ForkRunOn};

/// A fork moment: an event in the session lifecycle at which rostered forks
/// may fire (matched against each fork's `run_on` config).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForkMoment {
    /// The session has been idle for exactly this deadline (seconds).
    Idle { deadline_secs: u64 },
    /// Context compaction (auto or manual).
    Compact,
    /// A new session just started.
    SessionStart,
    /// The session is being closed. `manual` = closed while recently active
    /// (idle time below the default idle deadline), i.e. stopped rather than
    /// timed out.
    SessionEnd { manual: bool },
    /// Daemon startup found this session still tracked.
    Boot,
    /// End-of-turn context gauge (prompt tokens of the last turn plus the
    /// model's configured context window, if known).
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
                (ForkMoment::Compact, ForkRunOn::Compact) => true,
                (ForkMoment::SessionStart, ForkRunOn::SessionStart) => true,
                (ForkMoment::SessionEnd { .. }, ForkRunOn::SessionEnd) => true,
                (ForkMoment::SessionEnd { manual: true }, ForkRunOn::ManualStop) => true,
                (ForkMoment::Boot, ForkRunOn::Boot) => true,
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
    fn default_run_on_matches_default_idle_and_compact() {
        let f = fork(default_run_on());
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 240 }], 240),
            Some(ForkRunOn::Idle { after_secs: None })
        );
        assert_eq!(
            match_moments(&f, &[ForkMoment::Idle { deadline_secs: 600 }], 240),
            None
        );
        assert_eq!(
            match_moments(&f, &[ForkMoment::Compact], 240),
            Some(ForkRunOn::Compact)
        );
        assert_eq!(match_moments(&f, &[ForkMoment::Boot], 240), None);
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
    fn manual_stop_vs_session_end() {
        let stop = fork(vec![ForkRunOn::ManualStop]);
        let end = fork(vec![ForkRunOn::SessionEnd]);
        let manual = [ForkMoment::SessionEnd { manual: true }];
        let timeout = [ForkMoment::SessionEnd { manual: false }];
        assert_eq!(
            match_moments(&stop, &manual, 240),
            Some(ForkRunOn::ManualStop)
        );
        assert_eq!(match_moments(&stop, &timeout, 240), None);
        assert_eq!(
            match_moments(&end, &manual, 240),
            Some(ForkRunOn::SessionEnd)
        );
        assert_eq!(
            match_moments(&end, &timeout, 240),
            Some(ForkRunOn::SessionEnd)
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
        // Absolute threshold still fires without a known window.
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
    fn first_matching_trigger_wins() {
        let f = fork(vec![ForkRunOn::SessionEnd, ForkRunOn::ManualStop]);
        assert_eq!(
            match_moments(&f, &[ForkMoment::SessionEnd { manual: true }], 240),
            Some(ForkRunOn::SessionEnd)
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
