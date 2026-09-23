//! Markdown with YAML frontmatter: the human format for one document.
//!
//! ```text
//! ---
//! _id: ...
//! _rev: ...
//! title: Hello
//! ---
//! <content, byte-verbatim>
//! ```
//!
//! Delimiter lines carry their own newline, so `render` then `parse` is a
//! bijection on content. Nothing is appended after the content.

use serde_json::{Map, Value};
use serde_yaml_ng::{Mapping, Value as Yaml};

use crate::doc::{Doc, PutInput};
use crate::error::StoreError;

/// Split into (frontmatter YAML, content). No frontmatter means the whole
/// text is content.
fn split(text: &str) -> Result<(Option<&str>, &str), StoreError> {
    let after_open = match text.strip_prefix("---\n").or_else(|| text.strip_prefix("---\r\n")) {
        Some(rest) => rest,
        None => return Ok((None, text)),
    };
    let mut pos = 0;
    loop {
        let rest = &after_open[pos..];
        let (line, consumed, has_newline) = match rest.find('\n') {
            Some(i) => (&rest[..i], i + 1, true),
            None => (rest, rest.len(), false),
        };
        if line.trim_end_matches('\r') == "---" {
            return Ok((Some(&after_open[..pos]), &after_open[pos + consumed..]));
        }
        if !has_newline {
            return Err(StoreError::invalid("unterminated frontmatter: missing closing ---"));
        }
        pos += consumed;
    }
}

/// Parse YAML into a JSON object. Empty YAML is an empty object.
pub fn yaml_to_map(yaml: &str) -> Result<Map<String, Value>, StoreError> {
    let value: Value =
        serde_yaml_ng::from_str(yaml).map_err(|e| StoreError::invalid(format!("invalid YAML: {e}")))?;
    match value {
        Value::Null => Ok(Map::new()),
        Value::Object(map) => Ok(map),
        _ => Err(StoreError::invalid("YAML must be a mapping")),
    }
}

/// Parse a Markdown document into body fields. Frontmatter sets fields; a
/// non-empty content section sets `content`.
pub fn parse(text: &str) -> Result<Map<String, Value>, StoreError> {
    let (frontmatter, content) = split(text)?;
    let mut map = match frontmatter {
        None => Map::new(),
        Some(yaml) => yaml_to_map(yaml)?,
    };
    if !content.is_empty() {
        if map.contains_key("content") {
            return Err(StoreError::invalid(
                "frontmatter sets `content` but the document also has a body",
            ));
        }
        map.insert("content".into(), Value::String(content.to_string()));
    }
    Ok(map)
}

fn yaml_str(s: &str) -> Yaml {
    Yaml::String(s.to_string())
}

/// Render a document. Reserved fields first, then body fields in key order.
/// A non-empty string `content` becomes the body; anything else stays in
/// the frontmatter.
pub fn render(doc: &Doc) -> String {
    let mut fm = Mapping::new();
    fm.insert(yaml_str("_id"), yaml_str(&doc.id));
    fm.insert(yaml_str("_rev"), yaml_str(&doc.rev));
    if let Some(p) = &doc.parent {
        fm.insert(yaml_str("_parent"), yaml_str(p));
    }
    if let Some(t) = &doc.type_id {
        fm.insert(yaml_str("_type"), yaml_str(t));
    }
    if doc.deleted {
        fm.insert(yaml_str("_deleted"), Yaml::Bool(true));
    }
    fm.insert(yaml_str("_created_at"), yaml_str(&doc.created_at));
    if let Some(a) = &doc.actor {
        fm.insert(yaml_str("_actor"), yaml_str(a));
    }
    if let Some(seq) = doc.seq {
        fm.insert(yaml_str("_seq"), Yaml::Number(seq.into()));
    }
    for (key, revs) in [("_conflicts", &doc.conflicts), ("_deleted_conflicts", &doc.deleted_conflicts)] {
        if !revs.is_empty() {
            fm.insert(yaml_str(key), Yaml::Sequence(revs.iter().map(|r| yaml_str(r)).collect()));
        }
    }
    with_body(fm, &doc.body)
}

