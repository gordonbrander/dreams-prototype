# Dreams

A versioned document vault in one SQLite file, with a command line and an MCP server.

Dreams stores JSON documents the way CouchDB does. Every write makes a new immutable revision. Documents can carry a JSON Schema type, and three fields get first-class support: `title`, `content`, and `tags`. You can read and write the vault from a shell, from files of Markdown with frontmatter, or from an AI agent over MCP.

## Build

```
cargo build --release
```

The binary is `target/release/dreams`. SQLite is bundled, so there is nothing else to install.

## Quick start

```
dreams init

cat > note.md <<'EOF'
---
_id: notes/hello.md
title: Hello
tags: [greeting, demo]
---
The first note.
EOF

dreams doc put note.md
dreams doc list
dreams doc search hello
dreams doc get notes/hello.md
```

Every command takes `--db PATH`. The default is `vault.db` in the current directory. The database is created on first use, so `init` is optional.

## Documents

A document is a JSON object. Reserved fields start with an underscore.

| Field | Meaning |
|---|---|
| `_id` | The document id. Any string up to 512 bytes. Generated as `<UUID v7>.md` when omitted. |
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
dreams doc put note.yaml
```

A document names its schema with a `doc://` reference: `_type: doc://schemas/note`. On write, the store pins the reference to the schema's current revision, `doc://schemas/note?rev=1-c04d…`, validates the body against that revision, and only then computes the document's `_rev`. So the pinned type is part of the revision, and an unchanged document written again is a no-op until its schema moves.

Revision ids are content hashes, so a pinned reference names the same schema bytes in every vault, forever. Edit a schema and new writes pin the new revision. Old documents keep their pin and still validate against what they were written with. To re-pin an old document, update it with the unpinned `_type`: `dreams doc update n1 n1.md`, or `put_doc` with `_parent` set. A fetched document carries its pin, so an edit cycle keeps it.

Filters take either form. `--type doc://schemas/note` matches every pinned revision of that schema. `--type doc://schemas/note?rev=1-c04d…` matches one.

## Command line

```
dreams [--db PATH] [--json] <command>

  init                                            create the database if needed, and seed
  restore-defaults                                write the built-in documents again
  serve                                           serve MCP over stdio
  mcp-json                                        print the MCP server entry for this vault

  doc put     [FILE] [--format json|yaml|md]      create, or update when the input names a revision
  doc update  <id> [FILE] [--parent REV]          replace the body
  doc get     <id | doc://id[?rev=REV]> [--deleted-conflicts]
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

  task add    <id> --runner R --every 15m [--glob G] [--tag T] [--type T] [--id D]... [--no-deploy] [--yes] [PROMPT_FILE]
  task list                                       every task, its state here, its last run, and whether it is due
  task deploy [<id>] [--yes]                      run the current revisions on this vault; asks first
  task disable <id>                               stop running a task on this vault
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
  import <dir>                                    read every .md/.json/.yaml file under <dir>
```

Every command also takes `--actor NAME`, or the `DREAMS_ACTOR` variable, to name the writer of the revisions it creates. `--db` also reads `DREAMS_DB`.

### Input

`FILE` is a path, or `-` for stdin. No file means stdin. The extension picks the format: `.json`, `.yaml` or `.yml`, and `.md` or `.markdown`. Stdin is JSON unless `--format` says otherwise.

A Markdown file is YAML frontmatter between two `---` lines, then the content. The frontmatter sets fields. The text after the closing line becomes `content`, byte for byte. Nothing is inferred from headings.

### Output

One document prints as Markdown with frontmatter. A list prints as a table. Pass `--json` to get the same JSON structures the MCP tools return. Errors are always a JSON object on stderr, and the exit code is 1. The object's `name` says what failed, for example `conflict`, `validation`, or `runner`.

### Edit and write back

