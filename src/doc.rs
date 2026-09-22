//! Document shapes and input normalization.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::error::StoreError;

pub const MAX_ID_BYTES: usize = 512;
pub const MAX_BODY_BYTES: usize = 1 << 20;

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
    /// Change-feed sequence. Present only in `changes` results.
    #[serde(rename = "_seq", skip_serializing_if = "Option::is_none")]
    pub seq: Option<i64>,
    #[serde(flatten)]
    pub body: Map<String, Value>,
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
    /// Registered schema id (for example `note/v1`). The body is validated against it.
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
    pub type_id: Option<String>,
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
        let ok = tags
            .as_array()
            .is_some_and(|items| items.iter().all(Value::is_string));
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
        if let Some(t) = &self.type_id
            && t.is_empty()
        {
            return Err(StoreError::invalid("_type must not be empty"));
        }
        if let Some(p) = &self.parent {
            crate::rev::parse(p)?;
        }
        check_body(&self.body)?;
        Ok(Draft {
            id,
            parent: self.parent,
            type_id: self.type_id,
            deleted: false,
            body: self.body,
        })
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
        assert!(input(json!({"title": "ok", "tags": ["a"]})).into_draft().is_ok());
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
            seq: None,
            body: serde_json::from_value(json!({"title": "T"})).unwrap(),
        };
        let v = serde_json::to_value(&d).unwrap();
        assert_eq!(v["_id"], "a");
        assert_eq!(v["title"], "T");
        assert!(v.get("_seq").is_none());
    }
}
