//! The document store: one write path (`put`), reads through `doc_heads`.

use std::collections::HashMap;
use std::path::Path;

use jsonschema::Validator;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::db;
use crate::doc::{Doc, DocRef, Draft, PutInput, check_body, check_id, new_id};
use crate::error::StoreError;
use crate::rev;
use crate::schema;

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 1000;
pub const MAX_HISTORY: usize = 10_000;

pub(crate) const DOC_COLS: &str =
    "d._local_seq, d._rev, d._id, d._parent, d._type, d._deleted, d.body, d._created_at, d.actor";

/// The timestamp format every stored time uses (`_created_at`, run times).
pub const TIME_FORMAT: &str = "%Y-%m-%dT%H:%M:%fZ";

/// Compiled validators keyed by pinned schema reference. A pinned
/// reference names immutable content, so an entry never goes stale.
type Validators = HashMap<String, Validator>;
const MAX_VALIDATORS: usize = 256;

pub struct Store {
    conn: Connection,
    validators: Validators,
    /// Written into `docs.actor` on every revision this store creates.
    actor: Option<String>,
    /// Type paths this connection may not write or delete.
    protected_types: Vec<String>,
    /// Ids this connection may not write or delete.
    protected_ids: Vec<String>,
}

/// Filters shared by `list` and `search`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct ListQuery {
    /// Only docs with this `_type`: a `doc://` reference. Without `?rev=` it
    /// matches every pinned revision of that schema.
    #[serde(rename = "type", default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<String>,
    /// Only docs whose current revision carries this tag.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,
    /// Only docs whose `_id` starts with this text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prefix: Option<String>,
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

/// One search hit: a current document's metadata and where the query matched.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchResult {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_rev")]
    pub rev: String,
    #[serde(rename = "_type", default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<String>,
    #[serde(rename = "_created_at")]
    pub created_at: String,
    #[serde(rename = "_actor", default, skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Snippet of the best-matching field, matched terms in `**`. Absent
    /// when an empty query listed instead.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_matches: Option<String>,
}

