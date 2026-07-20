# forksan

**Forks for Claude Code.** At the moments your session naturally pauses — going idle, compacting
context, starting or ending — forksan runs *forks*: throwaway, forked-context prompts that
inherit everything your session knows, do background work with tools, and report back into your
session's context on its next turn.

Think of a fork as a background thought your agent has while you're away: update the project
journal, distill notes, groom a TODO list, re-check an assumption — without interrupting the
conversation, and with the fork's report waiting as context when you come back.

> forks are **not** skills. A skill is something the model chooses to load and follow. A fork is
> something the *harness* runs on the model's behalf at lifecycle moments the model never sees.
> Forks are never announced to the model and there is no retrieval/RAG involved — a fork fires
> because its `run_on` moment happened, full stop.

## Install

From the plugin marketplace:

```
/plugin marketplace add TheUnderdev/forksan
/plugin install forksan@forksan
```

On first use a bootstrap step downloads the prebuilt binary for your platform from GitHub
Releases into the plugin's persistent data directory (or builds it with `cargo` if no artifact
matches). macOS (arm64/x64) and Linux (x64/arm64) are covered.

For local development: `claude --plugin-dir ./plugin` inside this repo.

## Writing forks

Forks live in `.forksan/forks/`, discovered upward from your project directory plus the
user-level `~/.forksan/forks/`. Two layouts, mix freely (subfolders are just organization):

```
.forksan/forks/
├── journal.md              # a fork named "journal"
├── maintenance/
│   └── groom-todos.md      # a fork named "groom-todos"
└── deep-review/
    └── FORK.md             # a fork named "deep-review"
```

A fork is a markdown file: YAML frontmatter for *when and how*, body for *what to do*.

```markdown
---
description: Keep NOTES.md current with what happened this session
run_on:
  - idle: 15m
  - session_end
throttle: 30m
---
Review the session so far and update NOTES.md with any durable decisions,
open questions, and next steps. Keep it under 200 lines.
```

### Frontmatter reference

| Key | Values | Default |
|---|---|---|
| `description` | free text, for humans (`forksan forks`) | — |
| `run_on` | list of moments, see below | `[idle, compact]` |
| `delivery` | `next_turn` \| `discard` | `next_turn` |
| `throttle` | min gap between runs: `30m`, `2h`, `90` (seconds) | none |
| `after` | fork name(s): `journal`, `[a, b]`, or maps `{fork: name, context: parent\|fork}` | — |
| `overlap` | `true` to allow two runs of this fork at once | `false` |
| `model` | model override for the fork run | session default |

Moments for `run_on`:

- `idle` — the session has been quiet for the default idle deadline (config, 10m)
- `idle: 20m` — a custom idle deadline
- `compact` — context is about to be compacted (fork snapshots the *pre*-compaction context)
- `session_start` — a new session began
- `session_end` — the session ended (any reason)
- `manual_stop` — the session ended *while recently active* (stopped mid-conversation, not timed out)
- `boot` — the forksan daemon (re)discovered this live session (once per session)
- `context_tokens: 150000` / `context_used: 80%` / `context_left: 20000` — context-size
  thresholds, each firing at most once per session

Unknown keys are ignored; invalid values warn and fall back to defaults (`forksan forks` shows
the warnings). Fork bodies should be **idempotent** — a fork may fire at every idle pause.

`delivery: next_turn` queues the fork's final report and injects it into your session as
context on the next prompt (or into the next session in the same project, if the original is
gone). `discard` runs the fork for its tool side effects only.

`after` sequencing: the dependent fork runs once **all** its listed predecessors finish, with
every report quoted in its prompt (`after: [research, lint]`). `context: fork` goes further —
the dependent forks that predecessor fork's *own session*, seeing everything it saw and did;
at most one dependency may use it.

Runs of the same fork never overlap by default: if a moment fires while a previous run of that
fork is still going (say a 4-minute idle fork that takes ten minutes), the new fire waits for
it to finish, and any further fires arriving in the meantime are dropped — one is already
queued, and fork bodies are idempotent. Set `overlap: true` to allow concurrent runs.

## How it works

```
Claude Code ──hooks──▶ forksan (CLI) ──unix socket──▶ forksan-daemon
                                                        │  idle timers, moment planning,
                                                        │  SQLite state, report queue
                                                        └──▶ claude -p --resume <session> --fork-session
```

- A tiny hook shim forwards lifecycle events (SessionStart, UserPromptSubmit, Stop, PreCompact,
  SessionEnd) to a per-user daemon, auto-spawned on demand and self-terminating when idle.
- A fork run is a headless `claude -p --resume <your session> --fork-session` — a *copy* of your
  session's context; your real session is never touched. Hooks, plugins, and MCP servers are
  disabled inside forks, so forks can't recursively trigger forks.
- Reports come back as `additionalContext` on your next prompt, formatted as small
  `source: forksan` blocks.
- A boot sweep on daemon start services anything a dead daemon still owed (missed idle forks,
  sessions to close).

## CLI

```
forksan status          # daemon, sessions, running forks, recent runs + costs
forksan forks           # forks visible from here, with warnings
forksan run <name>      # fire a fork manually against the current session
forksan logs [-f]       # daemon log
forksan doctor          # install checks; --gc-fork-sessions 30d prunes old fork transcripts
forksan stop-daemon     # retire the daemon (it restarts on the next event)
```

## Configuration

`~/.forksan/config.toml`, overridable per project in `<project>/.forksan/config.toml`:

```toml
default_idle_deadline = "10m"  # bare `idle` deadline; 0 disables idle forks
session_timeout = "12h"        # close sessions idle longer than this (boot sweep)
quiet_period = "20m"           # daemon self-exit after this much nothing (global only)
concurrency = 4                # parallel fork runs (leader always warms the cache alone first)
fork_timeout = "10m"           # kill a fork run after this long
claude_bin = "claude"          # global only
context_window = 200000        # for context_used / context_left
report_ttl = "7d"              # drop undelivered reports after this
poll_budget_chars = 24000      # max report chars injected per turn

[models]                       # per-model context windows
"some-model-id" = 500000
```

## Costs, caveats

- **Every fork run is a real model call** billed to your Claude Code account (API or
  subscription). Use `throttle`, tight `run_on` lists, and `forksan status` (which shows
  per-run cost) to keep it deliberate.
- Fork runs leave ordinary headless-session transcripts under `~/.claude/projects/`; they're
  inert but may show up in `claude --resume` pickers. `forksan doctor --gc-fork-sessions 30d`
  prunes forksan's own old ones.
- "Once per session" latches (context thresholds, boot) reset when a session is resumed —
  Claude Code assigns resumed sessions a new id, so each resume leg counts as a fresh session.
- Two live sessions in the same project each run project forks independently; give heavy forks
  a generous `throttle`.
- The transcript-based context gauge parses an internal Claude Code format; if it changes,
  `context_*` triggers degrade to inactive rather than erroring.

## Other tools

The `.forksan/forks/` format is deliberately tool-agnostic; forksan is the reference
implementation for Claude Code. Other agent harnesses are welcome to read the same fork
definitions natively — the format spec above is the whole contract.

## License

MIT
