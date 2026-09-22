//! Revision identifiers: `<generation>-<sha256 hex>` over a canonical encoding of
//! the revision's content. Compatible in shape with eto and slouchdb.

use serde_json::{Map, Value};
use sha2::{Digest, Sha256};

use crate::error::StoreError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rev {
    pub generation: u64,
    pub hash: String,
}

/// Parse `<generation>-<hex>`. Gen must be a positive decimal integer.
pub fn parse(rev: &str) -> Result<Rev, StoreError> {
    let (generation, hash) = rev
        .split_once('-')
        .ok_or_else(|| StoreError::invalid(format!("malformed _rev {rev:?}: expected <generation>-<hash>")))?;
    let generation: u64 = generation
        .parse()
        .ok()
        .filter(|g| *g >= 1)
        .ok_or_else(|| StoreError::invalid(format!("malformed _rev {rev:?}: generation must be a positive integer")))?;
    if hash.is_empty() || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(StoreError::invalid(format!("malformed _rev {rev:?}: hash must be hex")));
    }
    Ok(Rev {
        generation,
        hash: hash.to_string(),
    })
}

pub fn format(generation: u64, hash: &str) -> String {
    format!("{generation}-{hash}")
}

/// Deterministic JSON: objects with keys in byte order, no whitespace,
/// standard serde_json escaping for strings and formatting for numbers.
/// Explicit so that a `preserve_order` feature elsewhere cannot change revs.
pub fn canonicalize(value: &Value, out: &mut Vec<u8>) {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push(b'{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                serde_json::to_writer(&mut *out, key).expect("string serialization cannot fail");
                out.push(b':');
                canonicalize(&map[*key], out);
            }
            out.push(b'}');
        }
        Value::Array(items) => {
            out.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(b',');
                }
                canonicalize(item, out);
            }
            out.push(b']');
        }
        scalar => serde_json::to_writer(&mut *out, scalar).expect("scalar serialization cannot fail"),
    }
}

pub fn canonical_bytes(value: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    canonicalize(value, &mut out);
    out
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// Content hash of a revision. Covers `_id`, `_parent`, `_type`, `_deleted`
/// and the user body. Absent `_parent` / `_type` are written as `null`.
pub fn hash(
    id: &str,
    parent: Option<&str>,
    type_id: Option<&str>,
    deleted: bool,
    body: &Map<String, Value>,
) -> String {
    let mut preimage = body.clone();
    preimage.insert("_id".into(), Value::String(id.to_string()));
    preimage.insert("_parent".into(), parent.map(|p| Value::String(p.to_string())).unwrap_or(Value::Null));
    preimage.insert("_type".into(), type_id.map(|t| Value::String(t.to_string())).unwrap_or(Value::Null));
    preimage.insert("_deleted".into(), Value::Bool(deleted));
    let bytes = canonical_bytes(&Value::Object(preimage));
    hex(&Sha256::digest(&bytes))
}

/// The `_rev` a revision with this content will have. Gen is 1 for a
/// genesis revision, otherwise the parent's generation plus one.
pub fn rev_of(
    id: &str,
    parent: Option<&str>,
    type_id: Option<&str>,
    deleted: bool,
    body: &Map<String, Value>,
) -> Result<String, StoreError> {
    let generation = match parent {
        None => 1,
        Some(p) => parse(p)?.generation + 1,
    };
    Ok(format(generation, &hash(id, parent, type_id, deleted, body)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parse_and_format_round_trip() {
        let r = parse("12-abc0").unwrap();
        assert_eq!(r.generation, 12);
        assert_eq!(r.hash, "abc0");
        assert_eq!(format(12, "abc0"), "12-abc0");
        assert!(parse("0-abc").is_err());
        assert!(parse("x-abc").is_err());
        assert!(parse("1-zz").is_err());
        assert!(parse("1").is_err());
    }

    #[test]
    fn canonical_sorts_keys_recursively() {
        let v = json!({"b": {"z": 1, "a": [3, {"y": null, "x": "s"}]}, "a": true});
        let s = String::from_utf8(canonical_bytes(&v)).unwrap();
        assert_eq!(s, r#"{"a":true,"b":{"a":[3,{"x":"s","y":null}],"z":1}}"#);
    }

    #[test]
    fn key_order_does_not_change_rev() {
        let a: Map<String, Value> = serde_json::from_str(r#"{"title":"t","tags":["x"],"n":1}"#).unwrap();
        let b: Map<String, Value> = serde_json::from_str(r#"{"n":1,"tags":["x"],"title":"t"}"#).unwrap();
        assert_eq!(
            rev_of("id", None, Some("note/v1"), false, &a).unwrap(),
            rev_of("id", None, Some("note/v1"), false, &b).unwrap()
        );
    }

    #[test]
    fn gen_follows_parent() {
        let body = Map::new();
        let r1 = rev_of("id", None, None, false, &body).unwrap();
        assert!(r1.starts_with("1-"));
        let r2 = rev_of("id", Some(&r1), None, false, &body).unwrap();
        assert!(r2.starts_with("2-"));
        assert_ne!(r1, r2);
    }
}
