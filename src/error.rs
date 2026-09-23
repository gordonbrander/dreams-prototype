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

    #[error("conflict: _parent {parent:?} is not a current leaf of {id}; current leaves: {leaves:?}")]
    Conflict {
        id: String,
        parent: Option<String>,
        leaves: Vec<String>,
    },

    #[error("schema validation failed against {schema}: {}", summarize(errors))]
    Validation {
        schema: String,
        errors: Vec<FieldError>,
    },

    #[error("unknown _type {type_id}; register its schema first")]
    UnknownType {
        #[serde(rename = "type")]
        type_id: String,
    },

    #[error("schema {id} is already registered with a different body; schemas are immutable, register a new version")]
    ImmutableSchema { id: String },

    #[error("documents of type {type_id} are read-only over this connection")]
    Protected {
        #[serde(rename = "type")]
        type_id: String,
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

impl StoreError {
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
