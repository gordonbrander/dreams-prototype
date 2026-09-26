//! Line diffs of document bodies, for a person to read before a resolve.

use serde_json::{Map, Value};
use similar::TextDiff;

use crate::doc::PutInput;
use crate::markdown;

/// A body as Markdown with frontmatter, without the revision metadata, so a
/// diff shows only what a person edits.
pub fn body_text(type_id: Option<&str>, body: &Map<String, Value>) -> String {
    markdown::render_input(&PutInput {
        id: None,
        parent: None,
        type_id: type_id.map(str::to_string),
        body: body.clone(),
    })
}

/// A unified diff from `old` to `new`. Empty when they are the same.
pub fn unified(old: &str, new: &str, old_name: &str, new_name: &str) -> String {
    TextDiff::from_lines(old, new).unified_diff().context_radius(3).header(old_name, new_name).to_string()
}
