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
| `model` | model override for the fork run (window-guarded, see below) | session default |
| `tags` | labels for the enable/disable filter: `ci`, `[ci, review]` | — |
| `allowed_tools` | permission rules granted to the fork: `Write`, `[Write, "Bash(git add:*)"]` | — (read-only) |
| `permission_mode` | `default` \| `acceptEdits` \| `bypassPermissions` | config, else none |

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

Runs of the same fork never overlap by default: if a moment fires while a previous run of
that fork is still going (say a 4-minute idle fork whose run takes ten minutes), that fire is
simply cancelled. The fork fires again at its next moment — the next idle timeout only comes
around after you've prompted again, which is exactly when the previous run's report gets
injected into your session — so the next run inherits a parent context that already contains
the previous result. Set `overlap: true` to allow concurrent runs.

`model` overrides the model for the fork run, but is **window-guarded**: forking a large
session onto a smaller-window model would overflow its context and hang until `run_timeout`.
So when the session's tracked prompt already exceeds ~90% of the override model's context
window (from `[models]`, or `context_window` if unlisted), forksan drops the override and runs
the fork on the session's inherited model instead (logged at info). Before the first gauge
reading the override always applies.

### Fork permissions

A fork runs headless (`claude -p`) with isolated settings, so it **cannot answer permission
prompts** — by default it can only use read-only tools, and anything needing Write, Edit, or
Bash is denied. Grant what a fork needs up front:

