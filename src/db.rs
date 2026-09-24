//! Connection setup and migrations.

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, TransactionBehavior};

/// Each entry is one migration, applied once, in order, inside its own
/// IMMEDIATE transaction. Append only; never edit an applied entry.
const MIGRATIONS: &[&str] = &[MIGRATION_1, MIGRATION_2, MIGRATION_3, MIGRATION_4, MIGRATION_5];

/// Local state that never replicates. `vault` holds this vault's id, one
/// row, made on first use. `task_state` is the schedule of each task this
/// vault runs: a task document with no row is dormant here. A row pins the
/// deployed task revision and runner reference; `cursor` is the change-feed
/// position the last run consumed; `lease_until` is set while a run holds
/// the task. A row lives only while its document is a live task: the
/// trigger drops it on a tombstone or a type change, local or replicated.
const MIGRATION_5: &str = r#"
CREATE TABLE vault (id TEXT NOT NULL);
CREATE TABLE task_state (
  task_id     TEXT PRIMARY KEY,
  task_rev    TEXT NOT NULL,
  runner      TEXT NOT NULL,
  enabled     INTEGER NOT NULL CHECK (enabled IN (0,1)),
  enabled_at  TEXT NOT NULL,
  cursor      INTEGER NOT NULL,
  last_run_at TEXT,
  lease_until TEXT
);
CREATE TRIGGER task_state_follows_doc AFTER INSERT ON docs BEGIN
  DELETE FROM task_state WHERE task_id = new._id
    AND NOT EXISTS (SELECT 1 FROM docs_winners w
                     WHERE w._id = new._id AND w._deleted = 0 AND w._type_path = 'doc://schemas/task');
END;
"#;

/// Replication checkpoints, one per source vault this vault pulls from.
/// `rev` is the source's revision at `seq`; a mismatch means the source was
/// replaced, and the next pull starts again from zero.
const MIGRATION_4: &str = r#"
CREATE TABLE checkpoints (
  peer TEXT PRIMARY KEY,
  seq  INTEGER NOT NULL,
  rev  TEXT NOT NULL
);
"#;

/// Who wrote the revision. Set by `serve --actor` and the CLI `--actor`
/// flag; a scheduled task's writes carry its id so the task does not wake
/// itself. Not part of the revision hash.
const MIGRATION_2: &str = "ALTER TABLE docs ADD COLUMN actor TEXT;";

/// Schemas became documents. `_type` is now a `doc://` reference, pinned
/// to one schema revision; `_type_path` is the reference without the pin,
/// so filters can match every revision of a schema.
const MIGRATION_3: &str = r#"
ALTER TABLE docs ADD COLUMN _type_path TEXT GENERATED ALWAYS AS
  (CASE WHEN instr(_type, '?rev=') > 0 THEN substr(_type, 1, instr(_type, '?rev=') - 1) ELSE _type END) VIRTUAL;
DROP TABLE schemas;
"#;

const MIGRATION_1: &str = r#"
-- One row per revision. Append-only.
CREATE TABLE docs (
  _local_seq  INTEGER PRIMARY KEY AUTOINCREMENT,
  _rev        TEXT NOT NULL UNIQUE,
  _id         TEXT NOT NULL,
  _parent     TEXT,
  _type       TEXT,
  _deleted    INTEGER NOT NULL DEFAULT 0 CHECK (_deleted IN (0,1)),
  body        TEXT NOT NULL CHECK (json_valid(body) AND json_type(body) = 'object'),
  _created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
  _rev_gen    INTEGER GENERATED ALWAYS AS (CAST(substr(_rev, 1, instr(_rev,'-') - 1) AS INTEGER)) STORED,
  _rev_hash   TEXT    GENERATED ALWAYS AS (substr(_rev, instr(_rev,'-') + 1)) STORED,
  -- Blessed scalars. VIRTUAL so content is not stored twice. Promoted only when the JSON type is right.
  title       TEXT GENERATED ALWAYS AS (CASE WHEN json_type(body,'$.title')   = 'text' THEN json_extract(body,'$.title')   END) VIRTUAL,
  content     TEXT GENERATED ALWAYS AS (CASE WHEN json_type(body,'$.content') = 'text' THEN json_extract(body,'$.content') END) VIRTUAL
);
CREATE INDEX docs_id_idx     ON docs(_id);
CREATE INDEX docs_parent_idx ON docs(_parent);

