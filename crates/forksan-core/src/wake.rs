//! Wake-payload construction: the text a due Stop hook prints to stderr and
//! exits 2 with, so Claude Code wakes the idle session and shows it as a
//! system reminder. The woken model reads the payload and spawns the due
//! forks as background `fork` subagents (Agent tool, `subagent_type: "fork"`).
//!
//! The parent model is told to spawn each fork with a prompt that makes the
//! *fork* read the fork file — the parent must never read it itself.

/// One fork that is due to fire, as the payload builder sees it.
#[derive(Debug, Clone)]
pub struct DueFork {
    /// The fork's name.
    pub name: String,
    /// Absolute path to the fork's `.md` definition (the fork reads this).
    pub path: String,
    /// The matched `run_on` trigger label (e.g. `idle`, `context_used:80%`).
    pub trigger: String,
    /// Whether concurrent runs are allowed (`overlap: true`). When false, the
    /// wake block tells the model to skip if a previous run is still active.
    pub overlap: bool,
    /// Predecessor fork names (within this due set) this fork runs `after`.
    pub after: Vec<String>,
}

fn spawn_prompt(fork: &DueFork, session_id: &str, project_root: &str) -> String {
    format!(
        "Read the file {path} and follow the instructions in its body. Context for this \
         run: fork '{name}', trigger '{trigger}', parent session {session_id}, project root \
         {project_root}. Your final message is your report.",
        path = fork.path,
        name = fork.name,
        trigger = fork.trigger,
    )
}

fn overlap_line(fork: &DueFork) -> &'static str {
    if fork.overlap {
        ""
    } else {
        " If a previous run of this fork is still among your running background tasks, skip \
         spawning it."
    }
}

fn root_block(fork: &DueFork, session_id: &str, project_root: &str) -> String {
    format!(
        "---\nsource: forksan\ndue: {name} (trigger: {trigger})\n---\n\
         Spawn a background fork subagent now: use the Agent tool with subagent_type \"fork\" \
         and this prompt: \"{prompt}\" Do not read that file yourself — only the fork reads \
         it.{overlap}",
        name = fork.name,
        trigger = fork.trigger,
        prompt = spawn_prompt(fork, session_id, project_root),
        overlap = overlap_line(fork),
    )
}

fn dependent_block(fork: &DueFork, session_id: &str, project_root: &str) -> String {
    let preds = fork
        .after
        .iter()
        .map(|p| format!("'{p}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "---\nsource: forksan\ndue: {name} (trigger: {trigger}) — after {preds}\n---\n\
         Do not spawn fork '{name}' yet. After {preds} finish and you receive their \
         completion notifications, spawn a background fork subagent: use the Agent tool with \
         subagent_type \"fork\" and this prompt: \"{prompt} This fork runs after {preds}; \
         include the report(s) they returned in their completion notifications so the fork \
         can build on them.\" Do not read that file yourself — only the fork reads \
         it.{overlap}",
        name = fork.name,
        trigger = fork.trigger,
        prompt = spawn_prompt(fork, session_id, project_root),
        overlap = overlap_line(fork),
    )
}

/// Build the full wake payload for a set of due forks. Roots (no `after`
/// within the set) come first as immediate spawn blocks; dependents follow as
/// deferred spawn instructions. A trailing line asks the model to acknowledge
/// the background work so the harness doesn't nudge about missing output.
pub fn build_wake_payload(session_id: &str, project_root: &str, forks: &[DueFork]) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for fork in forks.iter().filter(|f| f.after.is_empty()) {
        blocks.push(root_block(fork, session_id, project_root));
    }
    for fork in forks.iter().filter(|f| !f.after.is_empty()) {
        blocks.push(dependent_block(fork, session_id, project_root));
    }

    let closer = if blocks.len() == 1 {
        "After spawning the fork above, reply with one short line acknowledging the \
         background work and stop."
    } else {
        "After spawning all forks above, reply with one short line acknowledging the \
         background work and stop."
    };
    format!("{}\n\n{closer}", blocks.join("\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn due(name: &str, after: &[&str], overlap: bool) -> DueFork {
        DueFork {
            name: name.to_string(),
            path: format!("/x/{name}.md"),
            trigger: "idle".to_string(),
            overlap,
            after: after.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[test]
    fn single_root_block() {
        let p = build_wake_payload("sid-1", "/proj", &[due("journal", &[], false)]);
        assert!(p.contains("---\nsource: forksan\ndue: journal (trigger: idle)\n---\n"));
        assert!(p.contains("subagent_type \"fork\""));
        assert!(p.contains("Read the file /x/journal.md"));
        assert!(p.contains("parent session sid-1"));
        assert!(p.contains("project root /proj"));
        assert!(p.contains("Do not read that file yourself"));
        // overlap:false → skip-if-running line present.
        assert!(p.contains("skip spawning it"));
        assert!(p.contains("After spawning the fork above"));
    }

    #[test]
    fn overlap_true_omits_skip_line() {
        let p = build_wake_payload("s", "/p", &[due("j", &[], true)]);
        assert!(!p.contains("skip spawning it"));
    }

    #[test]
    fn multiple_forks_get_plural_closer() {
        let p = build_wake_payload("s", "/p", &[due("a", &[], false), due("b", &[], false)]);
        assert!(p.contains("due: a (trigger: idle)"));
        assert!(p.contains("due: b (trigger: idle)"));
        assert!(p.contains("After spawning all forks above"));
    }

    #[test]
    fn dependents_are_deferred_and_reference_predecessors() {
        let p = build_wake_payload(
            "s",
            "/p",
            &[due("alpha", &[], false), due("beta", &["alpha"], false)],
        );
        // Root block for alpha comes first.
        let alpha_pos = p.find("Spawn a background fork subagent now").unwrap();
        let beta_pos = p.find("Do not spawn fork 'beta' yet").unwrap();
        assert!(alpha_pos < beta_pos, "root must precede dependent");
        assert!(p.contains("After 'alpha' finish"));
        assert!(p.contains("include the report(s) they returned"));
    }
}