/// Render a write that has not happened yet, as valid `put` input.
pub fn render_input(input: &PutInput) -> String {
    let mut fm = Mapping::new();
    for (key, value) in [("_id", &input.id), ("_parent", &input.parent), ("_type", &input.type_id)] {
        if let Some(v) = value {
            fm.insert(yaml_str(key), yaml_str(v));
        }
    }
    with_body(fm, &input.body)
}

/// Frontmatter, then body fields in key order. A non-empty string
/// `content` becomes the text after the frontmatter.
fn with_body(mut fm: Mapping, body: &Map<String, Value>) -> String {
    let mut content = "";
    for (key, value) in body {
        if key == "content"
            && let Value::String(s) = value
            && !s.is_empty()
        {
            content = s;
            continue;
        }
        let yaml = serde_yaml_ng::to_value(value).expect("JSON values convert to YAML");
        fm.insert(yaml_str(key), yaml);
    }
    let yaml = serde_yaml_ng::to_string(&fm).expect("a mapping serializes");
    format!("---\n{yaml}---\n{content}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn doc(body: Value) -> Doc {
        Doc {
            id: "007".into(),
            rev: "1-abc".into(),
            parent: None,
            type_id: Some("note/v1".into()),
            deleted: false,
            created_at: "2026-09-22T20:14:03.512Z".into(),
            actor: None,
            seq: None,
            conflicts: Vec::new(),
            deleted_conflicts: Vec::new(),
            body: body.as_object().unwrap().clone(),
        }
    }

    fn round_trip(body: Value) -> Map<String, Value> {
        let d = doc(body);
        let text = render(&d);
        let parsed = parse(&text).unwrap();
        // reserved fields come back as written
        assert_eq!(parsed["_id"], "007");
        assert_eq!(parsed["_rev"], "1-abc");
        assert_eq!(parsed["_type"], "note/v1");
        assert!(parsed.get("_parent").is_none());
        assert!(parsed.get("_deleted").is_none());
        assert_eq!(parsed["_created_at"], "2026-09-22T20:14:03.512Z");
        parsed
    }

    #[test]
    fn content_variants_round_trip_exactly() {
        for content in ["a", "a\n", "\na", "a\n\nb\n", "---\nnot frontmatter\n", "  indented\n"] {
            let parsed = round_trip(json!({"title": "yes", "content": content}));
            assert_eq!(parsed["content"], content, "{content:?}");
            assert_eq!(parsed["title"], "yes");
        }
    }

    #[test]
    fn empty_and_absent_content() {
        let parsed = round_trip(json!({"title": "t", "content": ""}));
        assert_eq!(parsed["content"], "");
        let parsed = round_trip(json!({"title": "t"}));
        assert!(parsed.get("content").is_none());
        // rendering ends right after the closing delimiter
        assert!(render(&doc(json!({"title": "t"}))).ends_with("---\n"));
    }

    #[test]
    fn nested_values_and_quoting_round_trip() {
        let body = json!({"title": "true", "n": 42, "flag": false, "tags": ["a", "b"],
                          "meta": {"k": [1, {"z": null}]}, "content": "x"});
        let parsed = round_trip(body.clone());
        for key in ["title", "n", "flag", "tags", "meta", "content"] {
            assert_eq!(parsed[key], body[key], "{key}");
        }
    }

    #[test]
    fn plain_markdown_is_content() {
        assert_eq!(parse("just text\n").unwrap()["content"], "just text\n");
        assert!(parse("").unwrap().is_empty());
        assert_eq!(parse("--- not a delimiter").unwrap()["content"], "--- not a delimiter");
    }

    #[test]
    fn crlf_delimiters_and_empty_frontmatter() {
        let m = parse("---\r\ntitle: t\r\n---\r\nbody\r\n").unwrap();
        assert_eq!(m["title"], "t");
        assert_eq!(m["content"], "body\r\n");
        assert!(parse("---\n---\n").unwrap().is_empty());
        assert_eq!(parse("---\n---\nx").unwrap()["content"], "x");
    }

    #[test]
    fn errors() {
        assert!(parse("---\ntitle: t\nno end").is_err());
        assert!(parse("---\ntitle: t\n").is_err());
        assert!(parse("---\ncontent: x\n---\nbody").is_err());
        assert!(parse("---\n- a list\n---\n").is_err());
        assert!(parse("---\n: [bad\n---\n").is_err());
    }
}
