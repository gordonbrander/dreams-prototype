# Proposal: the vault as a file system

Status: assessment only, 2026-09-23. Nothing is built. This file keeps the options and the tradeoffs.

## Question

Can other apps read and write the vault as files? The idea is to mount the SQLite database as a virtual file system. How involved is that?

## What a mount would look like

A document id is already a path. The Markdown export already gives each document a file body with front matter. So the file tree is a projection of the store, and it reuses code that exists.

| File operation | Store operation |
|---|---|
| list a directory | `list` with an id prefix |
| read a file | `get`, then Markdown render |
| write a file | `put` with `_parent` set to the head |
| delete a file | `delete` (tombstone) |
| rename a file | `put` under the new id, then `delete` the old id |

Two things fall out of this for free:

- Every save is a revision. The vault keeps file history.
- Every save hits the changes feed. A task with a `when` filter wakes when an editor touches a document. That is the real payoff.

## Options

### 1. FUSE with the `fuser` crate

Implement one trait: `lookup`, `getattr`, `readdir`, `open`, `read`, `write`, `create`, `unlink`, `rename`, `setattr`, `flush`.

On macOS the user must install macFUSE or FUSE-T. Classic macFUSE needs a kernel extension. Apple Silicon users must lower security in Recovery to load it. macFUSE has an FSKit backend on macOS 26 that runs in user space, and FUSE-T is kext-free, but both are an extra install that we do not control.

Effort: about 1000 to 1500 lines, plus the install story.

### 2. A userspace NFSv3 server, mounted by the OS client

Crates such as `nfsserve` and `nfs3_server` give a FUSE-like trait. The binary listens on localhost. macOS and Linux mount it with the built-in `mount` command, with no driver. FUSE-T works this way internally.

`subconscious mount <dir>` can start the server and run the `mount` command itself. `subconscious unmount` reverses it.

Effort: about 800 to 1200 lines for the trait. The install is one command we run. This is the best of the mount options.

### 3. A WebDAV server with `dav-server`

Finder, Windows and davfs2 all mount WebDAV. The trait is smaller than NFS. But the macOS WebDAV client is slow, caches hard, and writes many `._` files. Not recommended.

### 4. Two-way folder sync (the gloves)

`export` and `import` exist. A `sync` mode in the daemon would watch a real directory with the `notify` crate, and watch the vault with the `data_version` poll the daemon already has. Every app works. No driver, no mount.

Effort: about 300 to 500 lines.

Cost: a second copy of the state, and a reconcile rule for edits on both sides. This complects two stores. A mount keeps one source of truth.

## Problems common to every option

- Editors save with write-temp-then-rename. The rename must become put-new-then-delete-old, or we special-case temp names.
- Finder writes `.DS_Store` and `._*` files. We must refuse or hide them.
- Writes arrive in chunks. Buffer per open file and `put` once on flush or close.
- A pinned `_type` in front matter round-trips as is. An unpinned one re-pins on save. That already works.
- Non-Markdown bodies need a rule. The simple one: a `.json` file for a document with no Markdown body.
- Protected types and ids: a mount is a local, trusted client, like the CLI. A sync directory is too.

## Recommendation

If a mount is wanted, build option 2. It is one trait and one `mount` command, and it works on macOS today with no driver.

If the least machinery is wanted, build option 4 first. Both reuse the same projection code, so a sync can grow into a mount later without waste.

## Sources

- [nfsserve](https://github.com/xetdata/nfsserve)
- [nfs3_server](https://crates.io/crates/nfs3_server)
- [fuser](https://github.com/cberner/fuser)
- [macFUSE](https://macfuse.github.io/)
- [FUSE-T](https://www.fuse-t.org/)
- [dav-server](https://lib.rs/crates/dav-server)
