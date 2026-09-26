//! The file formats of one document: JSON, YAML, and Markdown with YAML
//! frontmatter. A path's extension picks the format. `import`, `doc put`,
//! and the seeds read through here.

use std::path::Path;

use clap::ValueEnum;
use serde_json::{Map, Value};

use crate::error::StoreError;
use crate::markdown;

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum Format {
    Json,
    Yaml,
    /// Markdown with YAML frontmatter; the body is `content`.
    Md,
}

/// The format a path's extension names, if any. Only lowercase extensions match.
pub fn detect_format(path: &Path) -> Option<Format> {
    match path.extension()?.to_str()? {
        "json" => Some(Format::Json),
        "yaml" | "yml" => Some(Format::Yaml),
        "md" | "markdown" => Some(Format::Md),
        _ => None,
    }
}

/// A document's fields from `text`: body fields and metadata keys such as `_id` and `_type`.
pub fn parse_input(text: &str, format: Format) -> Result<Map<String, Value>, StoreError> {
    match format {
        Format::Json => match serde_json::from_str::<Value>(text)? {
            Value::Object(map) => Ok(map),
            _ => Err(StoreError::invalid("JSON input must be an object")),
        },
        Format::Yaml => markdown::yaml_to_map(text),
        Format::Md => markdown::parse(text),
    }
}
