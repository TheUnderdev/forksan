# autofork

**Forks for Claude Code.** When your session goes idle — or its context crosses a threshold —
autofork has the session's own model spawn *forks*: background **fork subagents** that inherit the
full conversation, do work with tools, and report back, all without interrupting you.

Think of a fork as a background thought your agent has while you're away: update the project
journal, distill notes, groom a TODO list, re-check an assumption — running in the background with
the fork's report arriving as a completion notification your agent relays when it's done.

> forks are **not** skills. A skill is something the model chooses to load and follow. A fork is
> something the *harness* schedules at lifecycle moments the model never sees. A fork fires because
> its `run_on` moment happened, full stop — there is no retrieval/RAG involved.

## How a fork fires (v0.5)

autofork no longer runs forks as headless subprocesses. Instead:

1. When a turn ends, an **asyncRewake `Stop` hook** long-polls the autofork daemon in the
   background without blocking your session.
2. When forks come due (an idle deadline elapses, or a context threshold was crossed), the daemon
   answers the poll with a **wake payload** and the hook exits 2 — which wakes the idle session and
   shows the payload as a system reminder.
3. The woken model reads the payload and calls the **Agent tool with `subagent_type: "fork"`** for
   each due fork — background subagents that inherit the entire conversation. Measured first-request
   cache on such a fork: **cache_read 31,681 / cache_creation 326 — ~99% of the parent prefix
   reused.**
4. Each fork runs in the background; its completion notification wakes the session again on its own,
   and the model relays the fork's report. **Delivery is native** — no report queue, no context
   injection.

Because the `fork` subagent type only exists in interactive sessions, **v0.5 is interactive-only by
design**; headless/`-p` and postmortem forks are gone. A fork inherits the session's **permissions
and model** — there is nothing to grant or override.

## Requirements

autofork v0.5+ needs a Claude Code version whose Agent tool supports `subagent_type: "fork"` in
interactive sessions:

- **Claude Code >= 2.1.161** — the fork subagent is enabled by default (recommended).
- **2.1.117 – 2.1.160** — it exists but is gated; export `CLAUDE_CODE_FORK_SUBAGENT=1`.
- **< 2.1.117** — no fork subagent; autofork v0.5 can't run forks.

`autofork doctor` checks your `claude --version` against these thresholds.

### If wakes report the fork type unavailable

Even on a fully current version a wake can report **`Agent type 'fork' not found`** — the fork
subagent ships behind a **staged server-side rollout**. The confirmed fix is to force-enable it
persistently in `~/.claude/settings.json`:

```json
{ "env": { "CLAUDE_CODE_FORK_SUBAGENT": "1" } }
```

(Prefer this over a shell `export` so every session gets it.) As a safety net, each wake also tells
the model to retry the fork call once and, if it still fails, to hold the spawn instructions and run
them on your next message rather than substituting a wrong agent — so a transient miss self-corrects
even without the pin. (Deferred agent rosters that key off the user's prompt are plausible and were
briefly suspected here, but the evidence was confounded — see below — so the env pin, not any
disclosure mechanism, is the remedy.)

> **Never let a wake create a `fork` agent file.** If the fork type is missing, the correct fix is
> the env pin above — *not* a custom `~/.claude/agents/fork.md`. A custom agent named `fork` does not
> inherit the conversation (only the built-in type does) and shadows the real one, so its "report"
> will show no knowledge of your session. Wakes are instructed never to create one; if you suspect an
> impostor slipped in (a fork "ran" but its report is context-blind), run `autofork doctor` — it flags
> `fork.md` under `.claude/agents/`. Delete it.

## Install

From the plugin marketplace:

```
/plugin marketplace add TheUnderdev/autofork
/plugin install autofork@autofork
```

On first use a bootstrap step downloads the prebuilt binary for your platform from GitHub
Releases into the plugin's persistent data directory (or builds it with `cargo` if no artifact
matches). macOS (arm64/x64) and Linux (x64/arm64) are covered.

For local development: `claude --plugin-dir ./plugin` inside this repo.

## Writing forks

Forks live in `.autofork/forks/`, discovered upward from your project directory plus the
user-level `~/.autofork/forks/`. Two layouts, mix freely (subfolders are just organization):

```
.autofork/forks/
├── journal.md              # a fork named "journal"
├── style-guide.md          # a companion NOTE (no `fork: true`) — not a fork
├── maintenance/
│   └── groom-todos.md      # a fork named "groom-todos"
└── deep-review/
    └── FORK.md             # a fork named "deep-review"
```