`doc get` output is valid input. You can save it, edit it, and give it to `doc put` or `doc update`. The CLI recomputes the revision from the file. An unchanged file is a no-op. An edited file becomes the next revision, with the file's `_rev` as its parent. So this is a complete edit cycle:

```
dreams doc get notes/hello.md > hello.md
$EDITOR hello.md
dreams doc put hello.md
```

`doc update` and `doc delete` look up the current revision for you. Pass `--parent` when you want an explicit compare-and-swap.

### Export and import

`export` writes every current document to `<dir>/<_id>`. The `_id` extension picks the format: `.json` is JSON, `.yaml` and `.yml` are YAML, and all other ids are Markdown with frontmatter. Nested ids make nested folders. Tombstones are skipped.

`import` reads every `.md`, `.markdown`, `.json`, `.yaml`, and `.yml` file under a folder. The extension picks the parser. Only lowercase extensions match. The path relative to the folder, extension included, is the `_id`. So `notes/foo.md` becomes the document `notes/foo.md`, and `data/bar.json` becomes `data/bar.json`. An `_id` in the file that differs from the path is ignored. Files that came from `export` and were not changed are no-ops. Edited files become the next revision. New files are created.

Both commands continue past a failing file, report every file, and exit 1 if any failed.

## Sync

Two vaults replicate the way CouchDB databases do. A vault can pull from another vault, or sync with it in both directions:

```
dreams --db laptop.db pull desktop.db     # desktop's changes into laptop
dreams --db laptop.db sync desktop.db     # pull, then push
```

The peer is another vault file on this machine. It must exist, so run `init` on it first. A vault cannot sync with itself. Pull and sync are CLI commands only. An agent cannot start them over MCP.

### A session

```
dreams --db laptop.db init
dreams --db desktop.db init
dreams --db laptop.db doc put notes/plan.md
dreams --db laptop.db sync desktop.db
pulled 0 revisions from /Users/me/desktop.db (15 present)
pushed 1 revision to /Users/me/desktop.db (15 present)
```

Two vaults made by the same binary seed the same fifteen built-in documents with identical revisions, so the first sync copies none of them.

### How a pull works

A pull reads the peer's change feed and copies every revision that this vault does not have. It copies the full history of each document, tombstones included. Revision ids are content hashes, so a revision has the same id in every vault, and a second pull copies nothing.

A copied revision keeps its `_created_at` and its `_actor` from the vault that wrote it. `--actor` does not apply to copied revisions. The pull does not validate copied revisions against their schemas again. Instead, each revision must hash to its `_rev`. If one does not, the pull fails.

The pull writes in batches of up to 1000 revisions. Each batch and its checkpoint commit together, so an interrupted pull continues where it stopped. The next pull reads only the peer's new changes. The checkpoint is kept per peer, keyed by the peer's full path, and records the peer's revision at that position. If the peer file was replaced, that revision does not match, and the pull reads the whole feed again. A moved peer file also causes a full read. Both are safe, because a revision that is already present is skipped.

Each line of output counts revisions:

| Count | Meaning |
|---|---|
| `pulled N` / `pushed N` | Revisions written into the target vault. |
| `present` | Revisions the target already had. |
| `missing parent` | Revisions skipped because an ancestor did not replicate. Shown only when not zero. |
| `checkpoint reset` | The peer changed, so the whole feed was read again. |

`--json` prints one report for `pull` and two for `sync`: `peer` (the vault the revisions came from), `read`, `written`, `present`, `missing_parent`, `last_seq`, and `restarted`.

### What replicates

Every document replicates: notes, schemas, runners, tasks, and run receipts.

State that belongs to one vault is not a document, and does not replicate. This includes which tasks run here, and where each task's schedule is. A task that arrives by sync is **dormant**: it does not run until you deploy it with `dreams task deploy <id>`. So a task runs only on the vaults where you deployed it.

Runners replicate so that you can share your configurations. A deploy pins the task revision and the runner revision, so an edit from a peer, to a task or to its runner, does not run on your vault until you deploy again. `task list` shows `changed` for a task with an edit that is not deployed. A deleted task, or a deleted runner, stops at once on every vault that syncs the deletion. No deploy is needed to stop.

