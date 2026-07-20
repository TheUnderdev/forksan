//! Per-fork tag filtering.
//!
//! Forks may carry `tags`; a session (via its per-session env filter) or the
//! config may narrow which forks run through an enable (whitelist) set and a
//! disable (blocklist) set. The predicate is pure so the whole rule set is
//! unit-testable without a daemon.

/// Decide whether a fork with `fork_tags` passes the enable/disable filter.
///
/// Rules, applied in order:
/// 1. If any fork tag is in `disable`, the fork is excluded (disable wins).
/// 2. If `enable` is present and non-empty, the fork runs only when at least
///    one of its tags is in it — so untagged forks are excluded by a
///    whitelist.
/// 3. Otherwise the fork runs (no filter configured → everything runs).
pub fn tags_allowed(
    fork_tags: &[String],
    enable: Option<&[String]>,
    disable: Option<&[String]>,
) -> bool {
    if let Some(disable) = disable {
        if fork_tags.iter().any(|t| disable.iter().any(|d| d == t)) {
            return false;
        }
    }
    match enable {
        Some(enable) if !enable.is_empty() => {
            fork_tags.iter().any(|t| enable.iter().any(|e| e == t))
        }
        _ => true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn no_filter_runs_everything() {
        assert!(tags_allowed(&v(&["ci"]), None, None));
        assert!(tags_allowed(&[], None, None));
    }

    #[test]
    fn disable_hit_excludes() {
        assert!(!tags_allowed(
            &v(&["ci", "review"]),
            None,
            Some(&v(&["ci"]))
        ));
    }

    #[test]
    fn disable_miss_runs() {
        assert!(tags_allowed(&v(&["review"]), None, Some(&v(&["ci"]))));
    }

    #[test]
    fn enable_hit_runs() {
        assert!(tags_allowed(&v(&["ci", "docs"]), Some(&v(&["ci"])), None));
    }

    #[test]
    fn enable_miss_excludes() {
        assert!(!tags_allowed(&v(&["docs"]), Some(&v(&["ci"])), None));
    }

    #[test]
    fn untagged_excluded_by_whitelist() {
        assert!(!tags_allowed(&[], Some(&v(&["ci"])), None));
    }

    #[test]
    fn disable_beats_enable() {
        // Tag is on both lists: disable wins.
        assert!(!tags_allowed(
            &v(&["ci"]),
            Some(&v(&["ci"])),
            Some(&v(&["ci"]))
        ));
    }

    #[test]
    fn empty_enable_is_no_whitelist() {
        // An empty enable set must not exclude untagged forks.
        assert!(tags_allowed(&[], Some(&[]), None));
        assert!(tags_allowed(&v(&["ci"]), Some(&[]), None));
    }
}
