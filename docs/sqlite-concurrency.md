# SQLite concurrency

This document tells what happens when two agents write to one vault at the same time. An example is two agent shell tabs.

## Two shell tabs are two processes

Each agent tab starts its own `dreams` process, for MCP or the CLI. Each process opens its own connection to the same vault file. SQLite locks the database file across processes, so the operating system coordinates the writers. No shared memory is necessary.

## Configuration

`configure` in `src/db.rs` sets these values:

- `journal_mode=WAL`: many readers and one writer can work at the same time. Readers do not block the writer, and the writer does not block readers.
- `busy_timeout(5s)`: when a writer finds the write lock taken, it waits and tries again for up to 5 seconds. It does not fail immediately.

All writes use `BEGIN IMMEDIATE`: `put_draft`, `put_if`, `resolve`, and `apply_replicas` in `src/store.rs`, and `migrate` in `src/db.rs`. This gets the write lock at the start of the transaction. It prevents the usual WAL problem, where a read transaction tries to become a write transaction and gets `SQLITE_BUSY` at once, with no retry.

## Two writes at the same time

1. Tab A gets the write lock and commits. This takes milliseconds.
2. Tab B waits in the busy handler, then gets the lock and commits.
3. Tab B gets a `SQLITE_BUSY` ("database is locked") error only if Tab A holds the lock for more than 5 seconds. Dreams writes are small, so this is unlikely.

## Two edits to the same document

SQLite makes the writes serial, and the revision check stops lost updates. Each update must name a current leaf as `_parent` (`put_draft_in` in `src/store.rs`). The check runs inside the IMMEDIATE transaction. The example below shows the result:

- A and B both read rev `1-x`.
- A writes with `_parent: 1-x` and succeeds.
- B writes with `_parent: 1-x`. `1-x` is not a leaf now, so B gets a `Conflict` error. B must read the document again and retry.

Two branches of one document can exist in the vault only after a sync from a different vault (`apply_replicas`). You then use `resolve` to merge them. In one local vault, compare-and-set stops the second edit before it becomes a branch.

## Threads in one process

The MCP server keeps one connection in an `Arc<Mutex<Store>>` (`src/mcp.rs`), so its threads take turns. A rusqlite `Connection` is `Send` but not `Sync`, so Rust does not let two threads use one connection without a lock.

## Limits

- A network filesystem, for example NFS or some synced folders, can break SQLite file locks and WAL shared memory. Keep the vault on a local disk.
- The daemon polls `PRAGMA data_version` to see writes from other connections (`src/daemon.rs`). This is correct, but it adds a delay of one poll interval.