A fork is a markdown file whose frontmatter carries **`fork: true`**: YAML frontmatter for *when*,
body for *what to do*.

```markdown
---
fork: true
description: Keep NOTES.md current with what happened this session
run_on:
  - idle: 15m
throttle: 30m
---
Review the session so far and update NOTES.md with any durable decisions,
open questions, and next steps. Keep it under 200 lines.
```

Since v0.5, `.autofork/forks/` may hold arbitrary companion `.md` files (reference material a fork's
body tells it to read, for instance). Only files marked `fork: true` are forks; anything else is
skipped. As a guard rail, a file that looks like a fork (carries `run_on`, `throttle`, `tags`,
`after`, `overlap`, `description`, …) but lacks the marker produces a warning in `autofork forks`, so
a missing marker can't silently disable a real fork. `fork: false` is an explicit, silent opt-out.

### Frontmatter reference

| Key | Values | Default |
|---|---|---|
| `fork` | `true` — **required** on every fork | — |
| `description` | free text, for humans (`autofork forks`) | — |
| `run_on` | list of moments, see below | `[idle]` |
| `throttle` | min gap between runs: `30m`, `2h`, `90` (seconds) | none |
| `after` | fork name(s) to run after: `journal`, `[a, b]` | — |
| `overlap` | `true` to allow two runs of this fork at once | `false` |
| `tags` | labels for the enable/disable filter: `ci`, `[ci, review]` | — |

Moments for `run_on`:

- `idle` — the session has been quiet for the default idle deadline (config, 10m)
- `idle: 20m` — a custom idle deadline
- `context_tokens: 150000` / `context_used: 80%` / `context_left: 20000` — context-size thresholds,
  each firing at most once per session

Unknown keys are ignored; invalid values warn and fall back to defaults (`autofork forks` shows the
warnings). Fork bodies should be **idempotent** — a fork may fire on any idle pause.

**Once per pause.** An idle-triggered fork fires **at most once per idle pause** (restoring the
pre-v0.5 "fires once per idle pause" semantics). A *pause* is the quiet stretch after one of your
turns; only genuine user activity starts a new one. This matters because each wake turn — and each
fork-completion relay turn — ends with its own `Stop`, which re-arms the machinery; without the
per-pause rule a fork whose `throttle` is shorter than its idle deadline would wake you again every
cycle, forever. So within a single pause a fork issues one wake and no more, regardless of throttle;
`throttle` still applies *across* pauses. (`context_*` thresholds are separately once-per-session.)

`after` sequencing: the wake payload tells the model to spawn the root fork(s) now and, once a
predecessor's completion notification arrives, spawn its dependents with the predecessor's report
quoted into their prompt (`after: [research, lint]` waits for both). The parent orchestrates the
chain from the notifications it receives.

By default two runs of the same fork never overlap: the wake block for a fork tells the model to
skip spawning it if a previous run of that fork is still among its running background tasks. Set
`overlap: true` to drop that line and allow concurrent runs.

## CLI

```
autofork status          # daemon, sessions, recent wakes
autofork forks           # forks visible from here, with warnings
autofork run <name>      # print the spawn instruction to paste into an interactive session
autofork run --tag <tag> # print instructions for every fork carrying <tag>
autofork logs [-f]       # daemon log
autofork doctor          # install checks
autofork stop-daemon     # retire the daemon (it restarts on the next event)
```

`autofork run` can no longer spawn a fork itself (forks are subagents of an interactive session); it
prints the wake-style spawn instruction for you to paste into a live session.

## Configuration

`~/.autofork/config.toml`, overridable per project in `<project>/.autofork/config.toml`:

```toml
default_idle_deadline = "10m"  # bare `idle` deadline; 0 disables idle forks
session_timeout = "12h"        # close sessions idle longer than this
quiet_period = "20m"           # daemon self-exit after this much nothing (global only)
wake_debounce = "5s"           # batch near-simultaneous forks into one wake; 0 answers immediately
enable_tags = ["ci"]           # default tag whitelist (see below)
disable_tags = ["noisy"]       # default tag blocklist (see below)

[tag_throttles]                # min gap between wakes of any fork carrying a tag
ci = "1h"
```

`wake_debounce` gives near-simultaneous forks (idle deadlines close together, say) a moment to
coalesce into a single wake with multiple spawn blocks. A prompt arriving during the window cancels
the wake cleanly and stamps no throttles.

### Tag filtering

