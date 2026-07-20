//! forksan-core: the tool-agnostic semantics of forksan.
//!
//! Everything in this crate is synchronous and side-effect-light so the full
//! semantic surface (frontmatter parsing, moment matching, dependency
//! layering, prompt building, store invariants) is unit-testable without a
//! daemon or a Claude Code installation.

pub mod config;
pub mod discovery;
pub mod duration;
pub mod frontmatter;
pub mod moments;
pub mod project;
pub mod prompt;
pub mod protocol;
pub mod schedule;
pub mod store;
pub mod tags;

/// Wire/CLI/daemon protocol version. Bump only on breaking frame changes;
/// the `shutdown` frame shape is frozen forever at proto 1.
pub const PROTO_VERSION: u32 = 1;

/// Fork reports delivered to the parent are capped at this many characters.
pub const REPORT_MAX_CHARS: usize = 8_000;

/// A predecessor report piped into a successor's prompt is capped at this.
pub const PREDECESSOR_MAX_CHARS: usize = 4_000;

/// Truncate a string to at most `max` characters (not bytes), appending a
/// marker when truncation happened.
pub fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let cut: String = s.chars().take(max).collect();
    format!("{cut}…(truncated)")
}

#[cfg(test)]
mod tests {
    use super::truncate_chars;

    #[test]
    fn truncate_is_char_aware() {
        assert_eq!(truncate_chars("héllo", 10), "héllo");
        let t = truncate_chars("héllo wörld", 5);
        assert!(t.starts_with("héllo"));
        assert!(t.ends_with("…(truncated)"));
    }
}
