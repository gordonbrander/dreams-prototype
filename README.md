# Subconscious

A versioned document vault in one SQLite file, with a command line and an MCP server.

Subconscious stores JSON documents the way CouchDB does. Every write makes a new immutable revision. Documents can carry a JSON Schema type, and three fields get first-class support: `title`, `content`, and `tags`. You can read and write the vault from a shell, from files of Markdown with frontmatter, or from an AI agent over MCP.

## Build

```
cargo build --release
```

The binary is `target/release/subconscious`. SQLite is bundled, so there is nothing else to install.

## Quick start

```
subconscious init

cat > note.md <<'EOF'
---
_id: notes/hello.md
title: Hello
tags: [greeting, demo]
---
The first note.
EOF

subconscious doc put note.md
subconscious doc list
subconscious doc search hello
subconscious doc get notes/hello.md
```

Every command takes `--db PATH`. The default is `vault.db` in the current directory. The database is created on first use, so `init` is optional.

## Documents

A document is a JSON object. Reserved fields start with an underscore.

| Field | Meaning |
|---|---|
| `_id` | The document id. Any string up to 512 bytes. Generated as a UUID v7 when omitted. |
| `_rev` | The revision id, `<generation>-<sha256>`. Computed from the content. |
| `_parent` | The revision this one replaced. Absent on the first revision. |
| `_type` | A `doc://` reference to a schema document, pinned to one revision: `doc://schemas/note?rev=3-9f2a…`. Optional. |
| `_deleted` | `true` on a tombstone. |
| `_created_at` | When the revision was written. |
| `_actor` | Who wrote the revision, when a writer named itself with `--actor`. A scheduled task's agent writes as the task. |
| `_seq` | The global sequence number of the revision. |
| `_conflicts` | Other live revisions of the document, after sync made concurrent edits. Only on `get`. See [Conflicts](#conflicts). |
| `_deleted_conflicts` | Tombstoned revisions of the document other than the current one, such as the losers of a resolve. Only on `get --deleted-conflicts`. |

Everything else is the body. Three body fields are blessed:

- `title` must be a string.
- `content` must be a string. Markdown files put the text after the frontmatter here.
- `tags` must be an array of strings. Tags are part of the body, so they are versioned with it. An index makes lookup by tag fast.

### Revisions

Every write creates a new revision and keeps the old one. To update, name the current revision as `_parent`. If another write got there first, the write fails with a conflict. In one vault, history is linear. [Sync](#sync) can add a second branch, as in CouchDB. Then the revisions of a document form a tree, and the document has [conflicts](#conflicts) until you resolve them.

Delete writes a tombstone. A later write on a tombstone revives the document. The change feed shows every revision, tombstones included, in the order they were committed.

### Schemas

A schema is a document whose body is a JSON Schema. Put it like any other document, by convention under `schemas/`:

```
cat > note.yaml <<'EOF'
_id: schemas/note
title: Note
description: A note with a required title
type: object
required: [title]
properties:
  title: {type: string, minLength: 1}
  tags: {type: array, items: {type: string}}
EOF
subconscious doc put note.yaml
```

A document names its schema with a `doc://` reference: `_type: doc://schemas/note`. On write, the store pins the reference to the schema's current revision, `doc://schemas/note?rev=1-c04d…`, validates the body against that revision, and only then computes the document's `_rev`. So the pinned type is part of the revision, and an unchanged document written again is a no-op until its schema moves.

Revision ids are content hashes, so a pinned reference names the same schema bytes in every vault, forever. Edit a schema and new writes pin the new revision. Old documents keep their pin and still validate against what they were written with. To re-pin an old document, update it with the unpinned `_type`: `subconscious doc update n1 n1.md`, or `put_doc` with `_parent` set. A fetched document carries its pin, so an edit cycle keeps it.

Filters take either form. `--type doc://schemas/note` matches every pinned revision of that schema. `--type doc://schemas/note?rev=1-c04d…` matches one.

## Command line

```
subconscious [--db PATH] [--json] <command>

  init                                            create the database if needed
  serve                                           serve MCP over stdio
  mcp-json                                        print the MCP server entry for this vault

  doc put     [FILE] [--format json|yaml|md]      create, or update when the input names a revision
  doc update  <id> [FILE] [--parent REV]          replace the body
  doc get     <id> [--rev REV] [--deleted-conflicts]
                                                  current revision, or one revision
  doc delete  <id> [--parent REV]                 write a tombstone
  doc list    [--type T] [--tag G] [--limit N] [--before SEQ]
  doc search  <query> [--type T] [--tag G] [--limit N]
  doc conflicts [--limit N] [--after ID]          documents with conflicts
  doc resolve <id> [FILE]                         keep FILE (or the winner) and tombstone the conflicts
  doc resolve <id> --auto [--runner R] [--dry-run]
                                                  an agent writes the merge
  doc history <id> [--limit N]
  doc changes [--since SEQ] [--limit N]

  pull <peer.db>                                  copy the revisions this vault does not have
  sync <peer.db>                                  pull, then push

  task add    <id> --runner R --every 15m [--glob G] [--tag T] [--type T] [--id D]... [PROMPT_FILE]
  task list                                       every enabled task, its last run, and whether it is due
  task rm     <id>
  task check  <id>                                evaluate one task; nothing runs
  task run    <id> [--force]                      fire one task now
  task runs   <id> [--limit N]                    past runs

  runner add  <id> [--title T] [--timeout 10m] -- <command> [args]...
  runner list
  runner rm   <id>

  tick                                            one scheduler pass
  daemon [--interval 60s] [--poll 2s]             run the scheduler until stopped
  daemon install | uninstall                      start it at login (launchd or systemd)

  export <dir> [--type T] [--tag G]               write current documents to <dir>/<_id>
  import <dir>                                    read every *.md file under <dir>
```

Every command also takes `--actor NAME`, or the `SUBCONSCIOUS_ACTOR` variable, to name the writer of the revisions it creates. `--db` also reads `SUBCONSCIOUS_DB`.

### Input

`FILE` is a path, or `-` for stdin. No file means stdin. The extension picks the format: `.json`, `.yaml` or `.yml`, and `.md` or `.markdown`. Stdin is JSON unless `--format` says otherwise.

A Markdown file is YAML frontmatter between two `---` lines, then the content. The frontmatter sets fields. The text after the closing line becomes `content`, byte for byte. Nothing is inferred from headings.

### Output

One document prints as Markdown with frontmatter. A list prints as a table. Pass `--json` to get the same JSON structures the MCP tools return. Errors are always a JSON object on stderr, and the exit code is 1. The object's `name` says what failed, for example `conflict`, `validation`, or `runner`.

### Edit and write back

`doc get` output is valid input. You can save it, edit it, and give it to `doc put` or `doc update`. The CLI recomputes the revision from the file. An unchanged file is a no-op. An edited file becomes the next revision, with the file's `_rev` as its parent. So this is a complete edit cycle:

```
subconscious doc get notes/hello.md > hello.md
$EDITOR hello.md
subconscious doc put hello.md
```

`doc update` and `doc delete` look up the current revision for you. Pass `--parent` when you want an explicit compare-and-swap.

### Export and import

`export` writes every current document to `<dir>/<_id>` as Markdown with frontmatter. Nested ids make nested folders. Tombstones are skipped.

`import` reads every `.md` file under a folder. The path relative to the folder, extension included, is the `_id`. So `notes/foo.md` becomes the document `notes/foo.md`. A frontmatter `_id` that differs from the path is ignored. Files that came from `export` and were not changed are no-ops. Edited files become the next revision. New files are created.

Both commands continue past a failing file, report every file, and exit 1 if any failed.

## Sync

Two vaults replicate the way CouchDB databases do. A vault can pull from another vault, or sync with it in both directions:

```
subconscious --db laptop.db pull desktop.db     # desktop's changes into laptop
subconscious --db laptop.db sync desktop.db     # pull, then push
```

The peer is another vault file on this machine. It must exist, so run `init` on it first. A vault cannot sync with itself. Pull and sync are CLI commands only. An agent cannot start them over MCP.

### A session

```
subconscious --db laptop.db init
subconscious --db desktop.db init
subconscious --db laptop.db doc put notes/plan.md
subconscious --db laptop.db sync desktop.db
pulled 0 revisions from /Users/me/desktop.db (6 present, 0 excluded)
pushed 1 revision to /Users/me/desktop.db (6 present, 0 excluded)
```

Two vaults made by the same binary seed the same six built-in documents with identical revisions, so the first sync copies none of them.

### How a pull works

A pull reads the peer's change feed and copies every revision that this vault does not have. It copies the full history of each document, tombstones included. Revision ids are content hashes, so a revision has the same id in every vault, and a second pull copies nothing.

A copied revision keeps its `_created_at` and its `_actor` from the vault that wrote it. `--actor` does not apply to copied revisions. The pull does not validate copied revisions against their schemas again. Instead, each revision must hash to its `_rev`. If one does not, the pull fails.

The pull writes in batches of up to 1000 revisions. Each batch and its checkpoint commit together, so an interrupted pull continues where it stopped. The next pull reads only the peer's new changes. The checkpoint is kept per peer, keyed by the peer's full path, and records the peer's revision at that position. If the peer file was replaced, that revision does not match, and the pull reads the whole feed again. A moved peer file also causes a full read. Both are safe, because a revision that is already present is skipped.

Each line of output counts revisions:

| Count | Meaning |
|---|---|
| `pulled N` / `pushed N` | Revisions written into the target vault. |
| `present` | Revisions the target already had. |
| `excluded` | Tasks and runs, which never replicate. |
| `missing parent` | Revisions skipped because an ancestor did not replicate. Shown only when not zero. |
| `checkpoint reset` | The peer changed, so the whole feed was read again. |

`--json` prints one report for `pull` and two for `sync`: `peer` (the vault the revisions came from), `read`, `written`, `present`, `excluded`, `missing_parent`, `last_seq`, and `restarted`.

### What replicates

Every document replicates, with two exceptions:

- **Tasks.** A synced task would fire in both vaults.
- **Runs.** A run records a position in its own vault's change feed.

Runners and schemas replicate like other documents. **So a peer can change the commands that your tasks run.** A task follows the current revision of its runner. Sync only with vaults that you trust.

A copied revision counts as a change in this vault. So a task that waits for changes wakes for edits that came in through a sync.

Do not copy a vault file with Dropbox, iCloud, or a similar service while it is in use. Use `sync` between two separate files.

### Conflicts

If two vaults edit the same revision, a sync keeps both edits. The document now has two leaves. Every vault picks the same winner: a live revision beats a tombstone, then the higher generation wins, then the lower hash. (CouchDB keeps the higher hash. Any fixed rule gives every vault the same winner.) `doc get` returns the winner, and `_conflicts` lists the other live leaves:

```
subconscious doc get notes/plan.md
---
_id: notes/plan.md
_rev: 3-4be1…
_conflicts:
- 3-09ac…
…
```

An edit made on one side wins over a delete made on the other side, so the document comes back. This is the CouchDB rule.

Until you resolve, the document works as usual. Reads, lists, and search use the winner. `doc update` writes on the winner, and the conflict stays. `doc history` follows the winner's branch only. Read a losing leaf with `doc get <id> --rev <rev>`.

To find every document with conflicts:

```
subconscious doc conflicts
ID             WINNER      CONFLICTS
notes/plan.md  3-4be1…     1
```

To resolve, keep the winner:

```
subconscious doc resolve notes/plan.md
```

or write a merge on the winner:

```
subconscious doc get notes/plan.md > plan.md
$EDITOR plan.md
subconscious doc resolve notes/plan.md plan.md
```

`resolve` writes the merge (if given) as a child of the winner, and a tombstone on each revision in `_conflicts`, in one transaction. A merge file must build on the winner. A file with a different `_parent` fails with a conflict. Sync the result to the other vaults. You can also do the same steps by hand: `doc update` on the winner, then `doc delete <id> --parent <rev>` for each conflict.

A resolve does not delete the losing revisions. It writes a tombstone on each one, and you can still read them. As in CouchDB, `doc get <id> --deleted-conflicts` lists these tombstones as `_deleted_conflicts`.

Vaults seeded by different versions of this binary can have different built-in schemas or runners. A sync then makes conflicts on those documents. MCP clients cannot write them, so resolve them with the CLI.

### Let an agent merge

```
subconscious doc resolve notes/plan.md --auto --dry-run   # look first
subconscious doc resolve notes/plan.md --auto
```

`--auto` gives a runner the winner, every conflicting revision, and the last revision that they all shared. The agent compares each side with that shared revision, keeps the changes from every side, and replies with one merged body in JSON. The default runner is `runners/claude`. Use `--runner` to select a different one. The runner starts as it does for a task, with `{task}` set to `resolve/<id>`.

The merge keeps the winner's `_type`, unpinned, so it is validated against the current schema. Its `_actor` is the pinned reference of the runner revision that wrote it, unless you give `--actor`.

`--dry-run` prints the merge and writes nothing. Its output is valid input for `doc resolve <id> FILE`, so you can edit the merge before you apply it:

```
subconscious doc resolve notes/plan.md --auto --dry-run > merge.md
$EDITOR merge.md
subconscious doc resolve notes/plan.md merge.md
```

If a sync brings in a new conflict while the agent works, the resolve fails and writes nothing. Run it again. If the runner fails, times out, or replies without a JSON object, the command fails with an error named `runner`, and nothing is written. A merge that you do not like loses nothing: the losing revisions are still in the vault, and the merge is an ordinary revision that you can edit. A document without conflicts is printed, and no runner starts.

### Conflicts over MCP

An agent resolves conflicts with the same steps:

1. `list_conflicts` finds the documents.
2. `get_doc` returns the winner and its `_conflicts`. `get_rev` reads each one.
3. `resolve_doc` with `id`, the `merged` document (in the same shape as `put_doc`), and the `conflicts` it read. If new conflicts arrived since the read, the call fails, and the agent reads again.

## Scheduled tasks

A task wakes an agent on a schedule with a prompt. There are two kinds:

- **Periodic.** Every interval, run the prompt.
- **On change.** Every interval, run the prompt only if a watched document changed since the last run.

Tasks, runners, and runs are all documents in the vault. There is no other configuration.

### Your first task

1. Seed the default runners and look at them.

   ```
   subconscious init
   subconscious runner list
   ```

   You get `runners/claude`, `runners/codex`, and `runners/pi`. Each is the command that starts one agent. Pick the one whose CLI is installed and logged in.

2. Write the prompt in a file.

   ```
   cat > digest.md <<'EOF'
   Read every document tagged `inbox` with `subconscious doc list --tag inbox --json`.
   Write a short digest as a new document with the tag `digest`.
   EOF
   ```

   The agent has the `subconscious` binary on its `PATH`, and `SUBCONSCIOUS_DB` already points at this vault. It can read and write with the shell commands in this README.

3. Add the task.

   ```
   subconscious task add tasks/digest --runner runners/claude --every 1d digest.md
   ```

   The id is any document id. `--runner` takes a runner id, or a `doc://` reference; the task stores `doc://runners/claude` and follows later edits to that runner. The interval takes `30s`, `15m`, `2h`, `1d`, or `1w`. The prompt file is the last argument, or `-` for stdin.

4. Look before it runs.

   ```
   subconscious task check tasks/digest
   ```

   This prints the schedule, the last run, whether the task is due, and the exact command it will spawn. Nothing runs.

5. Run it once by hand.

   ```
   subconscious task run tasks/digest
   subconscious task runs tasks/digest
   ```

   `task run` fires at once and prints the run document. `task runs` lists past runs with their exit code and error. The agent's last message is in the run's `content`.

6. Start the clock.

   ```
   subconscious daemon install
   ```

   From now on the daemon starts at login and fires each task when it is due. `subconscious task list` shows every enabled task, its last run, and whether it is due right now.

### A task that waits for changes

Add a `when` filter and the task fires only if a matching document changed since its last run:

```
subconscious task add tasks/triage --runner runners/claude --every 15m --tag inbox triage.md
```

The filters are `--tag`, `--type`, `--glob` (a SQLite GLOB on `_id`, for example `inbox/*`), and `--id` (repeatable). They are AND-ed. The agent gets the prompt, then a section that lists what changed:

```
Triage the documents listed below.

---
Changed since last run (seq 4120 to 4133):
- inbox/call.md  rev 3-9f2a1c0e
- inbox/quote.md  rev 1-c04d77b2  (deleted)
```

Changes collect until a run consumes them. So the task fires at most once per interval, and never misses a change. `task check` shows the pending changes at any time. A task's own writes do not count: the agent writes with the task id as `_actor`, and the filter skips them.

### From an agent

An agent that uses the MCP server creates a task by writing a document. It does not need new tools.

```json
{
  "_id": "tasks/triage",
  "_type": "doc://schemas/task",
  "runner": "doc://runners/claude",
  "every": "15m",
  "when": { "tag": "inbox" },
  "prompt": "Triage the documents listed below.",
  "enabled": true
}
```

It finds runners with `list_docs` and `type: doc://schemas/runner`, and reads past runs with `list_docs`, `type: doc://schemas/run`, and `tag: <task id>`. It cannot write runner or run documents, or the seeded schemas. Those are read-only over MCP.

### Managing tasks

```
subconscious task list                 every enabled task, last run, due or not
subconscious task check <id>           one task in detail, plus the command it would run
subconscious task run <id> [--force]   fire now; --force also when the last run has not finished
subconscious task runs <id>            past runs, newest first
subconscious task rm <id>              delete the task; its runs stay
```

To pause a task, edit it with `enabled: false`:

```
subconscious doc get tasks/digest > t.md   # edit enabled: false
subconscious doc put t.md
```

A disabled task leaves `task list`; `task check` still shows it. `task add` on an existing id replaces it. An identical re-add writes nothing.

### Runners

A runner is a document typed `doc://schemas/runner` with the command that starts an agent. The command is an argument list, spawned without a shell. Inside each argument, `{db}`, `{task}`, `{run}`, `{mcp}`, `{out}`, and `{exe}` are replaced. The prompt goes to stdin. The last message is read from stdout, or from the `{out}` file when the command wrote one. `timeout` defaults to `10m`, after which the command is killed and the run records the timeout.

Add your own, for example a cheaper model for frequent tasks:

```
subconscious runner add runners/claude-fast --timeout 5m -- claude -p --model claude-sonnet-5 --permission-mode dontAsk
subconscious runner rm runners/pi
```

Runner commands are code. They enter only through the CLI, or from a vault that you [sync](#what-replicates) with. An agent can choose a runner for a task; it cannot define or change one. Deleted defaults stay deleted. `doc resolve --auto` also uses runners.

The command inherits these variables: `SUBCONSCIOUS_DB`, `SUBCONSCIOUS_TASK`, `SUBCONSCIOUS_RUN`, `SUBCONSCIOUS_ACTOR` (the task id), `SUBCONSCIOUS_MCP` (a generated MCP config for this vault), and `SUBCONSCIOUS_OUT`. `PATH` starts with the directory of this binary.

### Runs

Each firing writes a document typed `doc://schemas/run` at `runs/<task id>/<time>-<seq>`, tagged with the task id. Revision 1 is written before the agent starts and records `runner` as the pinned reference of the runner revision about to run. Revision 2 adds `finished_at`, `exit_code`, `error`, and the agent's last message as `content`. Run documents are the scheduler's only state. Tasks and runs stay in their own vault. A sync does not copy them. A run with no `finished_at` is in progress, or was cut off by a crash, and the task waits until the runner's timeout has passed before it fires again.

### The clock

```
subconscious tick                        one pass, then exit
subconscious daemon                      keep ticking; every --interval (60s) and sooner when the database changes
subconscious daemon install | uninstall  start it at login (launchd on macOS, systemd on Linux)
```

`daemon install` writes the service with your current `PATH`. Agents use their own stored logins; nothing else is copied. Logs go to `~/Library/Logs/subconscious/<name>.log` on macOS and to `journalctl --user -u subconscious-<name>` on Linux. Set `RUST_LOG=debug` for more. Two schedulers on one database do no harm: the run document is written before the agent starts, so the second one sees the task as running and skips it.

### When something goes wrong

- `task check <id>` shows the command. Copy it and run it by hand with the prompt on stdin.
- `task run <id>` exits 1 and prints the run when the agent fails. The run's `error` holds the exit code and the last lines of stderr.
- An agent that answers "not logged in" needs its own login: `claude`, `codex login`, or `pi`. The daemon has no terminal to ask you.
- A run with empty `content` and exit 0 from Codex means Codex could not reach its API. It exits 0 either way.

## MCP

`subconscious serve` speaks the stateless MCP protocol, version 2026-07-28, over stdio. Older protocol versions are refused. The host starts the binary as a child process and talks to it through its stdin and stdout. Logs go to stderr, controlled by `RUST_LOG`.

### Use with Claude Code

Register the server. `mcp-json` prints the server entry for the vault that `--db` names, with absolute paths, because the host does not start the server in your project directory.

```
claude mcp add-json -s user subconscious "$(subconscious --db ~/.subconscious/vault.db mcp-json)"
```

`-s user` makes the server available in all your projects. Leave it out to add the server to the current project only. To record writes under an agent name, add `--actor claude` before `mcp-json`. Other hosts take the same entry under `mcpServers` in their config file.

Then turn on protocol negotiation in Claude Code. Without it, Claude Code does not connect to stdio servers that speak 2026-07-28, and reports "Unsupported protocol version". Claude Code itself must have the variable, so `-e` does not work here. Add it to `~/.claude/settings.json`:

```json
{ "env": { "MCP_PROTOCOL_NEGOTIATION": "auto" } }
```

Or export `MCP_PROTOCOL_NEGOTIATION=auto` in your shell profile.

To check, start Claude Code and run `/mcp`. `subconscious` shows as connected, and its tools have names like `mcp__subconscious__list_docs`.

### Tools

Each store operation is one tool:

| Tool | Does |
|---|---|
| `put_doc` | Create, or update with `_parent` set to the current `_rev`. Takes `_id`, `_parent`, `_type`, and the fields in `body`, a JSON object. |
| `get_doc` | Current revision by `id`. `deleted_conflicts: true` adds `_deleted_conflicts`. |
| `get_rev` | One revision by `rev`. |
| `delete_doc` | Tombstone with `id` and `parent`. |
| `list_conflicts` | Documents with conflicts, in id order, with `after` and `limit`. |
| `resolve_doc` | Tombstone every conflict of `id`, after it writes `merged` on the winner if given. Pass the `conflicts` you read to fail if they changed. |
| `list_docs` | Current documents, newest first, with `type`, `tag`, `before`, `limit`. |
| `search_docs` | Full-text search with `query` and the same filters. |
| `doc_history` | Revisions of one document, newest first. |
| `changes` | Every revision after `since`. |

Results are structured JSON. A store error returns as an invalid params error with the error object as its data. Schemas and scheduled tasks need no extra tools. An agent puts a schema document and references it as `_type: doc://<id>`. It writes a task with `put_doc`, finds runners with `list_docs` and `type: doc://schemas/runner`, and reads runs the same way. Writes to runner, run, and seeded schema documents are refused. Pull and sync have no tools. See [Conflicts over MCP](#conflicts-over-mcp) for resolving.

## Storage

One SQLite file in WAL mode. Migrations run on open.

- `docs` holds one row per revision. Triggers refuse updates and deletes, and enforce the parent chain.
- `checkpoints` holds the position of the last pull from each peer, and the peer's revision at that position.
- The winner of each document is chosen by one view, `docs_winners`, with the rule in [Conflicts](#conflicts). Copied revisions enter `docs` through the same triggers as local writes.
- `doc_heads`, `doc_tags`, and `docs_fts` are projections of each document's current revision. One trigger keeps them in step on every write.
- Schemas are documents. Three are seeded on first use: `schemas/task`, `schemas/run`, and `schemas/runner`, plus the three default runners.

Search uses FTS5 with the porter tokenizer. Title matches rank highest, then tags, then content.

## Development

```
cargo test
cargo clippy --all-targets
```

The tests cover the revision model, the store, the scheduler rules, and the full command surface. CLI tests call the command runner in process with a temporary database, and drive the scheduler with runners such as `cat` and `sleep`.
