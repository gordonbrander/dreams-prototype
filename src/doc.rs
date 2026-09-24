//! Document shapes and input normalization.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::error::StoreError;
use crate::rev;

pub const MAX_ID_BYTES: usize = 512;
pub const MAX_BODY_BYTES: usize = 1 << 20;

/// A reference to a document: `doc://<id>`, or `doc://<id>?rev=<rev>` for
/// one exact revision. Revision ids are content hashes, so a pinned
/// reference names the same bytes in every vault.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DocRef {
    pub id: String,
    pub rev: Option<String>,
}

impl DocRef {
    pub const SCHEME: &'static str = "doc://";
    const REV: &'static str = "?rev=";

    /// Strict: the text must start with `doc://`.
    pub fn parse(text: &str) -> Result<DocRef, StoreError> {
        let rest = text
            .strip_prefix(Self::SCHEME)
            .ok_or_else(|| StoreError::invalid(format!("{text:?} is not a doc:// reference")))?;
        let (id, rev) = match rest.split_once(Self::REV) {
            Some((id, rev)) => {
                rev::parse(rev)?;
                (id, Some(rev.to_string()))
            }
            None => (rest, None),
        };
        check_id(id)?;
        Ok(DocRef { id: id.to_string(), rev })
    }

    /// Lenient, for command-line flags: a bare id gets the scheme.
    pub fn from_cli(text: &str) -> Result<DocRef, StoreError> {
        if text.starts_with(Self::SCHEME) { Self::parse(text) } else { Self::parse(&format!("{}{text}", Self::SCHEME)) }
    }

    pub fn pinned(id: &str, rev: &str) -> DocRef {
        DocRef { id: id.to_string(), rev: Some(rev.to_string()) }
    }

    /// `doc://<id>`, without the revision.
    pub fn path(&self) -> String {
        format!("{}{}", Self::SCHEME, self.id)
    }

    /// The part of a reference before `?rev=`.
    pub fn path_of(text: &str) -> &str {
        text.split_once(Self::REV).map_or(text, |(p, _)| p)
    }
}

impl std::fmt::Display for DocRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}{}", Self::SCHEME, self.id)?;
        if let Some(rev) = &self.rev {
            write!(f, "{}{rev}", Self::REV)?;
        }
        Ok(())
    }
}

/// A stored revision as returned to clients. Body fields are flattened
/// alongside the reserved `_*` fields.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct Doc {
    #[serde(rename = "_id")]
    pub id: String,
    #[serde(rename = "_rev")]
    pub rev: String,
    #[serde(rename = "_parent")]
    pub parent: Option<String>,
    #[serde(rename = "_type")]
    pub type_id: Option<String>,
    #[serde(rename = "_deleted")]
    pub deleted: bool,
    #[serde(rename = "_created_at")]
    pub created_at: String,
    /// Who wrote the revision, when known (a scheduled task's id, or whatever
    /// `--actor` named). Absent for ordinary writes.
    #[serde(rename = "_actor", skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    /// Change-feed sequence. Present only in `changes` results.
    #[serde(rename = "_seq", skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    /// Other live leaves of this document, when replication made concurrent
    /// edits. Present only on the current revision from `get`. Resolve with
    /// `resolve`, or by tombstoning each leaf listed here.
    #[serde(rename = "_conflicts", default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    /// Tombstoned leaves other than the winner, such as losers a resolve
    /// discarded. Present only when asked for.
    #[serde(rename = "_deleted_conflicts", default, skip_serializing_if = "Vec::is_empty")]
    pub deleted_conflicts: Vec<String>,
    #[serde(flatten)]
    pub body: Map<String, Value>,
}

impl Doc {
    /// `_type` without its `?rev=` pin: the schema's `doc://` path.
    pub fn type_path(&self) -> Option<&str> {
        self.type_id.as_deref().map(DocRef::path_of)
    }
}

/// What a client sends to `put`. `_id` may be omitted (a UUID v7 is
/// generated). `_parent` is the revision being updated; omit for a genesis
/// create. Everything else is the body.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PutInput {
    /// Document id. Omit to create a new document with a generated UUID v7.
    #[serde(rename = "_id", default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// The current `_rev` of the document being updated. Omit for a create.
    #[serde(rename = "_parent", default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// `doc://<id>` or `doc://<id>?rev=<rev>` of a schema document. The body is validated
    /// against it. An unpinned reference is pinned to the schema's current revision at write.
    #[serde(rename = "_type", default, skip_serializing_if = "Option::is_none")]
    pub type_id: Option<String>,
    /// Body fields. Blessed: `title` (string), `content` (string), `tags` (array of strings).
    /// Keys starting with `_` are rejected.
    #[serde(flatten)]
    pub body: Map<String, Value>,
}

/// A normalized, not-yet-persisted revision.
#[derive(Debug, Clone)]
pub struct Draft {
    pub id: String,
    pub parent: Option<String>,
    /// Pinned or not. The store pins it before hashing.
    pub type_ref: Option<DocRef>,
    pub deleted: bool,
    pub body: Map<String, Value>,
}

pub fn new_id() -> String {
    Uuid::now_v7().to_string()
}