A copied revision counts as a change in this vault. So a task that waits for changes wakes for edits that came in through a sync.

Do not copy a vault file with Dropbox, iCloud, or a similar service while it is in use. Use `sync` between two separate files.

### Conflicts

If two vaults edit the same revision, a sync keeps both edits. The document now has two leaves. Every vault picks the same winner: a live revision beats a tombstone, then the higher generation wins, then the lower hash. (CouchDB keeps the higher hash. Any fixed rule gives every vault the same winner.) `doc get` returns the winner, and `_conflicts` lists the other live leaves:

```
dreams doc get notes/plan.md
---
_id: notes/plan.md
_rev: 3-4be1…
_conflicts:
- 3-09ac…
…
```

An edit made on one side wins over a delete made on the other side, so the document comes back. This is the CouchDB rule.

Until you resolve, the document works as usual. Reads, lists, and search use the winner. `doc update` writes on the winner, and the conflict stays. `doc history` follows the winner's branch only. Read a losing leaf with `doc get 'doc://<id>?rev=<rev>'`.

To find every document with conflicts:

```
dreams doc conflicts
ID             WINNER      CONFLICTS
notes/plan.md  3-4be1…     1
```

To resolve, keep the winner:

```
dreams doc resolve notes/plan.md
```

or write a merge on the winner:

```
dreams doc get notes/plan.md > plan.md
$EDITOR plan.md
dreams doc resolve notes/plan.md plan.md
```

`resolve` writes the merge (if given) as a child of the winner, and a tombstone on each revision in `_conflicts`, in one transaction. A merge file must build on the winner. A file with a different `_parent` fails with a conflict. Sync the result to the other vaults. You can also do the same steps by hand: `doc update` on the winner, then `doc delete <id> --parent <rev>` for each conflict.

A resolve does not delete the losing revisions. It writes a tombstone on each one, and you can still read them. As in CouchDB, `doc get <id> --deleted-conflicts` lists these tombstones as `_deleted_conflicts`.

Vaults seeded by different versions of this binary can have different built-in schemas, runners, skills, or prompts. A sync then makes conflicts on those documents. MCP clients cannot write the schemas or runners, so resolve them with the CLI.

### Let an agent merge

```
dreams doc resolve notes/plan.md --auto --dry-run   # look first
dreams doc resolve notes/plan.md --auto
```

`--auto` gives a runner the winner, every conflicting revision, and the last revision that they all shared. The agent compares each side with that shared revision, keeps the changes from every side, and replies with one merged body in JSON. The default runner is `runners/claude`. Use `--runner` to select a different one. The runner starts as it does for a task, with `{task}` set to `resolve/<id>`.

The merge keeps the winner's `_type`, unpinned, so it is validated against the current schema. Its `_actor` is the pinned reference of the runner revision that wrote it, unless you give `--actor`.

`--dry-run` prints the merge and writes nothing. Its output is valid input for `doc resolve <id> FILE`, so you can edit the merge before you apply it:

```
dreams doc resolve notes/plan.md --auto --dry-run > merge.md
$EDITOR merge.md
dreams doc resolve notes/plan.md merge.md
```

If a sync brings in a new conflict while the agent works, the resolve fails and writes nothing. Run it again. If the runner fails, times out, or replies without a JSON object, the command fails with an error named `runner`, and nothing is written. A merge that you do not like loses nothing: the losing revisions are still in the vault, and the merge is an ordinary revision that you can edit. A document without conflicts is printed, and no runner starts.

### Conflicts over MCP

An agent resolves conflicts with the same steps:

1. `list_conflicts` finds the documents.
2. `get_doc` returns the winner and its `_conflicts`. `get_doc` with `doc://<id>?rev=<rev>`, or `get_rev`, reads each one.
3. `resolve_doc` with `id`, the `merged` document (in the same shape as `put_doc`), and the `conflicts` it read. If new conflicts arrived since the read, the call fails, and the agent reads again.