impl From<Doc> for SearchResult {
    fn from(d: Doc) -> Self {
        let title = d.body.get("title").and_then(Value::as_str).map(str::to_string);
        SearchResult {
            id: d.id,
            rev: d.rev,
            type_id: d.type_id,
            created_at: d.created_at,
            actor: d.actor,
            title,
            content_matches: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SearchPage {
    pub results: Vec<SearchResult>,
    /// Pass as `before` to fetch the next page. Set only when an empty query
    /// listed and more remain.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct History {
    /// Revisions from the current head back to genesis, newest first.
    pub revisions: Vec<Doc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Changes {
    /// Revisions in commit order, each carrying `_seq`.
    pub results: Vec<Doc>,
    /// Pass as `since` to continue.
    pub last_seq: i64,
}

/// One document with conflicts: its winner and the other live leaves.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Conflicted {
    pub id: String,
    /// The winning revision.
    pub rev: String,
    pub conflicts: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConflictPage {
    /// Documents with conflicts, in id order.
    pub docs: Vec<Conflicted>,
    /// Pass as `after` to fetch the next page. Absent on the last page.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next: Option<String>,
}

/// Split a type filter into (exact, path) so one SQL string serves both:
/// `AND (?a IS NULL OR d._type = ?a) AND (?b IS NULL OR d._type_path = ?b)`.
pub(crate) fn type_filter(t: Option<&str>) -> (Option<&str>, Option<&str>) {
    match t {
        None => (None, None),
        Some(t) if t.contains("?rev=") => (Some(t), None),
        Some(t) => (None, Some(t)),
    }
}

fn clamp_limit(limit: Option<usize>) -> usize {
    limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT)
}

pub(crate) fn row_to_doc(row: &Row<'_>, with_seq: bool) -> rusqlite::Result<Doc> {
    let body_text: String = row.get(6)?;
    let body: Map<String, Value> = serde_json::from_str(&body_text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(6, rusqlite::types::Type::Text, Box::new(e)))?;
    Ok(Doc {
        seq: if with_seq { Some(row.get(0)?) } else { None },
        rev: row.get(1)?,
        id: row.get(2)?,
        parent: row.get(3)?,
        type_id: row.get(4)?,
        deleted: row.get::<_, i64>(5)? != 0,
        body,
        created_at: row.get(7)?,
        actor: row.get(8)?,
        conflicts: Vec::new(),
        deleted_conflicts: Vec::new(),
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

/// Leaves of `id` other than `winner`, live or tombstoned: the CouchDB
/// `_conflicts` and `_deleted_conflicts`. Highest generation first.
fn other_leaves_in(conn: &Connection, id: &str, winner: &str, deleted: bool) -> Result<Vec<String>, StoreError> {
    let mut stmt = conn.prepare_cached(
        "SELECT d._rev FROM docs d WHERE d._id = ?1 AND d._deleted = ?3 AND d._rev <> ?2
           AND NOT EXISTS (SELECT 1 FROM docs c WHERE c._parent = d._rev)
         ORDER BY d._rev_gen DESC, d._rev_hash",
    )?;
    let rows = stmt.query_map(params![id, winner, deleted as i64], |r| r.get::<_, String>(0))?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn conflicts_in(conn: &Connection, id: &str, winner: &str) -> Result<Vec<String>, StoreError> {
    other_leaves_in(conn, id, winner, false)
}

fn rev_exists_in(conn: &Connection, rev_id: &str) -> Result<bool, StoreError> {
    Ok(conn.query_row("SELECT EXISTS(SELECT 1 FROM docs WHERE _rev = ?1)", [rev_id], |r| r.get(0))?)
}

/// A tombstone draft on `parent`. It keeps the parent's pinned type.
fn tombstone_draft(parent: &Doc) -> Result<Draft, StoreError> {
    let type_ref = parent.type_id.as_deref().map(DocRef::parse).transpose()?;
    Ok(Draft { id: parent.id.clone(), parent: Some(parent.rev.clone()), type_ref, deleted: true, body: Map::new() })
}

/// What happened to one replicated revision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
    Written,
    /// This vault already had the revision.
    Present,
    /// The revision's parent is not in this vault, because an ancestor was
    /// not replicated. Skipped.
    MissingParent,
}

/// Counts for one batch of replicated revisions.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, JsonSchema)]
pub struct BatchReport {
    pub written: usize,
    pub present: usize,
    pub missing_parent: usize,
}

/// Insert a revision written by another vault, as it is. The content must
/// hash to its `_rev`. No leaf check, no pinning, no schema validation: the
/// hash proves the content, and the pin names the exact schema bytes.
fn insert_replica_in(tx: &Connection, doc: &Doc) -> Result<Applied, StoreError> {
    check_id(&doc.id)?;
    check_body(&doc.body)?;
    if doc.deleted && !doc.body.is_empty() {
        return Err(StoreError::invalid(format!("tombstone {} has a body", doc.rev)));
    }
    if let Some(t) = &doc.type_id {
        DocRef::parse(t)?;
    }
    let expected = rev::rev_of(&doc.id, doc.parent.as_deref(), doc.type_id.as_deref(), doc.deleted, &doc.body)?;
    if expected != doc.rev {
        return Err(StoreError::invalid(format!(
            "revision {} of {} does not match its content (it hashes to {expected})",
            doc.rev, doc.id
        )));
    }
    if rev_exists_in(tx, &doc.rev)? {
        return Ok(Applied::Present);
    }
    if let Some(p) = &doc.parent
        && !rev_exists_in(tx, p)?
    {
        return Ok(Applied::MissingParent);
    }
    tx.execute(
        "INSERT INTO docs (_rev, _id, _parent, _type, _deleted, body, _created_at, actor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            doc.rev,
            doc.id,
            doc.parent,
            doc.type_id,
            doc.deleted as i64,
            serde_json::to_string(&doc.body)?,
            doc.created_at,
            doc.actor,
        ],
    )?;
    Ok(Applied::Written)
}

/// Resolve a type reference to one schema revision, validate `body`
/// against it, and return the pinned reference.
fn resolve_and_validate(
    tx: &Connection,
    validators: &mut Validators,
    type_ref: &DocRef,
    body: &Map<String, Value>,
) -> Result<String, StoreError> {
    let schema = match &type_ref.rev {
        None => match head_in(tx, &type_ref.id)? {
            None => return Err(StoreError::UnknownType { type_id: type_ref.to_string() }),
            Some(d) if d.deleted => return Err(StoreError::Deleted { id: d.id, rev: d.rev }),
            Some(d) => d,
        },
        Some(rev_id) => {
            let d = get_rev_in(tx, rev_id).map_err(|_| StoreError::UnknownType { type_id: type_ref.to_string() })?;
            if d.id != type_ref.id {
                return Err(StoreError::invalid(format!("{rev_id} is a revision of {}, not {}", d.id, type_ref.id)));
            }
            if d.deleted {
                return Err(StoreError::Deleted { id: d.id, rev: d.rev });
            }
            d
        }
    };
    let pinned = DocRef::pinned(&schema.id, &schema.rev).to_string();
    if !validators.contains_key(&pinned) {
        if validators.len() >= MAX_VALIDATORS {
            validators.clear();
        }
        let validator = schema::compile(&pinned, &Value::Object(schema.body))?;
        validators.insert(pinned.clone(), validator);
    }
    let errors = schema::validate(&validators[&pinned], &Value::Object(body.clone()));
    if !errors.is_empty() {
        return Err(StoreError::Validation { schema: pinned, errors });
    }
    Ok(pinned)
}

/// The one write path. Checks protection, pins and validates the type,
/// enforces the revision chain, and inserts. Idempotent by content:
/// replaying an input that is already a leaf returns that leaf.
fn put_draft_in(
    tx: &Connection,
    validators: &mut Validators,
    actor: Option<&str>,
    protected_types: &[String],
    protected_ids: &[String],
    draft: Draft,
) -> Result<Doc, StoreError> {
    if protected_ids.contains(&draft.id) {
        return Err(StoreError::protected_id(&draft.id));
    }
    if let Some(r) = &draft.type_ref
        && protected_types.iter().any(|p| *p == r.path())
    {
        return Err(StoreError::protected_type(&r.path()));
    }
    if let Some(parent) = &draft.parent
        && let Ok(parent_doc) = get_rev_in(tx, parent)
        && let Some(path) = parent_doc.type_path()
        && protected_types.iter().any(|p| p == path)
    {
        return Err(StoreError::protected_type(path));
    }

    let type_id: Option<String> = match (&draft.type_ref, draft.deleted) {
        (None, _) => None,
        // A tombstone keeps its parent's pinned type; a pinned revision is immutable.
        (Some(r), true) => Some(r.to_string()),
        (Some(r), false) => Some(resolve_and_validate(tx, validators, r, &draft.body)?),
    };

    let leaves = leaves_in(tx, &draft.id)?;
    let rev_id = rev::rev_of(&draft.id, draft.parent.as_deref(), type_id.as_deref(), draft.deleted, &draft.body)?;

    if leaves.contains(&rev_id) {
        return get_rev_in(tx, &rev_id);
    }
    let parent_ok = match &draft.parent {
        None => leaves.is_empty(),
        Some(p) => leaves.contains(p),
    };
    if !parent_ok {
        // A create over an existing document with the same body: only the
        // pinned type differs, so the schema moved since it was written.
        let hint = match (&draft.parent, leaves.as_slice()) {
            (None, [leaf]) => get_rev_in(tx, leaf)
                .ok()
                .filter(|d| !d.deleted && d.body == draft.body && d.type_id != type_id)
                .map(|d| {
                    format!(
                        "same body, but the schema moved since {leaf}; update with _parent = {leaf} to re-pin it",
                        leaf = d.rev
                    )
                }),
            _ => None,
        };
        return Err(StoreError::Conflict { id: draft.id, parent: draft.parent, leaves, hint });
    }

    tx.execute(
        "INSERT INTO docs (_rev, _id, _parent, _type, _deleted, body, actor) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            rev_id,
            draft.id,
            draft.parent,
            type_id,
            draft.deleted as i64,
            serde_json::to_string(&draft.body)?,
            actor,
        ],
    )?;
    get_rev_in(tx, &rev_id)
}

impl Store {
    /// Open, or create, and migrate. Never writes a document.
    pub fn open(path: &Path) -> Result<Store, StoreError> {
        // SQLite creates a missing file but not a missing folder.
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| StoreError::Sqlite { message: format!("unable to create {}: {e}", dir.display()) })?;
        }
        Ok(Store::new(db::open(path)?))
    }

    pub fn open_in_memory() -> Result<Store, StoreError> {
        Ok(Store::new(db::open_in_memory()?))
    }

    fn new(conn: Connection) -> Store {
        Store { conn, validators: HashMap::new(), actor: None, protected_types: Vec::new(), protected_ids: Vec::new() }
    }

    /// Name the writer of every revision this store creates from now on.
    pub fn set_actor(&mut self, actor: Option<String>) {
        self.actor = actor;
    }

    pub fn actor(&self) -> Option<&str> {
        self.actor.as_deref()
    }

    /// Refuse writes and deletes of documents whose type path is in
    /// `types` (pinned or not) or whose id is in `ids`.
    pub fn set_protected(&mut self, types: &[&str], ids: &[&str]) {
        self.protected_types = types.iter().map(|t| t.to_string()).collect();
        self.protected_ids = ids.iter().map(|t| t.to_string()).collect();
    }

    /// The current time in the store's own format.
    pub fn now(&self) -> Result<String, StoreError> {
        Ok(self.conn.query_row(&format!("SELECT strftime('{TIME_FORMAT}','now')"), [], |r| r.get(0))?)
    }

    // ---- writes -------------------------------------------------------

    /// Create or update a document. Idempotent by content: replaying the
    /// same input returns the same revision.
    pub fn put(&mut self, input: PutInput) -> Result<Doc, StoreError> {
        let draft = input.into_draft()?;
        self.put_draft(draft)
    }

    /// `put`, but only when `check` returns true inside the same write
    /// transaction. Returns `None` when the check fails. A compare-and-set
    /// for conditions the revision chain cannot express.
    pub fn put_if(
        &mut self,
        input: PutInput,
        check: impl FnOnce(&Connection) -> Result<bool, StoreError>,
    ) -> Result<Option<Doc>, StoreError> {
        let draft = input.into_draft()?;
        let Store { conn, validators, actor, protected_types, protected_ids } = self;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !check(&tx)? {
            return Ok(None);
        }
        let doc = put_draft_in(&tx, validators, actor.as_deref(), protected_types, protected_ids, draft)?;
        tx.commit()?;
        Ok(Some(doc))
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
        self.put_draft(tombstone_draft(&parent_doc)?)
    }

    /// Resolve conflicts the CouchDB way, in one transaction: optionally
    /// write `merged` as a child of the winner, then tombstone every other
    /// live leaf. `merged` may omit `_id` and `_parent`; a `_parent` other
    /// than the winner is a conflict. When `expected` is given, the current
    /// conflicts must be exactly those revisions, so a leaf that arrived after
    /// the caller read the document is never discarded unseen. Returns the
    /// new current revision.
    pub fn resolve(
        &mut self,
        id: &str,
        merged: Option<PutInput>,
        expected: Option<&[String]>,
    ) -> Result<Doc, StoreError> {
        let Store { conn, validators, actor, protected_types, protected_ids } = self;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let head = head_in(&tx, id)?.ok_or_else(|| StoreError::NotFound { id: id.to_string() })?;
        let losers = conflicts_in(&tx, id, &head.rev)?;
        if let Some(expected) = expected {
            let mut want = expected.to_vec();
            let mut have = losers.clone();
            want.sort();
            have.sort();
            if want != have {
                let mut leaves = vec![head.rev.clone()];
                leaves.extend(losers.iter().cloned());
                return Err(StoreError::Conflict {
                    id: id.to_string(),
                    parent: Some(head.rev.clone()),
                    leaves,
                    hint: Some("the conflicts changed since they were read; run resolve again".into()),
                });
            }
        }
        if let Some(mut input) = merged {
            match &input.id {
                None => input.id = Some(id.to_string()),
                Some(other) if other != id => {
                    return Err(StoreError::invalid(format!("merged revision is for {other}, not {id}")));
                }
                Some(_) => {}
            }
            match &input.parent {
                None => input.parent = Some(head.rev.clone()),
                Some(p) if *p == head.rev => {}
                Some(_) => {
                    let mut leaves = vec![head.rev.clone()];
                    leaves.extend(losers.iter().cloned());
                    return Err(StoreError::Conflict {
                        id: id.to_string(),
                        parent: input.parent,
                        leaves,
                        hint: Some(format!("a merge must be written on the winner, {}", head.rev)),
                    });
                }
            }
            put_draft_in(&tx, validators, actor.as_deref(), protected_types, protected_ids, input.into_draft()?)?;
        }
        for loser in &losers {
            let doc = get_rev_in(&tx, loser)?;
            put_draft_in(&tx, validators, actor.as_deref(), protected_types, protected_ids, tombstone_draft(&doc)?)?;
        }
        let mut current = head_in(&tx, id)?.ok_or_else(|| StoreError::NotFound { id: id.to_string() })?;
        current.conflicts = conflicts_in(&tx, id, &current.rev)?;
        tx.commit()?;
        Ok(current)
    }

    /// Documents with more than one live leaf, in id order, after `after`.
    pub fn conflicted(&self, after: Option<&str>, limit: Option<usize>) -> Result<ConflictPage, StoreError> {
        let limit = clamp_limit(limit);
        let ids: Vec<String> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT d._id FROM docs d
                  WHERE d._deleted = 0 AND NOT EXISTS (SELECT 1 FROM docs c WHERE c._parent = d._rev)
                    AND (?1 IS NULL OR d._id > ?1)
                  GROUP BY d._id HAVING count(*) > 1 ORDER BY d._id LIMIT ?2",
            )?;
            let rows = stmt.query_map(params![after, limit as i64], |r| r.get(0))?;
            rows.collect::<Result<_, _>>()?
        };
        let mut docs = Vec::with_capacity(ids.len());
        for id in ids {
            let head = head_in(&self.conn, &id)?.ok_or_else(|| StoreError::NotFound { id: id.clone() })?;
            let conflicts = conflicts_in(&self.conn, &id, &head.rev)?;
            docs.push(Conflicted { id, rev: head.rev, conflicts });
        }
        let next = if docs.len() == limit { docs.last().map(|d| d.id.clone()) } else { None };
        Ok(ConflictPage { docs, next })
    }