Forks can carry `tags:` in their frontmatter, and a session can then narrow which forks fire. The
filter has two sets, an **enable** (whitelist) and a **disable** (blocklist), applied per fork at
selection time:

- If any of a fork's tags is in the disable set, the fork is skipped — **disable wins** over enable.
- If the enable set is present and non-empty, a fork runs only if at least one of its tags is in it
  — so **untagged forks are excluded by a whitelist**.
- With neither set configured, every fork runs.

Two sources feed the filter, per key:

- **Per session** — the environment variables `AUTOFORK_ENABLE_TAGS` and `AUTOFORK_DISABLE_TAGS`
  (comma-separated), read from the Claude Code process env by the hook. Set them per project/shell to
  scope a session (`AUTOFORK_DISABLE_TAGS=noisy claude`).
- **Defaults** — the `enable_tags` / `disable_tags` config keys above (project layer over home
  layer). A session's env value overrides the config default for that key.

### Per-tag throttles

`[tag_throttles]` maps a tag to a minimum gap between wakes of **any** fork carrying that tag — one
shared budget for the whole group. A wake of any fork with the tag suppresses every other fork
sharing it until the window passes. It composes with a fork's own `throttle` (both must pass) and
layers per key (project entries override home).

Because the daemon can no longer observe fork completion, `throttle` and the tag throttles are
stamped at **wake-issuance** (when the daemon answers the poll), not at fork completion.

## Costs, caveats

- **Every fork is a real model call** billed to your Claude Code account. Because a fork inherits the
  parent prefix, the *marginal* cost is dominated by cheap cache reads (~99% reuse measured) plus the
  fork's own work. Use `throttle`, tight `run_on` lists, and `autofork status` to keep it deliberate.
- "Once per session" latches (context thresholds) reset when a session is resumed — Claude Code
  assigns resumed sessions a new id, so each resume leg counts fresh.
- The transcript-based context gauge parses an internal Claude Code format; if it changes, the
  `context_*` triggers degrade to inactive rather than erroring. The window used for
  `context_used` / `context_left` is 200k by default and 1M when the session's model carries Claude
  Code's `[1m]` marker (e.g. `claude-opus-4-8[1m]`); a gauge that exceeds the assumed window bumps
  it to the 1M tier as a fallback. The per-model window config was dropped in v0.5.
- A wake requires a live parked `Stop` hook. If the daemon dies while a session is idle, that idle
  opportunity is simply missed — the next turn re-arms it. A hook never wedges or errors a session.
- A session whose Claude process dies (killed terminal, restart) is closed automatically: its parked
  poll drops, and after a short grace with no new event the daemon marks it closed. A stray open
  session that crashed mid-turn shows a `[stale?]` hint in `autofork status`.

## v0.4 → v0.5 migration

v0.5 is a breaking release that replaces headless fork subprocesses with fork subagents spawned by
the session's own model.

- **Add `fork: true`** to every existing fork file (both `<name>.md` and `<name>/FORK.md`). Files
  without the marker are no longer treated as forks.
- **Default `run_on`** changed from `[idle, compact]` to `[idle]`.
- **Dropped moments.** `compact`, `session_start`, `session_end`, `manual_stop`, and `boot` are no
  longer supported — they are parsed but warned and ignored, and a fork whose only moments are
  unsupported never fires (with a visible warning in `autofork forks`). Supported moments: `idle`,
  `idle:<dur>`, and the three `context_*` thresholds.
- **Ignored frontmatter keys.** `delivery`, `model`, `allowed_tools`, and `permission_mode` are
  parsed-and-ignored with a warning: delivery is native, and a fork inherits the session's model and
  permissions.
- **Ignored config keys.** `claude_bin`, `concurrency`, `isolation`, `permission_mode`,
  `run_timeout`/`fork_timeout`, `context_window`, `[models]`, and the report/poll budgets are
  accepted-and-warned, then ignored. Old config files never hard-error. The new `wake_debounce` key
  is the only addition.
- **Interactive-only.** The `fork` subagent type does not exist in headless `-p` sessions, so v0.5
  drops headless and postmortem support entirely.
- **Cache economics.** The old warning that an *interactive* parent's forks couldn't reuse its cache
  no longer applies — a fork subagent inherits the live conversation and reuses ~99% of the prefix.

## Other tools

The `.autofork/forks/` format is deliberately tool-agnostic; autofork is the reference implementation
for Claude Code. Other agent harnesses are welcome to read the same fork definitions natively — the
format spec above is the whole contract.

## License

MIT