## Scheduled tasks

A task wakes an agent on a schedule with a prompt. There are two kinds:

- **Periodic.** Every interval, run the prompt.
- **On change.** Every interval, run the prompt only if a watched document changed since the last run.

Tasks, runners, and runs are documents in the vault. A task document is a template: it replicates, but it runs only on a vault where it is deployed. A deploy pins the exact task and runner revisions that run, and asks you to confirm them first. `task add` deploys the task on this vault.

### Your first task

1. Seed the default runners and look at them.

   ```
   dreams init
   dreams runner list
   ```

   You get `runners/claude`, `runners/codex`, and `runners/pi`. Each is the command that starts one agent. Pick the one whose CLI is installed and logged in.

2. Write the prompt in a file.

   ```
   cat > digest.md <<'EOF'
   Read every document tagged `inbox` with `dreams doc list --tag inbox --json`.
   Write a short digest as a new document with the tag `digest`.
   EOF
   ```

   The agent has the `dreams` binary on its `PATH`, and `DREAMS_DB` already points at this vault. It can read and write with the shell commands in this README.

3. Add the task.

   ```
   dreams task add tasks/digest --runner runners/claude --every 1d digest.md
   ```

   The id is any document id. `--runner` takes a runner id, or a `doc://` reference; the task stores `doc://runners/claude`. The interval takes `30s`, `15m`, `2h`, `1d`, or `1w`. The prompt file is the last argument, or `-` for stdin.

   `task add` then shows the prompt and the command, and asks `Deploy? [y/N]` on your terminal. Answer `y` to run the task on this vault. The question goes to the terminal, not to stdin, so a prompt on stdin works. In a script, pass `--yes` to deploy without the question. Without a terminal and without `--yes`, the deploy fails and the task stays dormant.

4. Look before it runs.

   ```
   dreams task check tasks/digest
   ```

   This prints the schedule, the last run, whether the task is due, and the exact command it will spawn. Nothing runs.

5. Run it once by hand.

   ```
   dreams task run tasks/digest
   dreams task runs tasks/digest
   ```

   `task run` fires at once and prints the run receipt. `task runs` lists past runs with their exit code and error. The agent's last message is in the run's `content`.

6. Start the clock.

   ```
   dreams daemon install
   ```

   From now on the daemon starts at login and fires each deployed task when it is due. `dreams task list` shows every task, its state on this vault (`deployed`, `disabled`, or `dormant`, plus `changed` when an edit is not deployed), its last run, and whether it is due right now.

### A task that waits for changes

Add a `when` filter and the task fires only if a matching document changed since its last run:

```
dreams task add tasks/triage --runner runners/claude --every 15m --tag inbox triage.md
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

An agent that uses the MCP server creates a task by writing a document, then asks to deploy it with the `deploy_task` tool. A task document that is not deployed does not run.

```json
{
  "_id": "tasks/triage",
  "_type": "doc://schemas/task",
  "runner": "doc://runners/claude",
  "every": "15m",
  "when": { "tag": "inbox" },
  "prompt": "Triage the documents listed below."
}
```

`deploy_task` takes a task `id`, or no id for every task that is not deployed at its current revisions. It never deploys on its own: it asks you, through your MCP client, to confirm the exact prompt and command. Your client shows the question; the agent cannot answer it. If you decline, nothing is deployed. If the task or runner changed before you answered, you are asked again. A client that cannot ask questions (MCP elicitation) cannot deploy; run `dreams task deploy` in a terminal instead. A scheduled agent runs with no person to ask, so it cannot deploy tasks.

`disable_task` takes a task `id` and stops it on this vault, with no question.

It finds runners with `list_docs` and `type: doc://schemas/runner`, and reads past runs with `list_docs`, `type: doc://schemas/run`, and `tag: <task id>`. It cannot write runner or run documents, or the seeded schemas. Those are read-only over MCP.

