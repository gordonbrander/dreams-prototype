//! The document store: one write path (`put`), reads through `doc_heads`.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::db;
use crate::doc::{Doc, Draft, PutInput};
use crate::error::StoreError;
use crate::rev;
use crate::schema::{SchemaRegistry, SchemaSummary};

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 1000;
pub const MAX_HISTORY: usize = 10_000;

const DOC_COLS: &str = "d._local_seq, d._rev, d._id, d._parent, d._type, d._deleted, d.body, d._created_at";

pub struct Store {
    conn: Connection,
    schemas: SchemaRegistry,
}

/// Filters shared by `list` and `search`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ListQuery {
    /// Only docs with this `_type`.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<String>,
    /// Only docs whose current revision carries this tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Keyset cursor: only heads with a sequence below this (from a previous page's `next`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<i64>,
    /// Page size, 1..=1000. Default 50.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Page {
    pub docs: Vec<Doc>,
    /// Pass as `before` to fetch the next page. Absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Changes {
    /// Revisions in commit order, each carrying `_seq`.
    pub results: Vec<Doc>,
    /// Pass as `since` to continue.
    pub last_seq: i64,
}

fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

fn row_to_doc(row: &Row<'_>, with_seq: bool) -> rusqlite::Result<Doc> {
    let body_text: String = row.get(6)?;
    let body: Map<String, Value> = serde_json::from_str(&body_text).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e))
    })?;
    Ok(Doc {
        seq: if with_seq { Some(row.get(0)?) } else { None },
        rev: row.get(1)?,
        id: row.get(2)?,
        parent: row.get(3)?,
        type_id: row.get(4)?,
        deleted: row.get::<_, i64>(5)? != 0,
        body,
        created_at: row.get(7)?,
    })
}

/// Turn free text into a safe FTS5 query: each whitespace token becomes a
/// quoted phrase, so operators and punctuation cannot cause syntax errors.
pub fn fts_query(text: &str) -> Option<String> {
    let tokens: Vec<String> = text
        .split_whitespace()
        .map(|t| t.replace('"', ""))
        .filter(|t| !t.is_empty())
        .map(|t| format!("\"{t}\""))
        .collect();
    if tokens.is_empty() { None } else { Some(tokens.join(" ")) }
}

fn get_rev_in(conn: &Connection, rev_id: &str) -> Result<Doc, StoreError> {
    let sql = format!("SELECT {DOC_COLS} FROM docs d WHERE d._rev = ?1");
    conn.query_row(&sql, [rev_id], |r| row_to_doc(r, false))
        .optional()?
        .ok_or_else(|| StoreError::NotFound { id: rev_id.to_string() })
}

fn head_in(conn: &Connection, id: &str) -> Result<Option<Doc>, StoreError> {
    let sql = format!("SELECT {DOC_COLS} FROM doc_heads h JOIN docs d ON d._local_seq = h.seq WHERE h._id = ?1");
    Ok(conn.query_row(&sql, [id], |r| row_to_doc(r, false)).optional()?)
}

