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
| `_type` | The id of a registered schema. Optional. |
| `_deleted` | `true` on a tombstone. |
| `_created_at` | When the revision was written. |
| `_seq` | The global sequence number of the revision. |

Everything else is the body. Three body fields are blessed:

- `title` must be a string.
- `content` must be a string. Markdown files put the text after the frontmatter here.
- `tags` must be an array of strings. Tags are part of the body, so they are versioned with it. An index makes lookup by tag fast.

### Revisions

Every write creates a new revision and keeps the old one. To update, name the current revision as `_parent`. If another write got there first, the write fails with a conflict. History is linear: one document has one current revision and one chain behind it.

Delete writes a tombstone. A later write on a tombstone revives the document. The change feed shows every revision, tombstones included, in the order they were committed.

### Schemas

A schema is a JSON Schema document with `$id`, `title`, and `description`. Register it once. Its `$id` becomes a `_type`. A document with that `_type` is validated on every write. Schemas are immutable: registering the same body again is a no-op, and a different body at the same id is an error.

```
subconscious schema register note.schema.yaml
subconscious schema list
```

## Command line

```
subconscious [--db PATH] [--json] <command>

  init                                            create the database if needed
  serve                                           serve MCP over stdio

  doc put     [FILE] [--format json|yaml|md]      create, or update when the input names a revision
  doc update  <id> [FILE] [--parent REV]          replace the body
  doc get     <id> [--rev REV]                    current revision, or one revision
  doc delete  <id> [--parent REV]                 write a tombstone
  doc list    [--type T] [--tag G] [--limit N] [--before SEQ]
  doc search  <query> [--type T] [--tag G] [--limit N]
  doc history <id> [--limit N]
  doc changes [--since SEQ] [--limit N]

  schema register [FILE] [--format json|yaml]
  schema get      <id>
  schema list

  export <dir> [--type T] [--tag G]               write current documents to <dir>/<_id>
  import <dir>                                    read every *.md file under <dir>
```

### Input

`FILE` is a path, or `-` for stdin. No file means stdin. The extension picks the format: `.json`, `.yaml` or `.yml`, and `.md` or `.markdown`. Stdin is JSON unless `--format` says otherwise.

A Markdown file is YAML frontmatter between two `---` lines, then the content. The frontmatter sets fields. The text after the closing line becomes `content`, byte for byte. Nothing is inferred from headings.

### Output

One document prints as Markdown with frontmatter. A list prints as a table. Pass `--json` to get the same JSON structures the MCP tools return. Errors are always a JSON object on stderr, and the exit code is 1.

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

## MCP

`subconscious serve` speaks the stateless MCP protocol, version 2026-07-28, over stdio. Older protocol versions are refused. The host starts the binary as a child process and talks to it through its stdin and stdout. Logs go to stderr, controlled by `RUST_LOG`.

To register it with Claude Code:

```
claude mcp add subconscious -- /path/to/subconscious --db /path/to/vault.db serve
```

Use absolute paths. The host does not start the server in your project directory.

Each store operation is one tool:

| Tool | Does |
|---|---|
| `put_doc` | Create, or update with `_parent` set to the current `_rev`. |
| `get_doc` | Current revision by `id`. |
| `get_rev` | One revision by `rev`. |
| `delete_doc` | Tombstone with `id` and `parent`. |
| `list_docs` | Current documents, newest first, with `type`, `tag`, `before`, `limit`. |
| `search_docs` | Full-text search with `query` and the same filters. |
| `doc_history` | Revisions of one document, newest first. |
| `changes` | Every revision after `since`. |
| `register_schema` | Register a JSON Schema. |
| `get_schema` | One schema by `id`. |
| `list_schemas` | Id, title, and description of each schema. |

Results are structured JSON. A store error returns as an invalid params error with the error object as its data.

## Storage

One SQLite file in WAL mode. Migrations run on open.

- `docs` holds one row per revision. Triggers refuse updates and deletes, and enforce the parent chain.
- `doc_heads`, `doc_tags`, and `docs_fts` are projections of each document's current revision. One trigger keeps them in step on every write.
- `schemas` holds registered schemas.

Search uses FTS5 with the porter tokenizer. Title matches rank highest, then tags, then content.

## Development

```
cargo test
cargo clippy --all-targets
```

The tests cover the revision model, the store, and the full command surface. CLI tests call the command runner in process with a temporary database.