### Where an agent runs, and what it can use

An agent runs in the folder `workspace`, next to the vault, and tasks share it. To use another folder, give the task a `cwd`: `task add --cwd DIR`, or the `cwd` field in the task document. A relative path is relative to the vault's folder, and `~/` is your home folder. The folder is made when the task first runs. `task check` shows it.

The seeded runners give the agent these tools:

- `runners/claude`: web search, web fetch, Bash, and the file tools. Bash runs in Claude Code's sandbox: it writes only in the cwd and has no network, so the agent uses web fetch for the web. The file tools write only in the cwd, and read anywhere. `dreams` runs outside the sandbox, so it can write the vault. On Linux, the sandbox needs `bubblewrap` and `socat`.
- `runners/codex`: web search, and a shell in Codex's `workspace-write` sandbox: it writes only in the cwd and has no network.
- `runners/pi`: Pi's own tools. Pi has no web tools without extensions, and no sandbox.

All of them can read and write the vault over MCP. A vault made before these tools gets them with `dreams restore-defaults`. A deployed task keeps the runner revision it has until you run `dreams task deploy` again.

### Managing tasks

```
dreams task list                 every task, its state here, last run, due or not
dreams task deploy [<id>]        run the current task and runner revisions here; asks first
dreams task disable <id>         pause the task on this vault; other vaults are not affected
dreams task check <id>           one task in detail, plus the command it would run
dreams task run <id> [--force]   fire now; --force also when another run holds the task
dreams task runs <id>            past runs from every vault, newest first
dreams task rm <id>              delete the task; its runs stay
```

An edit to a task or to its runner, made here or synced from a peer, runs only after the next `task deploy`. `task deploy` with no id deploys every task that is not deployed at its current revisions: tasks with edits, disabled tasks, and dormant tasks from peers. It asks once for all of them. A redeploy keeps the task's schedule and pending changes. `--yes` deploys without asking.

`task add --no-deploy` writes the task but does not deploy it here. `task run` on a dormant task fires its current revision once and leaves it dormant. `task add` on an existing id replaces the template; the new revision runs after a deploy. An identical re-add writes nothing. `task rm` deletes the task document, so after a sync the task stops on every vault. A task with sync conflicts cannot deploy until you resolve them.

### Runners

A runner is a document typed `doc://schemas/runner` with the command that starts an agent. The command is an argument list, spawned without a shell. Inside each argument, `{db}`, `{task}`, `{run}`, `{mcp}`, `{out}`, and `{exe}` are replaced. The prompt goes to stdin. The last message is read from stdout, or from the `{out}` file when the command wrote one. `timeout` defaults to `10m`, after which the command is killed and the run records the timeout.

Add your own, for example a cheaper model for frequent tasks:

```
dreams runner add runners/claude-fast --timeout 5m -- claude -p --model claude-sonnet-5 --permission-mode dontAsk
dreams runner rm runners/pi
```

