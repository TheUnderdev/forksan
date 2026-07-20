//! Wake-payload construction: the text a due Stop hook prints to stderr and
//! exits 2 with, so Claude Code wakes the idle session and shows it as a
//! system reminder. The woken model reads the payload and spawns the due
//! forks as background `fork` subagents (Agent tool, `subagent_type: "fork"`).
//!
//! The parent model is told to spawn each fork with a prompt that makes the
//! *fork* read the fork file — the parent must never read it itself.

/// The greppable marker every wake block carries (`source: forksan`). The
/// payload builder emits it and the continuation sniffer anchors on it, so the
/// two can never drift. It survives Claude Code wrapping the payload in a
/// system-reminder / task-notification envelope because it appears verbatim
/// inside the reminder text.
pub const WAKE_MARKER: &str = "source: forksan";

/// The prefix Claude Code uses for a fork-completion (task) notification the
/// session receives as a non-waking continuation.
pub const TASK_NOTIFICATION_PREFIX: &str = "<task-notification>";

/// Whether a submitted "prompt" is actually a non-waking continuation — an
/// asyncRewake wake reminder (carries [`WAKE_MARKER`]) or a fork-completion
/// task notification (starts with [`TASK_NOTIFICATION_PREFIX`]) — rather than
/// genuine user input. Used to decide whether a UserPromptSubmit advances the
/// pause epoch.
pub fn looks_like_continuation(prompt: &str) -> bool {
    let trimmed = prompt.trim_start();
    trimmed.starts_with(TASK_NOTIFICATION_PREFIX) || prompt.contains(WAKE_MARKER)
}

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

fn spawn_prompt(
    fork: &DueFork,
    session_id: &str,
    conversation_id: &str,
    project_root: &str,
) -> String {
    format!(
        "Read the file {path} and follow the instructions in its body. Context for this \
         run: fork '{name}', trigger '{trigger}', parent session {session_id}, conversation \
         {conversation_id}, project root {project_root}. The conversation id is stable when \
         a session is resumed (a resumed session gets a fresh session id); key any \
         per-conversation artifacts on it. Your final message is your report.",
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

fn root_block(
    fork: &DueFork,
    session_id: &str,
    conversation_id: &str,
    project_root: &str,
) -> String {
    format!(
        "---\nsource: forksan\ndue: {name} (trigger: {trigger})\n---\n\
         Spawn a background fork subagent now: use the Agent tool with subagent_type \"fork\" \
         and this prompt: \"{prompt}\" Do not read that file yourself — only the fork reads \
         it.{overlap}",
        name = fork.name,
        trigger = fork.trigger,
        prompt = spawn_prompt(fork, session_id, conversation_id, project_root),
        overlap = overlap_line(fork),
    )
}

fn dependent_block(
    fork: &DueFork,
    session_id: &str,
    conversation_id: &str,
    project_root: &str,
) -> String {
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
        prompt = spawn_prompt(fork, session_id, conversation_id, project_root),
        overlap = overlap_line(fork),
    )
}

/// Build the full wake payload for a set of due forks. Roots (no `after`
/// within the set) come first as immediate spawn blocks; dependents follow as
/// deferred spawn instructions. A trailing line asks the model to acknowledge
/// the background work so the harness doesn't nudge about missing output.
///
/// `conversation_id` is the identity that survives session resume (the
/// transcript file stem — resumed legs get a fresh session id but append to
/// the original transcript). Forks keying persistent artifacts should use it
/// over the session id.
pub fn build_wake_payload(
    session_id: &str,
    conversation_id: &str,
    project_root: &str,
    forks: &[DueFork],
) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for fork in forks.iter().filter(|f| f.after.is_empty()) {
        blocks.push(root_block(fork, session_id, conversation_id, project_root));
    }
    for fork in forks.iter().filter(|f| !f.after.is_empty()) {
        blocks.push(dependent_block(
            fork,
            session_id,
            conversation_id,
            project_root,
        ));
    }

    let closer = if blocks.len() == 1 {
        "After spawning the fork above, reply with one short line acknowledging the \
         background work and stop."
    } else {
        "After spawning all forks above, reply with one short line acknowledging the \
         background work and stop."
    };
    format!("{}\n\n{closer}{CONTINGENCY}", blocks.join("\n\n"))
}

