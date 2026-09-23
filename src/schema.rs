//! JSON Schema compilation and validation. A schema is a document whose
//! body is a JSON Schema; the store resolves `_type` references to one
//! revision and validates against it.

use jsonschema::Validator;
use serde_json::Value;

use crate::error::{FieldError, StoreError};

/// Compile a schema body. `schema_ref` names the schema in the error, so a
/// broken schema is not blamed on the document being written.
pub fn compile(schema_ref: &str, body: &Value) -> Result<Validator, StoreError> {
    jsonschema::options()
        .with_base_uri("json-schema:///")
        .build(body)
        .map_err(|e| StoreError::invalid(format!("{schema_ref} is not a valid JSON Schema: {e}")))
}

/// Every validation failure, addressed by JSON pointer into the body.
pub fn validate(validator: &Validator, body: &Value) -> Vec<FieldError> {
    validator
        .iter_errors(body)
        .map(|e| FieldError {
            path: e.instance_path().to_string(),
            message: e.to_string(),
        })
        .collect()
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
            "type": "object",
            "required": ["title"],
            "properties": {
                "title": {"type": "string", "minLength": 1},
                "tags": {"type": "array", "items": {"type": "string"}}
            }
        })
    }

    #[test]
    fn compiles_with_a_user_id_and_reports_paths() {
        let v = compile("doc://schemas/note?rev=1-a", &note_schema()).unwrap();
        assert!(validate(&v, &json!({"title": "x"})).is_empty());
        let errors = validate(&v, &json!({"tags": [1]}));
        let paths: Vec<&str> = errors.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(errors.len(), 2, "{paths:?}");
        assert!(paths.iter().any(|p| p.contains("tags")), "{paths:?}");
    }

    #[test]
    fn bad_schema_names_the_schema() {
        let err = compile("doc://schemas/bad?rev=1-a", &json!({"type": 12})).unwrap_err();
        assert!(err.to_string().contains("doc://schemas/bad?rev=1-a"), "{err}");
        let err = compile("doc://schemas/ref?rev=1-a", &json!({"$ref": "other.json"})).unwrap_err();
        assert!(err.to_string().contains("doc://schemas/ref"), "{err}");
    }
}
