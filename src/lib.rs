//! Subconscious: a CouchDB-like, append-only document store on SQLite.
//!
//! - `docs` holds one immutable row per revision.
//! - `doc_heads`, `doc_tags`, `docs_fts` are projections of the current
//!   winner per `_id`, maintained by one trigger.
//! - Schemas are JSON Schema documents keyed by `_type`, immutable once
//!   registered, validated on write.

pub mod cli;
pub mod daemon;
pub mod db;
pub mod doc;
pub mod error;
pub mod markdown;
pub mod mcp;
pub mod rev;
pub mod runner;
pub mod schema;
pub mod store;
pub mod task;

pub use doc::{Doc, PutInput};
pub use error::StoreError;
pub use store::{Changes, History, ListQuery, Page, SchemaList, Store};