pub fn check_id(id: &str) -> Result<(), StoreError> {
    if id.is_empty() {
        return Err(StoreError::invalid("_id must not be empty"));
    }
    if id.len() > MAX_ID_BYTES {
        return Err(StoreError::invalid(format!("_id longer than {MAX_ID_BYTES} bytes")));
    }
    if id.starts_with('_') {
        return Err(StoreError::invalid("_id must not start with '_'"));
    }
    if id.chars().any(char::is_control) {
        return Err(StoreError::invalid("_id must not contain control characters"));
    }
    if id.contains("?rev=") {
        return Err(StoreError::invalid("_id must not contain `?rev=`"));
    }
    Ok(())
}

/// Reject reserved keys and enforce blessed field types and the size limit.
pub fn check_body(body: &Map<String, Value>) -> Result<(), StoreError> {
    if let Some(key) = body.keys().find(|k| k.starts_with('_')) {
        let hint = if key == "_rev" { " (use _parent to name the revision you are updating)" } else { "" };
        return Err(StoreError::invalid(format!("body key {key:?} is reserved{hint}")));
    }
    for field in ["title", "content"] {
        if let Some(v) = body.get(field)
            && !v.is_string()
        {
            return Err(StoreError::invalid(format!("{field} must be a string")));
        }
    }
    if let Some(tags) = body.get("tags") {
        let ok = tags.as_array().is_some_and(|items| items.iter().all(Value::is_string));
        if !ok {
            return Err(StoreError::invalid("tags must be an array of strings"));
        }
    }
    let size = serde_json::to_vec(body)?.len();
    if size > MAX_BODY_BYTES {
        return Err(StoreError::invalid(format!("body larger than {MAX_BODY_BYTES} bytes")));
    }
    Ok(())
}

impl PutInput {
    pub fn into_draft(self) -> Result<Draft, StoreError> {
        let id = match self.id {
            Some(id) => {
                check_id(&id)?;
                id
            }
            None => new_id(),
        };
        let type_ref = match &self.type_id {
            Some(t) => Some(DocRef::parse(t).map_err(|e| StoreError::invalid(format!("_type: {e}")))?),
            None => None,
        };
        if let Some(p) = &self.parent {
            rev::parse(p)?;
        }
        check_body(&self.body)?;
        Ok(Draft { id, parent: self.parent, type_ref, deleted: false, body: self.body })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn input(v: Value) -> PutInput {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn generates_uuid_v7_when_id_omitted() {
        let d = input(json!({"title": "x"})).into_draft().unwrap();
        let u = Uuid::parse_str(&d.id).unwrap();
        assert_eq!(u.get_version_num(), 7);
    }

    #[test]
    fn rejects_reserved_keys_and_bad_blessed_types() {
        assert!(input(json!({"_rev": "1-a"})).into_draft().is_err());
        assert!(input(json!({"_foo": 1})).into_draft().is_err());
        assert!(input(json!({"title": 7})).into_draft().is_err());
        assert!(input(json!({"tags": "x"})).into_draft().is_err());
        assert!(input(json!({"tags": ["x", 1]})).into_draft().is_err());
        assert!(input(json!({"_id": "_design"})).into_draft().is_err());
        assert!(input(json!({"_id": ""})).into_draft().is_err());
        assert!(input(json!({"_id": "a?rev=1-x"})).into_draft().is_err());
        assert!(input(json!({"_type": "note/v1"})).into_draft().is_err());
        assert!(input(json!({"title": "ok", "tags": ["a"]})).into_draft().is_ok());
        let d = input(json!({"_type": "doc://schemas/note"})).into_draft().unwrap();
        assert_eq!(d.type_ref.unwrap().rev, None);
    }

    #[test]
    fn doc_refs_parse_and_print() {
        let r = DocRef::parse("doc://a/b.md").unwrap();
        assert_eq!((r.id.as_str(), r.rev.as_deref()), ("a/b.md", None));
        assert_eq!(r.to_string(), "doc://a/b.md");
        let r = DocRef::parse("doc://a/b.md?rev=1-ab").unwrap();
        assert_eq!((r.id.as_str(), r.rev.as_deref()), ("a/b.md", Some("1-ab")));
        assert_eq!(r.to_string(), "doc://a/b.md?rev=1-ab");
        assert_eq!(r.path(), "doc://a/b.md");
        for bad in ["a/b", "doc://", "doc://a?rev=bad", "doc://a?rev=1-x?rev=1-y", "doc://?rev=1-a", "doc://_x"] {
            assert!(DocRef::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(DocRef::from_cli("runners/claude").unwrap().to_string(), "doc://runners/claude");
        assert_eq!(DocRef::from_cli("doc://runners/claude?rev=2-ff").unwrap().rev.as_deref(), Some("2-ff"));
        assert_eq!(DocRef::path_of("doc://x?rev=1-a"), "doc://x");
        assert_eq!(DocRef::path_of("doc://x"), "doc://x");
    }

    #[test]
    fn doc_serializes_flat() {
        let d = Doc {
            id: "a".into(),
            rev: "1-x".into(),
            parent: None,
            type_id: None,
            deleted: false,
            created_at: "t".into(),
            actor: None,
            seq: None,
            conflicts: Vec::new(),
            deleted_conflicts: Vec::new(),
            body: serde_json::from_value(json!({"title": "T"})).unwrap(),
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["_id"], "a");
        assert_eq!(v["title"], "T");
        assert!(v.get("_seq").is_none());
        assert_eq!(d.type_path(), None);
        let typed = Doc { type_id: Some("doc://schemas/note?rev=1-aa".into()), ..d };
        assert_eq!(typed.type_path(), Some("doc://schemas/note"));
    }
}