Runner commands are code. They enter only through the CLI, or from a vault that you [sync](#what-replicates) with, and a runner revision runs only after you confirm it in a deploy. An agent can choose a runner for a task; it cannot define or change one. Deleted defaults stay deleted. `doc resolve --auto` also uses runners.

The command inherits these variables: `DREAMS_DB`, `DREAMS_TASK`, `DREAMS_RUN`, `DREAMS_ACTOR` (the task id), `DREAMS_MCP` (a generated MCP config for this vault), and `DREAMS_OUT`. `PATH` starts with the directory of this binary.

### Runs

Each firing writes a receipt when the agent finishes: a document typed `doc://schemas/run` at `runs/<task id>/<UUID v7>.md`, tagged with the task id. It records `task` and `runner` (the pinned references of the task and runner revisions that ran), `vault` (the id of the vault that ran it), `started_at`, `finished_at`, `exit_code`, `error`, and the agent's last message as `content`. Receipts replicate, so `task runs` shows runs from every vault.

The schedule itself is local to the vault. Before the agent starts, the scheduler takes a lease on the task for the runner's timeout plus one minute. When the agent finishes, the receipt is written, the task's change cursor moves, and the lease is released, all in one transaction. If a run is cut off by a crash, it writes no receipt. The lease expires, and the task fires again with the same changes.

### The clock

```
dreams tick                        one pass, then exit
dreams daemon                      keep ticking; every --interval (60s) and sooner when the database changes
dreams daemon install | uninstall  start it at login (launchd on macOS, systemd on Linux)
```

`daemon install` writes the service with your current `PATH`. Agents use their own stored logins; nothing else is copied. Logs go to `~/Library/Logs/dreams/<name>.log` on macOS and to `journalctl --user -u dreams-<name>` on Linux. Set `RUST_LOG=debug` for more. Two schedulers on one database do no harm: the first one takes the lease before the agent starts, so the second one sees the task as running and skips it.

### When something goes wrong

- `task check <id>` shows the command. Copy it and run it by hand with the prompt on stdin.
- `task run <id>` exits 1 and prints the run when the agent fails. The run's `error` holds the exit code and the last lines of stderr.
- An agent that answers "not logged in" needs its own login: `claude`, `codex login`, or `pi`. The daemon has no terminal to ask you.
- A run with empty `content` and exit 0 from Codex means Codex could not reach its API. It exits 0 either way.

## MCP

`dreams serve` speaks the stateless MCP protocol, version 2026-07-28, over stdio. Older protocol versions are refused. The host starts the binary as a child process and talks to it through its stdin and stdout. Logs go to stderr, controlled by `RUST_LOG`.

### Use with Claude Code

Register the server. `mcp-json` prints the server entry for the vault that `--db` names, with absolute paths, because the host does not start the server in your project directory.

```
claude mcp add-json -s user dreams "$(dreams --db ~/.dreams/vault.db mcp-json)"
```

`-s user` makes the server available in all your projects. Leave it out to add the server to the current project only. To record writes under an agent name, add `--actor claude` before `mcp-json`. Other hosts take the same entry under `mcpServers` in their config file.

Then turn on protocol negotiation in Claude Code. Without it, Claude Code does not connect to stdio servers that speak 2026-07-28, and reports "Unsupported protocol version". Claude Code itself must have the variable, so `-e` does not work here. Add it to `~/.claude/settings.json`:

```json
{ "env": { "MCP_PROTOCOL_NEGOTIATION": "auto" } }
```

Or export `MCP_PROTOCOL_NEGOTIATION=auto` in your shell profile.

To check, start Claude Code and run `/mcp`. `dreams` shows as connected, and its tools have names like `mcp__dreams__list_docs`.

### Tools

Each store operation is one tool:

| Tool | Does |
|---|---|
| `put_doc` | Create, or update with `_parent` set to the current `_rev`. Takes `_id`, `_parent`, `_type`, and the fields in `body`, a JSON object. |
| `get_doc` | A document by `href`: a bare id or `doc://<id>` gives the current revision, `doc://<id>?rev=<rev>` gives that revision. `deleted_conflicts: true` adds `_deleted_conflicts` (unpinned only). |
| `get_rev` | One revision by `rev`. |
| `delete_doc` | Tombstone with `id` and `parent`. |
| `list_conflicts` | Documents with conflicts, in id order, with `after` and `limit`. |
| `resolve_doc` | Tombstone every conflict of `id`, after it writes `merged` on the winner if given. Pass the `conflicts` you read to fail if they changed. |
| `list_docs` | Current documents, newest first, with `type`, `tag`, `before`, `limit`. |
| `search_docs` | Full-text search with `query` and the same filters. Each result has `_id`, `_rev`, `_type`, `_created_at`, `_actor`, `title`, and `content_matches`. |
| `doc_history` | Revisions of one document, newest first. |
| `changes` | Every revision after `since`. |

Results are structured JSON. A store error returns as an invalid params error with the error object as its data. Schemas and scheduled tasks need no extra tools. An agent puts a schema document and references it as `_type: doc://<id>`. It writes a task with `put_doc`, finds runners with `list_docs` and `type: doc://schemas/runner`, and reads runs the same way. Writes to runner, run, and seeded schema documents are refused. Pull and sync have no tools. See [Conflicts over MCP](#conflicts-over-mcp) for resolving.

### Skills

A document typed `doc://schemas/skill` is a skill. The server gives skills to the host through the [MCP Skills Extension](https://modelcontextprotocol.io/extensions/skills/overview) (`io.modelcontextprotocol/skills`). The body needs three fields:

- `name`: lowercase letters, digits, and single hyphens, 64 characters or less.
- `description`: what the skill does and when to use it, 1024 characters or less.
- `content`: the instructions, in Markdown.

```
dreams doc put - <<'EOF'
{"_id": "skills/git-workflow", "_type": "doc://schemas/skill",
 "name": "git-workflow", "description": "Branch, commit, and open a PR.",
 "content": "# Steps\n1. Make a branch first.\n"}
EOF
```

The server shows each skill as one file, `skill://<name>/SKILL.md`. The file has `name` and `description` as frontmatter, then `content`. `skills/list` and `skills/get` return it, and `resources/read` reads it. `resources/list` also lists it, for hosts that do not know the extension. When two documents have the same `name`, the most recently changed one wins. Agents can write skills with `put_doc`. The `schemas/skill` document itself is read-only over MCP.

### Prompts

A document typed `doc://schemas/prompt` is a prompt. The server gives prompts to the host as [MCP prompts](https://modelcontextprotocol.io/specification/2025-06-18/server/prompts). Claude Code shows each one as a slash command, `/dreams:<name>`. The body needs three fields, with the same rules as a skill:

- `name`: lowercase letters, digits, and single hyphens, 64 characters or less.
- `description`: what the prompt does, 1024 characters or less.
- `content`: the instructions. The server sends them as one user message.

```
dreams doc put - <<'EOF'
{"_id": "prompts/standup", "_type": "doc://schemas/prompt",
 "name": "standup", "description": "Summarize yesterday's daily note.",
 "content": "Use the daily-note skill. Summarize yesterday's note as three bullets."}
EOF
```

A prompt declares no arguments. The user's text comes with the user's own input. In Claude Code, the model sees `/dreams:daily buy milk` as the command and its full text. Claude Code splits declared arguments on whitespace and drops extra words, so a declared argument would lose text. There is no templating. When two documents have the same `name`, the most recently changed one wins. The host learns about new prompts without a reconnect; see [Change notifications](#change-notifications). The `schemas/prompt` document itself is read-only over MCP.

### Resources

Every current document is a resource at `doc://<id>`. `resources/read` returns it as Markdown with YAML frontmatter, the same text as `dreams doc get --format md`. `doc://<id>?rev=<rev>` reads one revision. A deleted document is not found. `resources/list` lists every current document, most recently changed first, 1000 per page, with its `title`. The first page also lists the skills. The template `doc://{+id}` tells hosts that they can read any id, also ids that are not listed. In Claude Code, type `@` to find a document.

### Change notifications

A host that opens a `subscriptions/listen` stream gets notifications. While the stream is open, the server checks the vault once each second. Writes from any source count: MCP tools, the CLI, other processes, and sync.

| Change | Notification |
|---|---|
| A prompt is added, removed, or edited | `notifications/prompts/list_changed` |
| A document is created, deleted, or revived, or its `title` changes | `notifications/resources/list_changed` |
| A skill is added or removed, or its name, description, or size changes | `notifications/resources/list_changed` |
| A document or skill gets new content, and the host subscribed to its URI | `notifications/resources/updated` |

A write that changes nothing a host sees sends nothing. A deleted resource sends only `list_changed`. A pinned `?rev=` URI never changes. Claude Code asks for the two `list_changed` notifications. It does not subscribe to single resources.

### Daily notes

Every vault is seeded with one skill, `skills/daily-note` (`skill://daily-note/SKILL.md`). A daily note is a document typed `doc://schemas/daily`. Its `_id` is the local date as `YYYY-MM-DD.md`, and it has the tag `daily`. `content` is the log for the day. `intention` is the one intention for the day, and a new one replaces the old one. The skill tells the agent how to create today's note, add to it with `_parent`, set the intention, and find old notes with `list_docs` and `tag: daily`. Edit the skill document to change how your agent writes notes. `dreams init` and `dreams restore-defaults` replace the edit with the default.

Two seeded prompts use the skill: `/dreams:daily <text>` adds text to today's note, and `/dreams:intention <text>` sets today's intention.

### Daily brief

A brief is a short page of food for thought for the day. It brings back ideas from your own notes, with today's intention as its theme. If there is no intention, the agent finds a theme in your recent notes. A brief has four sections:

- **Theme**: the theme, and where it came from.
- **Review**: excerpts from 3 notes that relate to the theme, with links.
- **Prompt**: one provocation to find new ideas, in the style of Oblique Strategies, SCAMPER, or the questions at the end of a textbook chapter.
- **Collider**: a draft for a new note that joins two far-apart notes, found with the Zettelkasten Compass, and a prompt to continue it.

The seeded skill `skills/brief` (`skill://brief/SKILL.md`) tells the agent how to make a brief. Two seeded documents use it:

- `/dreams:brief` makes today's brief and shows it. It writes nothing.
- The task `tasks/brief` makes a brief once a day and adds it to the end of today's daily note, under `## Brief`. If the note already has a `## Brief` heading, the run stops.

Like every task, `tasks/brief` is dormant until you deploy it. Deploy it on one vault only, because each vault that deploys it writes a brief. `every: 1d` counts from the time of deploy, so deploy it at the time of day that you want the brief:

```
dreams task deploy tasks/brief
```

## Storage

One SQLite file in WAL mode. Migrations run on open.

- `docs` holds one row per revision. Triggers refuse updates and deletes, and enforce the parent chain.
- `checkpoints` holds the position of the last pull from each peer, and the peer's revision at that position.
- The winner of each document is chosen by one view, `docs_winners`, with the rule in [Conflicts](#conflicts). Copied revisions enter `docs` through the same triggers as local writes.
- `doc_heads`, `doc_tags`, and `docs_fts` are projections of each document's current revision. One trigger keeps them in step on every write.
- Schemas are documents. Seeding writes the built-in documents: `schemas/task`, `schemas/run`, `schemas/runner`, `schemas/skill`, `schemas/prompt`, `schemas/daily`, the three default runners, the `skills/daily-note` and `skills/brief` skills, the `prompts/daily`, `prompts/intention`, and `prompts/brief` prompts, and the dormant `tasks/brief` task.
- Seeding writes each built-in document whose current revision is different from the default, as the next revision. It revives deleted ones. The earlier revisions stay in history.
- `dreams init` and `dreams restore-defaults` seed. Any other command seeds only when it creates the database. Between seeds, a built-in document that you edit or delete stays as you left it. Run `dreams restore-defaults` after an edit goes wrong, or to get the defaults of a newer binary. It replaces your edits to the built-in documents.

Search uses FTS5 with the porter tokenizer. Title matches rank highest, then tags, then content. A search result does not contain the document body. It contains the metadata, the title, and `content_matches`: a snippet of the field that matches best, with the matched terms in `**`. An empty query lists the documents newest first, with no `content_matches`.

## Development

```
cargo test
cargo clippy --all-targets
cargo fmt --check
```

The tests cover the revision model, the store, the scheduler rules, and the full command surface. CLI tests call the command runner in process with a temporary database, and drive the scheduler with runners such as `cat` and `sleep`.