- `allowed_tools` lists [Claude Code permission rules](https://docs.claude.com/en/docs/claude-code/iam#permission-rules),
  each passed verbatim to `--allowedTools`. Scope them tightly — prefer `Bash(git add:*)`,
  `Write`, `Edit` over blanket `Bash`. A single rule may contain commas, so entries are never
  comma-split (use a list for multiple rules).
- `permission_mode` maps to `--permission-mode`: `acceptEdits` auto-accepts file edits,
  `bypassPermissions` skips all checks (the blunt instrument — reserve it for trusted fork
  bodies), `default` is the normal gate. `plan` is rejected (a headless fork can't act on a
  plan). The fork's own `permission_mode` wins; otherwise the config default (below) applies;
  otherwise no flag. `allowed_tools` composes with whichever mode is in effect.

These two keys only affect the forksan runner — other tools consuming the fork format may
ignore them.

### Fork environment

Every fork subprocess gets the parent session's identity in its environment, so a fork can
key per-session state on disk deterministically (the cwd alone is not unique — several
sessions run in one directory):

- `FORKSAN_SESSION_ID` — the parent session id the fork was spawned for
- `FORKSAN_FORK_NAME` — this fork's name
- `FORKSAN_TRIGGER` — the trigger label (e.g. `idle:240`, `manual_stop`, `manual`)
- `FORKSAN_PROJECT_ROOT` — the session's project root

A fork always runs from the directory the session was **launched** in, even if the session
later `cd`'d elsewhere with its Bash tool — that launch directory is where Claude Code stored
the resumable transcript, so it's pinned per session and used as the fork's working directory.

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
  session's context; your real session is never touched. By default a fork loads your **full
  configuration** (plugins, MCP servers, skills, `CLAUDE.md`) so its request prefix matches the
  parent as closely as `claude` allows and reuses the **prompt cache** wherever possible.
  Measured cache behavior (byte-level request diffing): a fork of a **headless/print-mode**
  parent reuses ~100% of the parent's cached prefix; a fork of an **interactive** parent cannot
  reuse the parent's cache at all — interactive sessions load a larger tool set than `-p` mode
  (interactive-only tools lead the prefix), which is not controllable from the CLI. Consecutive
  forks of the *same* session always share their own full-config prefix (1-hour cache TTL), so
  repeated fork moments stay cheap either way. For big sessions triggering expensive-model
  forks, a `model:` override on the fork is often the bigger cost lever.
  Forks *do* fire your other (non-forksan) hooks and load MCP servers. Recursion is
  prevented not by stripping config but by an env guard: every fork subprocess carries
  `FORKSAN_FORK`, and forksan's own hooks exit immediately when they see it, so a fork can't
  trigger forks. Set `isolation = "hermetic"` (see [Configuration](#configuration)) to run bare
  forks instead (no plugins/MCP/hooks/`CLAUDE.md`) — note this *changes* the request prefix
  (settings-injected context differs), so hermetic forks never share cache with normal runs.
- Reports come back as `additionalContext` on your next prompt, formatted as small
  `source: forksan` blocks.
- A boot sweep on daemon start services anything a dead daemon still owed (missed idle forks,
  sessions to close).

## CLI

```
forksan status          # daemon, sessions, running forks, recent runs + costs
forksan forks           # forks visible from here, with warnings
forksan run <name>      # fire a fork manually against the current session
forksan run --tag <tag> # fire every fork carrying <tag> (bulk manual run)
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
run_timeout = "10m"            # kill a fork run after this long (alias: fork_timeout)
claude_bin = "claude"          # global only
context_window = 200000        # for context_used / context_left; per-model overrides in [models]
report_ttl = "7d"              # drop undelivered reports after this
poll_budget_chars = 24000      # max report chars injected per turn
enable_tags = ["ci"]           # default tag whitelist (see below)
disable_tags = ["noisy"]       # default tag blocklist (see below)
permission_mode = "acceptEdits" # default fork --permission-mode; a fork's own key wins
isolation = "open"             # "open" (full config, cache reuse) | "hermetic" (bare forks)

[models]                       # per-model context windows (sonnet/haiku/opus pinned to 200000)
"some-model-id" = 500000
```

`run_timeout` bounds a single fork run; raise it for legitimately long forks (`fork_timeout` is
an accepted alias). `[models]` maps a model id/alias to its context window; the common aliases
`sonnet`, `haiku`, and `opus` are pre-pinned to 200000 so the `model:` override guard (below)
stays correct even when you raise `context_window` for a large default model.

`isolation` controls how much of your setup a fork inherits. `open` (the default) loads your
full config so fork requests share your session's shape and reuse the prompt cache; forks then
fire your other hooks and load MCP servers, and recursion is held off by the `FORKSAN_FORK`
env guard. `hermetic` strips plugins, MCP servers, settings-derived hooks, and `CLAUDE.md`
(the pre-cache-economics behavior) for users who want fork sessions to run bare. Unknown
values warn and fall back to `open`.

`permission_mode` sets the default `--permission-mode` for every fork run
(`default` | `acceptEdits` | `bypassPermissions`; unknown values warn and are ignored). A
fork's own `permission_mode` frontmatter overrides it; with neither set, no flag is passed.
See [Fork permissions](#fork-permissions) for how it composes with `allowed_tools`.

### Tag filtering

Forks can carry `tags:` in their frontmatter, and a session can then narrow which
forks fire. The filter has two sets, an **enable** (whitelist) and a **disable**
(blocklist), applied per fork at selection time:

- If any of a fork's tags is in the disable set, the fork is skipped — **disable
  wins** over enable.
- If the enable set is present and non-empty, a fork runs only if at least one of
  its tags is in it — so **untagged forks are excluded by a whitelist**.
- With neither set configured, every fork runs (fully backward compatible).

Two sources feed the filter, per key:

- **Per session** — the environment variables `FORKSAN_ENABLE_TAGS` and
  `FORKSAN_DISABLE_TAGS` (comma-separated), read from the Claude Code process env
  by the hook and carried with the session. Set them per project/shell to scope a
  session (`FORKSAN_DISABLE_TAGS=noisy claude`).
- **Defaults** — the `enable_tags` / `disable_tags` config keys above (project
  layer over home layer). A session's env value overrides the config default for
  that key.

Manual runs (`forksan run <name>` and `forksan run --tag <tag>`) deliberately bypass the
filter.

### Per-tag throttles

`[tag_throttles]` maps a tag to a minimum gap between runs of **any** fork carrying that
tag — one shared budget for the whole group. A single run of any fork with the tag suppresses
every other fork sharing it until the window passes:

```toml
[tag_throttles]
ci = "1h"        # at most one run per hour across all forks tagged `ci`
review = "30m"
```

It composes with a fork's own `throttle` (both must pass), layers per key like `[models]`
(project entries override home), and — like the per-fork throttle — simply skips a suppressed
fork, which recurs at its next moment. Manual `forksan run` bypasses tag throttles.

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
- A session-end fork can fire before a large session has finished writing its transcript to
  disk, so `--resume` briefly can't find it; forksan retries the run a few times with backoff
  while the parent finishes, then gives up and reports the failure.

## Other tools

The `.forksan/forks/` format is deliberately tool-agnostic; forksan is the reference
implementation for Claude Code. Other agent harnesses are welcome to read the same fork
definitions natively — the format spec above is the whole contract.

## License

MIT
