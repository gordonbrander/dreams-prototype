# Technical specs

## Tech stack

- Language: Typescript
  - Rust components?
- Runtime: Node
- Bundler: Vite or ESBuild?
- DB: SQLite or [DialogDB](https://github.com/dialog-db/dialog-db)

Prototype approach:
- Pi plugin

## Architecture

- Event sourced
  - Pro: Event log allows agents to recover from failure by looking back at log
  - Pro: easy to build multiple projections/UIs on top
  - Track causality (automatically assign `prev` ID to event based on current HEAD at write time).
    - Allows deterministic conflict resolution when merging event logs
  - **Commands** (unicast requests / "please do x") and **events** (multicast facts / "x happend")
- Named queues (backed by DB table)
  - FIFO
  - Queryable
  - At-least-once delivery
  - SQS-style timeout lease system
- LLM wiki
  - Versioned JSON
  - CouchDB-like version/conflict resolution semantics
  - Optimistic concurrency
  - Read-before-write guard for client<->server synce
    - Forces agents to read and merge conflicts
- Content-addressed store
  - Structural sharing between fat events and projections
    - E.g. `content` field is a cid in event and in wiki entry
- Agents as actors
  - Actor-like system built on top of event sourced log
    - Messages are a type of command
  - Agents have an **address**
  - Agents have a **sendMessage** tool that takes an address
      - Tool automatically tracks `correlationId`, `causationId`, `depth`
  - Agents have a **spawnAgent** tool
      - Takes a template
      - Records address, adds to their context automatically
      - `{ to: string, replyTo?: string, message, data: Record<string, unknown> }`
          - Automatically expanded to `{ id, type, from, to, replyTo, message, data, time, correlationId, causationId, depth }` with publish tool expanding metadata and threading causality.
      - Subagents only live as long as their session
          - Can be sent steering messages to kill
  - Name system: Name -> Address
    - E.g. `@subsconscious -> abc123xyz...`
    - Local human-readable names (DNS-like)
    - Globally-unique addresses (UUID or similar)
    - Agent frontmatter can be configured with well-known addresses

## Sync

Distinguish between shared state and local state:

- Shared state (needs to sync)
  - LLM wiki
    - Agent templates
    - Skills
    - Notes
    - Etc
  - Messages
  - Memories
- Local state (doesn't need to sync)
  - Task queues
  - Cron jobs
  - Agent instances

## Event-sourcing

Optional. Need to work out eventual consistency semantics.

`events` table as Typescript schema:

```typescript
type Event = {
  id: string; // ULID
  prev?: string; // ULID
  kind: "command" | "event";
  type: string;
  from: string; // Address
  to?: string; // Address
  replyTo?: string; // Address
  message: string; // Human-friendly message
  data: Record<string, unknown>; // Structured data
  correlationId: string; // ULID
  causationId: string; // ULID
  depth: number; // Cycle detection. Always incremented by 1.
}
```

Notes:
- Log both commands and events
- Fat events: state must be derivable from event log
- ID: ULID or UUID v7?
  - Prefer UUID v7. Native support in Postgres 18.
- Causality data automatically threaded by the system (not the agent
  - Agent `sendMessage` tool bound to thread `{ cause: event }` through

## LLM wiki

Assuming a CouchDB-shaped record (vs DialogDB)...

```typescript
type Doc = {
  _id: string; // UUID. Globally-unique reference.
  _rev: string; // CouchDB-style rev.
  _schema: string; // URI e.g. `urn:dreams:schema:g:agent/v1`
  _path: string; // path string "foo/bar/baz". Locally unique address
  title?: string;
  content: string;
  [key: string]: unknown; // Arbitrary other properties
}
```

Notes:
- Must serialize to Markdown with frontmatter. Content is body. Everything else is frontmatter.

Open questions:
- Are paths better handled via name system (e.g. petnames)?
  - If so, `_path` could be interpreted as "preferred path"
