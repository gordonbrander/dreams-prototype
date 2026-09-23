//! Subconscious: a CouchDB-like, append-only document store on SQLite.
//!
//! - `docs` holds one immutable row per revision.
//! - `doc_heads`, `doc_tags`, `docs_fts` are projections of the current
//!   winner per `_id`, maintained by one trigger.
//! - A schema is a document whose body is a JSON Schema. `_type` is a
//!   `doc://` reference to one, pinned to a revision at write, and the
//!   body is validated against that revision.

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

pub use doc::{Doc, DocRef, PutInput};
pub use error::StoreError;
pub use store::{Changes, History, ListQuery, Page, Store};
