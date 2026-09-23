use serde::Serialize;
use thiserror::Error;

/// One schema validation failure, addressed by JSON pointer into the body.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct FieldError {
    pub path: String,
    pub message: String,
}

/// Every way a store operation can fail. Serializes with a `name` tag so
/// CLI and MCP clients can discriminate on it.
#[derive(Debug, Error, Serialize)]
#[serde(tag = "name", rename_all = "snake_case")]
pub enum StoreError {
    #[error("document not found: {id}")]
    NotFound { id: String },

    #[error("document {id} is deleted (tombstone {rev})")]
    Deleted { id: String, rev: String },

    #[error("conflict: _parent {parent:?} is not a current leaf of {id}; current leaves: {leaves:?}{}", hint.as_deref().map(|h| format!("; {h}")).unwrap_or_default())]
    Conflict {
        id: String,
        parent: Option<String>,
        leaves: Vec<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        hint: Option<String>,
    },

    #[error("schema validation failed against {schema}: {}", summarize(errors))]
    Validation {
        schema: String,
        errors: Vec<FieldError>,
    },

    #[error("schema not found: {type_id}")]
    UnknownType {
        #[serde(rename = "type")]
        type_id: String,
    },

    #[error("{} is read-only over this connection", describe_protected(type_id, id))]
    Protected {
        #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
        type_id: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        id: Option<String>,
    },

    #[error("invalid input: {message}")]
    InvalidInput { message: String },

    #[error("database error: {message}")]
    Sqlite { message: String },
}

fn summarize(errors: &[FieldError]) -> String {
    errors
        .iter()
        .map(|e| format!("{}: {}", if e.path.is_empty() { "/" } else { &e.path }, e.message))
        .collect::<Vec<_>>()
        .join("; ")
}

fn describe_protected(type_id: &Option<String>, id: &Option<String>) -> String {
    match (type_id, id) {
        (_, Some(id)) => format!("document {id}"),
        (Some(t), None) => format!("every document of type {t}"),
        (None, None) => "this document".to_string(),
    }
}

impl StoreError {
    pub fn protected_type(type_id: &str) -> Self {
        StoreError::Protected { type_id: Some(type_id.to_string()), id: None }
    }

    pub fn protected_id(id: &str) -> Self {
        StoreError::Protected { type_id: None, id: Some(id.to_string()) }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        StoreError::InvalidInput {
            message: message.into(),
        }
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite {
            message: e.to_string(),
        }
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(e: serde_json::Error) -> Self {
        StoreError::InvalidInput {
            message: format!("invalid JSON: {e}"),
        }
    }
}