    /// `rev` and every revision behind it, newest first.
    pub fn ancestors(&self, rev_id: &str) -> Result<Vec<String>, StoreError> {
        let mut stmt = self.conn.prepare_cached(
            "WITH RECURSIVE chain(_rev, _parent, n) AS (
               SELECT _rev, _parent, 1 FROM docs WHERE _rev = ?1
               UNION ALL
               SELECT d._rev, d._parent, n + 1 FROM chain JOIN docs d ON d._rev = chain._parent WHERE n < ?2)
             SELECT _rev FROM chain ORDER BY n",
        )?;
        let rows = stmt.query_map(params![rev_id, MAX_HISTORY as i64], |r| r.get::<_, String>(0))?;
        let revs = rows.collect::<Result<Vec<_>, _>>()?;
        if revs.is_empty() {
            return Err(StoreError::NotFound { id: rev_id.to_string() });
        }
        Ok(revs)
    }

    // ---- replication --------------------------------------------------

    /// Insert revisions from the vault `peer` and move its checkpoint to
    /// `(seq, rev)`, all in one transaction. A revision whose content does
    /// not hash to its `_rev` fails the whole batch.
    pub fn apply_replicas(&mut self, peer: &str, docs: &[Doc], seq: i64, rev: &str) -> Result<BatchReport, StoreError> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut report = BatchReport::default();
        for doc in docs {
            match insert_replica_in(&tx, doc)? {
                Applied::Written => report.written += 1,
                Applied::Present => report.present += 1,
                Applied::MissingParent => report.missing_parent += 1,
            }
        }
        tx.execute(
            "INSERT INTO checkpoints(peer, seq, rev) VALUES (?1, ?2, ?3)
             ON CONFLICT(peer) DO UPDATE SET seq = excluded.seq, rev = excluded.rev",
            params![peer, seq, rev],
        )?;
        tx.commit()?;
        Ok(report)
    }

    /// The last `(seq, rev)` of `peer` this vault applied.
    pub fn checkpoint(&self, peer: &str) -> Result<Option<(i64, String)>, StoreError> {
        Ok(self
            .conn
            .query_row("SELECT seq, rev FROM checkpoints WHERE peer = ?1", [peer], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    /// This vault's id, a UUID v7 made on first use. Local; never replicates.
    pub fn vault_id(&self) -> Result<String, StoreError> {
        self.conn.execute("INSERT INTO vault(id) SELECT ?1 WHERE NOT EXISTS (SELECT 1 FROM vault)", [new_id()])?;
        Ok(self.conn.query_row("SELECT id FROM vault", [], |r| r.get(0))?)
    }

    /// The revision committed at change-feed position `seq`.
    pub fn rev_at_seq(&self, seq: i64) -> Result<Option<String>, StoreError> {
        Ok(self.conn.query_row("SELECT _rev FROM docs WHERE _local_seq = ?1", [seq], |r| r.get(0)).optional()?)
    }

    fn put_draft(&mut self, draft: Draft) -> Result<Doc, StoreError> {
        let Store { conn, validators, actor, protected_types, protected_ids } = self;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let doc = put_draft_in(&tx, validators, actor.as_deref(), protected_types, protected_ids, draft)?;
        tx.commit()?;
        Ok(doc)
    }

    /// Whether any revision of `id` exists, tombstones included.
    pub fn exists(&self, id: &str) -> Result<bool, StoreError> {
        Ok(!leaves_in(&self.conn, id)?.is_empty())
    }

    // ---- reads --------------------------------------------------------

    /// The current revision of a document. A tombstone is reported as
    /// `Deleted` so callers can undelete from it.
    pub fn get(&self, id: &str) -> Result<Doc, StoreError> {
        match head_in(&self.conn, id)? {
            None => Err(StoreError::NotFound { id: id.to_string() }),
            Some(d) if d.deleted => Err(StoreError::Deleted { id: id.to_string(), rev: d.rev }),
            Some(mut d) => {
                d.conflicts = conflicts_in(&self.conn, id, &d.rev)?;
                Ok(d)
            }
        }
    }

    /// A document by reference: the current revision of `doc://<id>` (or a
    /// bare id), or the exact revision of `doc://<id>?rev=<rev>`.
    /// `deleted_conflicts` adds `_deleted_conflicts` to a current revision.
    pub fn get_href(&self, href: &str, deleted_conflicts: bool) -> Result<Doc, StoreError> {
        let r = DocRef::from_cli(href)?;
        match r.rev {
            None => {
                let mut doc = self.get(&r.id)?;
                if deleted_conflicts {
                    doc.deleted_conflicts = self.deleted_conflicts(&doc.id, &doc.rev)?;
                }
                Ok(doc)
            }
            Some(_) if deleted_conflicts => {
                Err(StoreError::invalid(format!("{href}: deleted_conflicts needs an unpinned reference")))
            }
            Some(rev) => {
                let doc = get_rev_in(&self.conn, &rev)?;
                if doc.id != r.id {
                    return Err(StoreError::invalid(format!("revision {rev} belongs to {}, not {}", doc.id, r.id)));
                }
                Ok(doc)
            }
        }
    }

    /// Tombstoned leaves of `id` other than `winner`: `_deleted_conflicts`.
    pub fn deleted_conflicts(&self, id: &str, winner: &str) -> Result<Vec<String>, StoreError> {
        other_leaves_in(&self.conn, id, winner, true)
    }

    pub fn get_rev(&self, rev_id: &str) -> Result<Doc, StoreError> {
        get_rev_in(&self.conn, rev_id)
    }

    /// Revisions from the current head back to genesis, newest first.
    pub fn history(&self, id: &str, limit: Option<usize>) -> Result<History, StoreError> {
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
        Ok(History { revisions: docs })
    }

    /// Current, non-deleted documents, most recently modified first.
    pub fn list(&self, q: &ListQuery) -> Result<Page, StoreError> {
        let limit = clamp_limit(q.limit);
        let (exact, path) = type_filter(q.type_id.as_deref());
        let sql = match &q.tag {
            Some(_) => format!(
                "SELECT {DOC_COLS} FROM doc_tags t
                   JOIN doc_heads h ON h._id = t._id
                   JOIN docs d ON d._local_seq = h.seq
                  WHERE t.tag = ?3 AND (?1 IS NULL OR d._type = ?1) AND (?2 IS NULL OR d._type_path = ?2)
                    AND (?4 IS NULL OR h.seq < ?4) AND (?6 IS NULL OR substr(d._id, 1, length(?6)) = ?6)
                  ORDER BY h.seq DESC LIMIT ?5"
            ),
            None => format!(
                "SELECT {DOC_COLS} FROM doc_heads h JOIN docs d ON d._local_seq = h.seq
                  WHERE d._deleted = 0 AND (?1 IS NULL OR d._type = ?1) AND (?2 IS NULL OR d._type_path = ?2)
                    AND ?3 IS NULL AND (?4 IS NULL OR h.seq < ?4)
                    AND (?6 IS NULL OR substr(d._id, 1, length(?6)) = ?6)
                  ORDER BY h.seq DESC LIMIT ?5"
            ),
        };
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows =
            stmt.query_map(params![exact, path, q.tag, q.before, limit as i64, q.prefix], |r| row_to_doc(r, true))?;
        let mut docs = rows.collect::<Result<Vec<_>, _>>()?;
        let next = if docs.len() == limit { docs.last().and_then(|d| d.seq) } else { None };
        for d in &mut docs {
            d.seq = None;
        }
        Ok(Page { docs, next })
    }

    /// Every current document, or every one of one type, most recently
    /// modified first.
    pub fn list_all(&self, type_id: Option<&str>) -> Result<Vec<Doc>, StoreError> {
        let mut docs = Vec::new();
        let mut before = None;
        loop {
            let page = self.list(&ListQuery {
                type_id: type_id.map(Into::into),
                before,
                limit: Some(1000),
                ..Default::default()
            })?;
            docs.extend(page.docs);
            match page.next {
                Some(next) => before = Some(next),
                None => return Ok(docs),
            }
        }
    }

    /// Full-text search over title, content and tags of current documents.
    /// An empty query degrades to `list`, with no `content_matches`.
    pub fn search(&self, text: &str, q: &ListQuery) -> Result<SearchPage, StoreError> {
        let Some(match_expr) = fts_query(text) else {
            let page = self.list(q)?;
            return Ok(SearchPage { results: page.docs.into_iter().map(Into::into).collect(), next: page.next });
        };
        let limit = clamp_limit(q.limit);
        let (exact, path) = type_filter(q.type_id.as_deref());
        let sql = "SELECT d._id, d._rev, d._type, d._created_at, d.actor, d.title,
                    snippet(docs_fts, -1, '**', '**', '…', 16)
               FROM docs_fts f JOIN docs d ON d._local_seq = f.rowid
              WHERE docs_fts MATCH ?1
                AND (?2 IS NULL OR d._type = ?2) AND (?3 IS NULL OR d._type_path = ?3)
                AND (?4 IS NULL OR EXISTS (SELECT 1 FROM doc_tags t WHERE t.tag = ?4 AND t._id = d._id))
                AND (?6 IS NULL OR substr(d._id, 1, length(?6)) = ?6)
              ORDER BY bm25(docs_fts, 10.0, 1.0, 5.0) LIMIT ?5";
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt.query_map(params![match_expr, exact, path, q.tag, limit as i64, q.prefix], |r| {
            Ok(SearchResult {
                id: r.get(0)?,
                rev: r.get(1)?,
                type_id: r.get(2)?,
                created_at: r.get(3)?,
                actor: r.get(4)?,
                title: r.get(5)?,
                content_matches: r.get(6)?,
            })
        })?;
        let results = rows.collect::<Result<Vec<_>, _>>()?;
        Ok(SearchPage { results, next: None })
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

    /// The seq of the newest revision, or 0 for an empty vault. It moves on
    /// every write: local, from another connection, or from sync.
    pub fn last_seq(&self) -> Result<i64, StoreError> {
        Ok(self.conn.query_row("SELECT COALESCE(MAX(_local_seq), 0) FROM docs", [], |r| r.get(0))?)
    }

    /// Raw connection, for tests and maintenance.
    pub fn connection(&self) -> &Connection {
        &self.conn
    }
}