-- Append-only guards. Never use INSERT OR REPLACE / UPSERT on docs.
CREATE TRIGGER docs_no_update BEFORE UPDATE ON docs BEGIN SELECT RAISE(ABORT, 'docs rows are immutable'); END;
CREATE TRIGGER docs_no_delete BEFORE DELETE ON docs BEGIN SELECT RAISE(ABORT, 'docs rows are immutable'); END;

-- Rev chain: genesis is gen 1; otherwise parent exists, same _id, exactly one gen back.
CREATE TRIGGER docs_rev_chain BEFORE INSERT ON docs BEGIN
  SELECT RAISE(ABORT, 'genesis revision must be generation 1')
   WHERE new._parent IS NULL AND new._rev_gen <> 1;
  SELECT RAISE(ABORT, 'parent revision missing for this _id or generation mismatch')
   WHERE new._parent IS NOT NULL
     AND NOT EXISTS (SELECT 1 FROM docs p
                      WHERE p._rev = new._parent AND p._id = new._id
                        AND p._rev_gen + 1 = new._rev_gen);
END;

-- The CouchDB winner rule, defined once: a leaf, beaten by no other leaf of the same _id.
-- Non-deleted beats deleted, then highest gen, then lowest hash.
CREATE VIEW docs_winners AS
  SELECT d.* FROM docs d
  WHERE NOT EXISTS (SELECT 1 FROM docs c WHERE c._parent = d._rev)
    AND NOT EXISTS (
      SELECT 1 FROM docs n2
      WHERE n2._id = d._id
        AND NOT EXISTS (SELECT 1 FROM docs c2 WHERE c2._parent = n2._rev)
        AND ( n2._deleted < d._deleted
           OR (n2._deleted = d._deleted AND n2._rev_gen > d._rev_gen)
           OR (n2._deleted = d._deleted AND n2._rev_gen = d._rev_gen AND n2._rev_hash < d._rev_hash)));

-- Projections of the current winner. Source of truth is always docs.
CREATE TABLE doc_heads (
  _id TEXT PRIMARY KEY,
  seq INTEGER NOT NULL REFERENCES docs(_local_seq)
);
CREATE TABLE doc_tags (
  tag TEXT NOT NULL,
  _id TEXT NOT NULL REFERENCES doc_heads(_id) ON DELETE CASCADE,
  PRIMARY KEY (tag, _id)
) WITHOUT ROWID;
CREATE VIRTUAL TABLE docs_fts USING fts5(title, content, tags, tokenize = 'porter unicode61');

CREATE TRIGGER docs_project_ai AFTER INSERT ON docs BEGIN
  DELETE FROM docs_fts WHERE rowid = (SELECT seq FROM doc_heads WHERE _id = new._id);
  INSERT OR REPLACE INTO doc_heads(_id, seq)
    SELECT _id, _local_seq FROM docs_winners WHERE _id = new._id;
  DELETE FROM doc_tags WHERE _id = new._id;
  INSERT OR IGNORE INTO doc_tags(tag, _id)
    SELECT j.value, new._id
      FROM doc_heads h JOIN docs d ON d._local_seq = h.seq, json_each(d.body, '$.tags') j
     WHERE h._id = new._id AND d._deleted = 0
       AND json_type(d.body, '$.tags') = 'array' AND j.type = 'text';
  INSERT INTO docs_fts(rowid, title, content, tags)
    SELECT d._local_seq, d.title, d.content,
           CASE WHEN json_type(d.body, '$.tags') = 'array'
                THEN (SELECT group_concat(value, ' ') FROM json_each(d.body, '$.tags') WHERE type = 'text') END
      FROM doc_heads h JOIN docs d ON d._local_seq = h.seq
     WHERE h._id = new._id AND d._deleted = 0;
END;

