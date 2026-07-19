//! Fork prompt construction: what the throwaway forked session is asked to
//! do. The body is the fork's markdown content; framing tells the fork why
//! it fired and what happens to its final report.

use crate::frontmatter::ForkDelivery;
use crate::{truncate_chars, PREDECESSOR_MAX_CHARS};

/// Build the prompt for one fork run.
///
/// `trigger` is the matched `run_on` label (surfaced in the frontmatter so
/// the fork knows why it fired). `delivery` decides what the prompt says
/// about the fate of the final report. `predecessor` is `Some((name,
/// report))` when this fork is sequenced `after` another fork on the parent
/// context.
pub fn build_fork_prompt(
    name: &str,
    path: &str,
    body: &str,
    trigger: &str,
    delivery: ForkDelivery,
    predecessor: Option<(&str, &str)>,
) -> String {
    let fate = match delivery {
        ForkDelivery::Discard => {
            "Your final text response will be discarded and shown to no one — do all the \
            work with tools."
        }
        ForkDelivery::NextTurn => {
            "Your final text response will be stored silently as a report of what this \
            fork did and injected as context into the parent session's next turn — the \
            user never sees it directly. Do the actual work with tools, then end with a \
            concise report the parent agent will find useful."
        }
    };

    let predecessor_section = match predecessor {
        Some((dep, report)) => {
            let report = truncate_chars(report, PREDECESSOR_MAX_CHARS);
            format!(
                "\n\nThis fork is sequenced after the fork '{dep}', which just finished \
                and reported:\n\n<predecessor fork=\"{dep}\">\n{report}\n</predecessor>"
            )
        }
        None => String::new(),
    };

    format!(
        "---\nsource: forksan\nfork: {name}\ntrigger: {trigger}\n---\n\
        **Fork: {name}** (trigger: {trigger}). You are running in a throwaway fork of \
        the parent session — you have its full context above, but nothing you say here \
        reaches the user. This fork exists only to execute the instructions below, which \
        were silently triggered during the session. {fate} Do not try to message the \
        user. The fork may have run earlier in this same session; treat its instructions \
        idempotently.{predecessor_section}\n\n\
        <fork name=\"{name}\" source=\"{path}\">\n{body}\n</fork>"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_turn_fate_and_frontmatter() {
        let p = build_fork_prompt(
            "journal",
            "/x/journal.md",
            "write the journal",
            "idle:600",
            ForkDelivery::NextTurn,
            None,
        );
        assert!(p.starts_with("---\nsource: forksan\nfork: journal\ntrigger: idle:600\n---\n"));
        assert!(p.contains("stored silently"));
        assert!(p.contains("<fork name=\"journal\" source=\"/x/journal.md\">"));
        assert!(p.contains("write the journal"));
        assert!(p.contains("idempotently"));
        assert!(!p.contains("<predecessor"));
    }

    #[test]
    fn discard_fate() {
        let p = build_fork_prompt("x", "/x", "b", "compact", ForkDelivery::Discard, None);
        assert!(p.contains("discarded"));
        assert!(!p.contains("stored silently"));
    }

    #[test]
    fn predecessor_block_present_and_capped() {
        let long = "r".repeat(PREDECESSOR_MAX_CHARS + 100);
        let p = build_fork_prompt(
            "b",
            "/b",
            "body",
            "idle",
            ForkDelivery::NextTurn,
            Some(("a", &long)),
        );
        assert!(p.contains("<predecessor fork=\"a\">"));
        assert!(p.contains("…(truncated)"));
    }
}
