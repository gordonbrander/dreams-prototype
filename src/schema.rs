//! Schema registry: JSON Schema documents keyed by `_type`, immutable once
//! registered, validated on write. Ported from eto's schema mechanism.

use std::collections::HashMap;

use jsonschema::Validator;
use rusqlite::{Connection, OptionalExtension, params};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::{FieldError, StoreError};
use crate::rev::canonical_bytes;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SchemaSummary {
    pub id: String,
    pub title: String,
    pub description: String,
}

/// Compiled-validator cache. Schemas are immutable, so the id is a
/// sufficient cache key.
#[derive(Default)]
pub struct SchemaRegistry {
    cache: HashMap<String, Validator>,
}

fn required_string<'a>(schema: &'a Value, key: &str) -> Result<&'a str, StoreError> {
    schema
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| StoreError::invalid(format!("schema `{key}` is required and must be a non-empty string")))
}

fn compile(schema: &Value) -> Result<Validator, StoreError> {
    jsonschema::options()
        .with_base_uri("json-schema:///")
        .build(schema)
        .map_err(|e| StoreError::invalid(format!("invalid JSON Schema: {e}")))
}

impl SchemaRegistry {
    /// Register a schema. Identical re-registration is a no-op. A different
    /// body at the same id is an error: schemas are immutable.
    pub fn register(&mut self, conn: &Connection, schema: Value) -> Result<SchemaSummary, StoreError> {
        let id = required_string(&schema, "$id")?.to_string();
        let title = required_string(&schema, "title")?.to_string();
        let description = required_string(&schema, "description")?.to_string();
        if id.starts_with('_') || id.chars().any(char::is_control) {
            return Err(StoreError::invalid("schema $id must not start with '_' or contain control characters"));
        }
        let validator = compile(&schema)?;

        let bytes = canonical_bytes(&schema);
        let text = String::from_utf8(bytes.clone()).expect("canonical JSON is UTF-8");
        conn.execute(
            "INSERT OR IGNORE INTO schemas(id, schema) VALUES (?1, ?2)",
            params![id, text],
        )?;
        let stored: String = conn.query_row("SELECT schema FROM schemas WHERE id = ?1", [&id], |r| r.get(0))?;
        let stored_value: Value = serde_json::from_str(&stored)?;
        if canonical_bytes(&stored_value) != bytes {
            return Err(StoreError::ImmutableSchema { id });
        }
        self.cache.insert(id.clone(), validator);
        Ok(SchemaSummary { id, title, description })
    }

    pub fn get(&self, conn: &Connection, id: &str) -> Result<Value, StoreError> {
        let text: Option<String> = conn
            .query_row("SELECT schema FROM schemas WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        match text {
            Some(t) => Ok(serde_json::from_str(&t)?),
            None => Err(StoreError::UnknownType { type_id: id.to_string() }),
        }
    }

    pub fn list(&self, conn: &Connection) -> Result<Vec<SchemaSummary>, StoreError> {
        let mut stmt = conn.prepare_cached(
            "SELECT id, json_extract(schema, '$.title'), json_extract(schema, '$.description')
               FROM schemas ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(SchemaSummary {
                id: r.get(0)?,
                title: r.get::<_, Option<String>>(1)?.unwrap_or_default(),
                description: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Validate `body` against the schema registered as `type_id`.
    pub fn validate(&mut self, conn: &Connection, type_id: &str, body: &Value) -> Result<(), StoreError> {
        if !self.cache.contains_key(type_id) {
            let schema = self.get(conn, type_id)?;
            self.cache.insert(type_id.to_string(), compile(&schema)?);
        }
        let validator = &self.cache[type_id];
        let errors: Vec<FieldError> = validator
            .iter_errors(body)
            .map(|e| FieldError {
                path: e.instance_path().to_string(),
                message: e.to_string(),
            })
            .collect();
        if errors.is_empty() {
            Ok(())
        } else {
            Err(StoreError::Validation {
                schema: type_id.to_string(),
                errors,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn note_schema() -> Value {
        json!({
            "$id": "note/v1",
            "$schema": "http://json-schema.org/draft-07/schema#",
            "title": "Note",
            "description": "A note",
            "type": "object",
            "required": ["title"],
            "properties": {
                "title": {"type": "string", "minLength": 1},
                "tags": {"type": "array", "items": {"type": "string"}}
            }
        })
    }

    #[test]
    fn relative_id_compiles_and_validates() {
        let conn = crate::db::open_in_memory().unwrap();
        let mut reg = SchemaRegistry::default();
        let s = reg.register(&conn, note_schema()).unwrap();
        assert_eq!(s.id, "note/v1");
        assert!(reg.validate(&conn, "note/v1", &json!({"title": "x"})).is_ok());
        let err = reg.validate(&conn, "note/v1", &json!({"tags": [1]})).unwrap_err();
        match err {
            StoreError::Validation { errors, .. } => {
                let paths: Vec<_> = errors.iter().map(|e| e.path.as_str()).collect();
                assert!(paths.contains(&"") || paths.iter().any(|p| p.contains("tags")), "{paths:?}");
                assert_eq!(errors.len(), 2);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn schemas_are_immutable() {
        let conn = crate::db::open_in_memory().unwrap();
        let mut reg = SchemaRegistry::default();
        reg.register(&conn, note_schema()).unwrap();
        // identical body: no-op
        reg.register(&conn, note_schema()).unwrap();
        let mut changed = note_schema();
        changed["properties"]["title"]["minLength"] = json!(2);
        assert!(matches!(
            reg.register(&conn, changed),
            Err(StoreError::ImmutableSchema { .. })
        ));
        assert!(matches!(
            reg.validate(&conn, "nope/v1", &json!({})),
            Err(StoreError::UnknownType { .. })
        ));
    }

    #[test]
    fn requires_id_title_description() {
        let conn = crate::db::open_in_memory().unwrap();
        let mut reg = SchemaRegistry::default();
        assert!(reg.register(&conn, json!({"title": "t", "description": "d"})).is_err());
        assert!(reg.register(&conn, json!({"$id": "x/v1", "description": "d"})).is_err());
        assert!(reg.register(&conn, json!({"$id": "x/v1", "title": "t"})).is_err());
        assert!(reg.register(&conn, json!({"$id": "x/v1", "title": "t", "description": "d", "type": 12})).is_err());
    }
}