fn leaves_in(conn: &Connection, id: &str) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT d._rev FROM docs d WHERE d._id = ?1
           AND NOT EXISTS (SELECT 1 FROM docs c WHERE c._parent = d._rev)",
    )?;
    let rows = stmt.query_map([id], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

impl Store {
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        Ok(Store {
            conn: db::open(path)?,
            schemas: SchemaRegistry::default(),
        })
    }

    pub fn open_in_memory() -> Result<Store, StoreError> {
        Ok(Store {
            conn: db::open_in_memory()?,
            schemas: SchemaRegistry::default(),
        })
    }

    // ---- writes -------------------------------------------------------

    /// Create or update a document. Idempotent by content: replaying the
    /// same input returns the same revision.
    pub fn put(&mut self, input: PutInput) -> Result<Doc, StoreError> {
        let draft = input.into_draft()?;
        self.put_draft(draft)
    }

    /// Write a tombstone as a child of `parent`. The tombstone keeps the
    /// parent's `_type`. Undelete by `put` with `_parent` = the tombstone.
    pub fn delete(&mut self, id: &str, parent: &str) -> Result<Doc, StoreError> {
        let parent_doc = get_rev_in(&self.conn, parent)?;
        if parent_doc.id != id {
            return Err(StoreError::invalid(format!("revision {parent} belongs to {}, not {id}", parent_doc.id)));
        }
        if parent_doc.deleted {
            return Err(StoreError::Deleted { id: id.to_string(), rev: parent.to_string() });
        }
        self.put_draft(Draft {
            id: id.to_string(),
            parent: Some(parent.to_string()),
            type_id: parent_doc.type_id,
            deleted: true,
            body: Map::new(),
        })
    }

    fn put_draft(&mut self, draft: Draft) -> Result<Doc, StoreError> {
        let Store { conn, schemas } = self;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        if let Some(type_id) = &draft.type_id
            && !draft.deleted
        {
            schemas.validate(&tx, type_id, &Value::Object(draft.body.clone()))?;
        }

        let leaves = leaves_in(&tx, &draft.id)?;
        let rev_id = rev::rev_of(
            &draft.id,
            draft.parent.as_deref(),
            draft.type_id.as_deref(),
            draft.deleted,
            &draft.body,
        )?;

        if leaves.contains(&rev_id) {
            let existing = get_rev_in(&tx, &rev_id)?;
            tx.commit()?;
            return Ok(existing);
        }
        let parent_ok = match &draft.parent {
            None => leaves.is_empty(),
            Some(p) => leaves.contains(p),
        };
        if !parent_ok {
            return Err(StoreError::Conflict {
                id: draft.id,
                parent: draft.parent,
                leaves,
            });
        }

        tx.execute(
            "INSERT INTO docs (_rev, _id, _parent, _type, _deleted, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                rev_id,
                draft.id,
                draft.parent,
                draft.type_id,
                draft.deleted as i64,
                serde_json::to_string(&draft.body)?,
            ],
        )?;
        let stored = get_rev_in(&tx, &rev_id)?;
        tx.commit()?;
        Ok(stored)
    }

    // ---- reads --------------------------------------------------------

    /// The current revision of a document. A tombstone is reported as
    /// `Deleted` so callers can undelete from it.
    pub fn get(&self, id: &str) -> Result<Doc, StoreError> {
        match head_in(&self.conn, id)? {
            None => Err(StoreError::NotFound { id: id.to_string() }),
            Some(d) if d.deleted => Err(StoreError::Deleted { id: id.to_string(), rev: d.rev }),
            Some(d) => Ok(d),
        }
    }

    pub fn get_rev(&self, rev_id: &str) -> Result<Doc, StoreError> {
        get_rev_in(&self.conn, rev_id)
    }

    /// Revisions from the current head back to genesis, newest first.
    pub fn history(&self, id: &str, limit: Option<usize>) -> Result<Vec<Doc>, StoreError> {
        let limit = limit.unwrap_or(MAX_HISTORY).clamp(1, MAX_HISTORY) as i64;
        let sql = format!(
            "WITH RECURSIVE chain(_rev, _parent, n) AS (
               SELECT d._rev, d._parent, 1 FROM doc_heads h JOIN docs d ON d._local_seq = h.seq WHERE h._id = ?1
               UNION ALL
               SELECT d._rev, d._parent, n + 1 FROM chain JOIN docs d ON d._rev = chain._parent WHERE n < ?2)
             SELECT {DOC_COLS} FROM chain JOIN docs d ON d._rev = chain._rev ORDER BY n"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![id, limit], |r| row_to_doc(r, false))?;
        let docs = rows.collect::<Result<Vec<_>, _>>()?;
        if docs.is_empty() {
            return Err(StoreError::NotFound { id: id.to_string() });
        }
        Ok(docs)
    }

    /// Current, non-deleted documents, most recently modified first.
    pub fn list(&self, q: &ListQuery) -> Result<Page, StoreError> {
        let limit = clamp_limit(q.limit);
        let sql = match &q.tag {
            Some(_) => format!(
                "SELECT {DOC_COLS} FROM doc_tags t
                   JOIN doc_heads h ON h._id = t._id
                   JOIN docs d ON d._local_seq = h.seq
                  WHERE t.tag = ?2 AND (?1 IS NULL OR d._type = ?1) AND (?3 IS NULL OR h.seq < ?3)
                  ORDER BY h.seq DESC LIMIT ?4"
            ),
            None => format!(
                "SELECT {DOC_COLS} FROM doc_heads h JOIN docs d ON d._local_seq = h.seq
                  WHERE d._deleted = 0 AND (?1 IS NULL OR d._type = ?1) AND ?2 IS NULL
                    AND (?3 IS NULL OR h.seq < ?3)
                  ORDER BY h.seq DESC LIMIT ?4"
            ),
        };
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![q.type_id, q.tag, q.before, limit as i64], |r| row_to_doc(r, true))?;
        let mut docs = rows.collect::<Result<Vec<_>, _>>()?;
        let next = if docs.len() == limit { docs.last().and_then(|d| d.seq) } else { None };
        for d in &mut docs {
            d.seq = None;
        }
        Ok(Page { docs, next })
    }

    /// Full-text search over title, content and tags of current documents.
    /// An empty query degrades to `list`.
    pub fn search(&self, text: &str, q: &ListQuery) -> Result<Page, StoreError> {
        let Some(match_expr) = fts_query(text) else {
            return self.list(q);
        };
        let limit = clamp_limit(q.limit);
        let sql = format!(
            "SELECT {DOC_COLS} FROM docs_fts f JOIN docs d ON d._local_seq = f.rowid
              WHERE docs_fts MATCH ?1
                AND (?2 IS NULL OR d._type = ?2)
                AND (?3 IS NULL OR EXISTS (SELECT 1 FROM doc_tags t WHERE t.tag = ?3 AND t._id = d._id))
              ORDER BY bm25(docs_fts, 10.0, 1.0, 5.0) LIMIT ?4"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![match_expr, q.type_id, q.tag, limit as i64], |r| row_to_doc(r, false))?;
        let docs = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(Page { docs, next: None })
    }

    /// Every revision committed after `since`, in commit order.
    pub fn changes(&self, since: i64, limit: Option<usize>) -> Result<Changes, StoreError> {
        let limit = clamp_limit(limit);
        let sql = format!("SELECT {DOC_COLS} FROM docs d WHERE d._local_seq > ?1 ORDER BY d._local_seq LIMIT ?2");
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(params![since, limit as i64], |r| row_to_doc(r, true))?;
        let results = rows.collect::<Result<Vec<_>, _>>()?;
        let last_seq = results.last().and_then(|d| d.seq).unwrap_or(since);
        Ok(Changes { results, last_seq })
    }

    // ---- schemas ------------------------------------------------------

    pub fn register_schema(&mut self, schema: Value) -> Result<SchemaSummary, StoreError> {
        self.schemas.register(&self.conn, schema)
    }

    pub fn get_schema(&self, id: &str) -> Result<Value, StoreError> {
        self.schemas.get(&self.conn, id)
    }

    pub fn list_schemas(&self) -> Result<Vec<SchemaSummary>, StoreError> {
        self.schemas.list(&self.conn)
    }

    /// Raw connection, for tests and maintenance.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}
