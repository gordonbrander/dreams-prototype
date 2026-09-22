//! Subconscious: a CouchDB-like, append-only document store on SQLite.
//!
//! - `docs` holds one immutable row per revision.
//! - `doc_heads`, `doc_tags`, `docs_fts` are projections of the current
//!   winner per `_id`, maintained by one trigger.
//! - Schemas are JSON Schema documents keyed by `_type`, immutable once
//!   registered, validated on write.

pub mod db;
pub mod doc;
pub mod error;
pub mod mcp;
pub mod rev;
pub mod schema;
pub mod store;

pub use doc::{Doc, PutInput};
pub use error::StoreError;
pub use store::{Changes, ListQuery, Page, Store};