-- Schema registry. Immutable per id.
CREATE TABLE schemas (
  id         TEXT PRIMARY KEY,
  schema     TEXT NOT NULL CHECK (json_valid(schema)),
  created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
"#;

/// Open (or create) the database file and bring it up to date.
pub fn open(path: &Path) -> rusqlite::Result<Connection> {
    let mut conn = Connection::open(path)?;
    configure(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

/// An in-memory database with the full schema, for tests.
pub fn open_in_memory() -> rusqlite::Result<Connection> {
    let mut conn = Connection::open_in_memory()?;
    configure(&conn)?;
    migrate(&mut conn)?;
    Ok(conn)
}

fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Virtual tables (FTS5) inside triggers require a trusted schema.
    conn.pragma_update(None, "trusted_schema", "ON")?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(())
}

pub fn migrate(conn: &mut Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS migrations (version INTEGER PRIMARY KEY, applied_at TEXT NOT NULL)",
    )?;
    for (i, sql) in MIGRATIONS.iter().enumerate() {
        let version = i as i64 + 1;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let applied: bool =
            tx.query_row("SELECT EXISTS(SELECT 1 FROM migrations WHERE version = ?1)", [version], |r| r.get(0))?;
        if !applied {
            tx.execute_batch(sql)?;
            tx.execute(
                "INSERT INTO migrations(version, applied_at) VALUES (?1, strftime('%Y-%m-%dT%H:%M:%fZ','now'))",
                [version],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent() {
        let mut conn = open_in_memory().unwrap();
        migrate(&mut conn).unwrap();
        let n: i64 = conn.query_row("SELECT count(*) FROM migrations", [], |r| r.get(0)).unwrap();
        assert_eq!(n as usize, MIGRATIONS.len());
    }

    #[test]
    fn migration_3_adds_type_path_and_drops_schemas() {
        let conn = open_in_memory().unwrap();
        let hidden: i64 = conn
            .query_row("SELECT hidden FROM pragma_table_xinfo('docs') WHERE name = '_type_path'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(hidden, 2, "virtual generated column");
        let schemas: i64 =
            conn.query_row("SELECT count(*) FROM sqlite_master WHERE name = 'schemas'", [], |r| r.get(0)).unwrap();
        assert_eq!(schemas, 0);
        conn.execute_batch(
            "INSERT INTO docs(_rev,_id,_type,body) VALUES ('1-a','x','doc://s?rev=1-b','{}'), ('1-c','y',NULL,'{}')",
        )
        .unwrap();
        let path: String = conn.query_row("SELECT _type_path FROM docs WHERE _id='x'", [], |r| r.get(0)).unwrap();
        assert_eq!(path, "doc://s");
        let none: Option<String> =
            conn.query_row("SELECT _type_path FROM docs WHERE _id='y'", [], |r| r.get(0)).unwrap();
        assert_eq!(none, None);
    }

    #[test]
    fn migration_4_adds_checkpoints() {
        let conn = open_in_memory().unwrap();
        conn.execute("INSERT INTO checkpoints(peer, seq, rev) VALUES ('/a.db', 3, '1-ab')", []).unwrap();
        let seq: i64 = conn.query_row("SELECT seq FROM checkpoints WHERE peer = '/a.db'", [], |r| r.get(0)).unwrap();
        assert_eq!(seq, 3);
    }

    #[test]
    fn migration_5_adds_vault_and_task_state() {
        let conn = open_in_memory().unwrap();
        conn.execute("INSERT INTO vault(id) VALUES ('v')", []).unwrap();
        let row = "INSERT INTO task_state(task_id, task_rev, runner, enabled, enabled_at, cursor)
                   VALUES (?1, '1-a', 'doc://r?rev=1-b', ?2, 'now', 0)";
        assert!(conn.execute(row, rusqlite::params!["bad", 2]).is_err());
        let state = |id: &str| -> i64 {
            conn.query_row("SELECT count(*) FROM task_state WHERE task_id = ?1", [id], |r| r.get(0)).unwrap()
        };
        // a live task keeps its row; a tombstone or a type change drops it
        conn.execute_batch(
            "INSERT INTO docs(_rev,_id,_type,body) VALUES ('1-t','t','doc://schemas/task?rev=1-s','{}'),
                                                        ('1-u','u','doc://schemas/task?rev=1-s','{}');",
        )
        .unwrap();
        conn.execute(row, rusqlite::params!["t", 1]).unwrap();
        conn.execute(row, rusqlite::params!["u", 1]).unwrap();
        conn.execute("INSERT INTO docs(_rev,_id,_parent,_type,body) VALUES ('2-t','t','1-t','doc://schemas/task?rev=1-s','{\"a\":1}')", []).unwrap();
        assert_eq!(state("t"), 1);
        conn.execute("INSERT INTO docs(_rev,_id,_parent,_deleted,_type,body) VALUES ('3-t','t','2-t',1,'doc://schemas/task?rev=1-s','{}')", []).unwrap();
        assert_eq!(state("t"), 0);
        conn.execute("INSERT INTO docs(_rev,_id,_parent,body) VALUES ('2-u','u','1-u','{}')", []).unwrap();
        assert_eq!(state("u"), 0);
    }
}
