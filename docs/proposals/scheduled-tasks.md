# Proposal: scheduled agent tasks

Status: Option B built on 2026-09-23 with runners as protected `runner/v1` documents and a daemon clock. See README, "Scheduled tasks". This file keeps the alternatives.

## Requirements

1. Model agnostic. MCP works with any agent. Scheduling should too, or at least support several agents.
2. Easy to use. One command adds, lists, or deletes a task. One command installs whatever the clock needs. No hand-made launchd.
3. An agent that uses the MCP server can add a task.
4. Most tasks wake when a document, or a set of documents, changes. Batched, at most once per interval. Pure time triggers are the minority.

## What the hosts offer today

Checked on 2026-09-22 against Claude Code 2.1.280, Codex CLI 0.39.0 (installed) and Codex docs, and Pi docs.

| Host | Headless run | Scheduler | Agent can add a task | Survives session end |
|---|---|---|---|---|
| Claude Code | `claude -p` with `--mcp-config`, `--permission-mode`, `--bare` | `/loop`, CronCreate (session only, 7-day expiry); `claude --bg "/loop ..."` (supervisor, 48 h); Desktop local routines in `~/.claude/scheduled-tasks/<name>/SKILL.md` (every minute, app must be open); cloud routines (1 h minimum, cannot reach a local file) | Yes: CronCreate in session, or write the routine file | Routines and `--bg` only |
| Codex CLI | `codex exec -a never --sandbox ... -o <file> -` | None in the CLI. Scheduled tasks exist in the Codex app and web only | No documented API | n/a |
| Pi | `pi -p --no-extensions -` | None. Scheduler extensions fire only while a Pi process is open | Only inside a live session | No |

Hooks in every host fire on events, never on a timer.

One blocker for every approach: `dreams serve` accepts only MCP 2026-07-28. Codex sends 2025-06-18 and cannot connect. Claude Code may also refuse. Either widen `supported_protocol_versions` in `src/mcp.rs`, or let the agent drive the `dreams` binary through Bash. Both are small.

## Shared piece: the watch primitive

Every viable option needs this, so it comes first. It answers "what changed that I care about, and is it time yet?"

A watch is a document. It is versioned, searchable, and an agent can create it with `put_doc`.

```yaml
_id: watches/inbox
_type: watch
watch: { tag: inbox }      # any of tag, type, prefix
every: 15m                 # fire at most once per interval
```

Two operations, exposed as MCP tools and CLI subcommands:

- `check_watch(id)` reads the changes feed since the watch's cursor, keeps the entries that match the filter, and returns them with `due: true` when the interval has passed since the last ack.
- `ack_watch(id, seq)` moves the cursor. The agent calls it after the work is done.

The cursor lives in the vault, so a fresh session each run still knows where it left off. This is the existing changes feed plus a cursor and a rate limit. About 100 lines.

### The self-wake trap

An agent that writes into the set it watches wakes itself on the next tick. Three fixes, in order of preference:

1. Add a nullable `actor` column to `docs`. `dreams serve --actor <id>` sets it on every write from that connection. `check_watch` skips rows whose actor is the watch's own id. Correct. One migration.
2. Ack the head `seq` at the end of the run. Simple, but drops changes made by others during the run.
3. Accept one extra run per interval and keep prompts idempotent. Zero code, wasteful.

## Option D: host clock, vault watches and teaches

The vault does not spawn agents and installs no timer. It gives the agent the watch primitive and instructions that map "recurring task" onto the host's own scheduler.

### The skill

MCP has three carriers for instructions: server `instructions`, prompts, and resources. There is no "install a skill" verb in the 2026-07-28 protocol as rmcp 3.4 exposes it. Claude Code turns MCP prompts into slash commands, so a `recurring-task` prompt is the closest thing. It says:

1. Create a watch document for the filter and interval.
2. Give your host scheduler this prompt: "call `check_watch`, do the task on the returned documents, then `ack_watch`."
3. Per host. Claude Code session: `/loop 15m`. Persistent: write `~/.claude/scheduled-tasks/<name>/SKILL.md`, or start `claude --bg "/loop ..."`. Codex and Pi: no clock available; see Option B.

A second carrier is files on disk. Claude, Codex, and Pi all read skill files. `dreams skill install <host>` writes the same text into each host's skill folder. One command, and it works for hosts that never read MCP prompts.

### Against the requirements

- Model agnostic: partly. The vault side is agnostic. The clock is not. Today only Claude has a scheduler an agent can drive that survives the session.
- Easy add, list, delete: add is easy in Claude. List and delete are per host. The vault sees watches, not schedules, so there is no single `schedule list`.
- Agent adds tasks: yes, where the host allows.
- Wake on change, batched: yes. The watch does the batching, the host cadence gives the interval.