/// Rides in every wake to recover from Claude Code's dynamic agent disclosure:
/// on current builds the `fork` type may not be in the Agent tool's disclosed
/// roster for a hook-injected turn (disclosure keys off the user's own prompt),
/// so the first call can fail with "Agent type 'fork' not found". Retrying once
/// in the same turn is cheap; if it still fails, the spawn instructions above
/// persist in context and the user's next real message discloses the type, so
/// we defer rather than substitute a (context-less) agent that can't do a
/// fork's job.
const CONTINGENCY: &str = " If the Agent tool reports \"Agent type 'fork' not found\" (or that \
    the 'fork' agent type is unavailable), first retry the exact same Agent call once. If it \
    still fails, do NOT substitute another agent type (other agents don't inherit this \
    conversation and cannot do a fork's job). Never create, install, or edit any agent \
    definition (e.g. a file named fork.md under .claude/agents/) to work around a missing \
    fork type — a custom agent cannot inherit this conversation and cannot do a fork's job, \
    and a custom agent named 'fork' shadows the real built-in type. Instead reply with one \
    line telling the user the fork agent type isn't loaded in this turn and that sending any \
    next message will let the forks spawn. When the user next messages, spawn the due forks \
    listed above before doing anything else.";

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
    fn payload_carries_the_sniffer_marker() {
        let p = build_wake_payload("s", "conv-s", "/p", &[due("j", &[], false)]);
        assert!(
            p.contains(WAKE_MARKER),
            "payload must carry the wake marker"
        );
        assert!(
            looks_like_continuation(&p),
            "the builder's own output must sniff as a continuation"
        );
    }

    #[test]
    fn continuation_sniffer() {
        assert!(looks_like_continuation(
            "<task-notification>fork 'j' finished</task-notification>"
        ));
        assert!(looks_like_continuation("  \n<task-notification>x"));
        assert!(looks_like_continuation(
            "---\nsource: forksan\ndue: journal (trigger: idle)\n---\nSpawn a fork"
        ));
        assert!(!looks_like_continuation(
            "please refactor the config parser"
        ));
        assert!(!looks_like_continuation(
            "what does source mean in forksan?"
        ));
    }

    #[test]
    fn single_root_block() {
        let p = build_wake_payload("sid-1", "conv-1", "/proj", &[due("journal", &[], false)]);
        assert!(p.contains("---\nsource: forksan\ndue: journal (trigger: idle)\n---\n"));
        assert!(p.contains("subagent_type \"fork\""));
        assert!(p.contains("Read the file /x/journal.md"));
        assert!(p.contains("parent session sid-1"));
        assert!(p.contains("conversation conv-1"));
        assert!(p.contains("key any per-conversation artifacts on it"));
        assert!(p.contains("project root /proj"));
        assert!(p.contains("Do not read that file yourself"));
        // overlap:false → skip-if-running line present.
        assert!(p.contains("skip spawning it"));
        assert!(p.contains("After spawning the fork above"));
        // Contingency v2: retry once, then defer to the next user message.
        assert!(p.contains("Agent type 'fork' not found"));
        assert!(p.contains("retry the exact same Agent call once"));
        assert!(p.contains("sending any next message will let the forks spawn"));
        assert!(p.contains("spawn the due forks listed above before doing anything else"));
        // Never substitute another agent type, and never fabricate an impostor.
        assert!(p.contains("do NOT substitute another agent type"));
        assert!(p.contains("Never create, install, or edit any agent definition"));
        assert!(p.contains(".claude/agents/"));
        assert!(p.contains("shadows the real built-in type"));
    }

    #[test]
    fn overlap_true_omits_skip_line() {
        let p = build_wake_payload("s", "conv-s", "/p", &[due("j", &[], true)]);
        assert!(!p.contains("skip spawning it"));
    }

    #[test]
    fn multiple_forks_get_plural_closer() {
        let p = build_wake_payload(
            "s",
            "conv-s",
            "/p",
            &[due("a", &[], false), due("b", &[], false)],
        );
        assert!(p.contains("due: a (trigger: idle)"));
        assert!(p.contains("due: b (trigger: idle)"));
        assert!(p.contains("After spawning all forks above"));
    }

    #[test]
    fn dependents_are_deferred_and_reference_predecessors() {
        let p = build_wake_payload(
            "s",
            "conv-s",
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
