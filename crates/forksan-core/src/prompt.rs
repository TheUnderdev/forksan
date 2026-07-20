//! Fork prompt construction: what the throwaway forked session is asked to
//! do. The body is the fork's markdown content; framing tells the fork why
//! it fired and what happens to its final report.

use crate::frontmatter::ForkDelivery;
use crate::{truncate_chars, PREDECESSOR_MAX_CHARS};

/// Build the prompt for one fork run.
///
/// `trigger` is the matched `run_on` label (surfaced in the frontmatter so
/// the fork knows why it fired). `delivery` decides what the prompt says
/// about the fate of the final report. `predecessors` are the `(name,
/// report)` pairs of the `after` dependencies that finished before this fork
/// (empty when independent).
pub fn build_fork_prompt(
    name: &str,
    path: &str,
    body: &str,
    trigger: &str,
    delivery: ForkDelivery,
    predecessors: &[(String, String)],
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

    let predecessor_section = if predecessors.is_empty() {
        String::new()
    } else {
        let names = predecessors
            .iter()
            .map(|(dep, _)| format!("'{dep}'"))
            .collect::<Vec<_>>()
            .join(", ");
        let blocks = predecessors
            .iter()
            .map(|(dep, report)| {
                let report = truncate_chars(report, PREDECESSOR_MAX_CHARS);
                format!("<predecessor fork=\"{dep}\">\n{report}\n</predecessor>")
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        format!(
            "\n\nThis fork is sequenced after {names}, which just finished and \
            reported:\n\n{blocks}"
        )
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
            &[],
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
        let p = build_fork_prompt("x", "/x", "b", "compact", ForkDelivery::Discard, &[]);
        assert!(p.contains("discarded"));
        assert!(!p.contains("stored silently"));
    }

    #[test]
    fn predecessor_blocks_present_and_capped() {
        let long = "r".repeat(PREDECESSOR_MAX_CHARS + 100);
        let preds = vec![
            ("a".to_string(), long),
            ("z".to_string(), "z report".to_string()),
        ];
        let p = build_fork_prompt("b", "/b", "body", "idle", ForkDelivery::NextTurn, &preds);
        assert!(p.contains("sequenced after 'a', 'z'"));
        assert!(p.contains("<predecessor fork=\"a\">"));
        assert!(p.contains("<predecessor fork=\"z\">\nz report"));
        assert!(p.contains("…(truncated)"));
    }
}