### Cost

The watch primitive, the skill text, the MCP prompt, and `skill install`. Nothing else.

## Option B: vault clock

Dreams owns schedules, fires them, and records the runs. This follows eto's scheduler: durable schedule rows, a one-minute tick, publish before advance, and a failing row that advances instead of retrying every minute.

### Schedules and runs are documents

```yaml
_id: schedules/triage-inbox
_type: schedule
title: Triage inbox
watch: { tag: inbox }      # change trigger; same filter as a watch
every: 15m
cron: "0 7 * * *"          # optional time trigger; either or both
tz: America/Los_Angeles
runner: claude
prompt: |
  Triage the documents listed below.
enabled: true
```

A `_type: run` document per fire holds the schedule id, start time, exit code, the `seq` consumed, and the agent's last message as `content`. Run documents are the only state. A fire never rewrites the schedule, so schedules do not churn revisions.

### The tick

`dreams tick` is one pass:

1. List current `_type: schedule` documents with `enabled: true`.
2. For each, find the last run. A `watch` schedule is due when matching changes exist since the run's `seq` and the interval has passed. A `cron` schedule is due when the newest occurrence after the last run is in the past. Fire once for the latest missed time.
3. Spawn the runner with the prompt on stdin and a timeout. Matched ids and revisions are appended to the prompt. Capture the last message.
4. Write the run document.

### Runners

A runner is a command template that reads the prompt on stdin and writes the last message to stdout. Three built-ins, and a `_type: runner` document can add or override one.

```
claude  claude -p --bare --permission-mode dontAsk --mcp-config <path> --output-format json
codex   codex exec -a never --sandbox workspace-write -o <file> -
pi      pi -p --no-extensions -
```

This is the model-agnostic part. About 30 lines plus the timeout.

### The clock

- **B1, recommended.** `dreams schedule install` writes a launchd agent on macOS or a systemd user timer on Linux that runs `tick` every minute. `uninstall` removes it. No daemon. Survives reboot. launchd never overlaps one job.
- **B2.** `dreams daemon` with a sleep loop, installed as a `KeepAlive` service by the same `install` command. Same tick plus a loop. Worth it only for sub-minute reaction or a status endpoint.

### Command surface

```
dreams schedule add <id> [--cron EXPR] [--watch tag=inbox] [--every 15m] --runner claude [PROMPT_FILE]
dreams schedule list
dreams schedule rm <id>
dreams schedule run <id>            manual fire
dreams schedule runs <id>           run history
dreams schedule install | uninstall
dreams tick                         one pass; what the timer calls
```

MCP needs nothing beyond the `schedule` schema. `run_schedule` and `list_runs` are optional conveniences.

### Against the requirements

- Model agnostic: yes, through runners.
- Easy add, list, delete: yes, one command each, plus one `install`.
- Agent adds tasks: yes, `put_doc` with `_type: schedule`.
- Wake on change, batched: yes, the same watch filter.

### Cost

A cron parser (the `croner` crate, or a hand-rolled 5-field parser like eto's, about 170 lines), the tick, runner spawn, `install` with plist and unit templates, and the CLI aliases. About 600 lines of Rust and tests, minus the watch code shared with Option D. The `actor` column is one migration.

## Ruled out

- **Host-owned schedules without the vault** (a launchd plist per task that runs `claude -p`; a Desktop routine by hand). Zero code, but fails requirements 1, 2, and 3.
- **Agent as dispatcher.** One Claude routine every hour that runs `dreams schedule due` and does each task. Claude only, and one long task delays the rest.
- **Tick inside `dreams serve`.** No install, but tasks stop when the session closes, and one agent spawns another.

## Recommended order

1. Build the watch primitive and the `actor` column. Every option needs them.
2. Write the skill text. Serve it as an MCP prompt and in `instructions`. Add `skill install`.
3. Use Claude's own scheduler with that skill (Option D). See whether it covers the real tasks.
4. Add `tick`, runners, and `schedule install` (Option B) only if Codex or Pi need a clock, or a single `schedule list` matters.

Steps 1 and 2 are shared. The choice between host clock and vault clock can wait until one has been used.

## Sources

- Claude Code: headless, scheduled tasks, routines, desktop scheduled tasks, agent view, hooks at `https://code.claude.com/docs/en/`.
- Codex: `codex exec`, MCP config, hooks, app scheduled tasks at `https://developers.openai.com/codex/`.
- Pi: CLI, extensions, SDK at `https://github.com/earendil-works/pi/tree/main/packages/coding-agent/docs`.
- eto scheduler: `app/server/features/scheduler/` in `/Users/gordonb/Dev/eto`.
