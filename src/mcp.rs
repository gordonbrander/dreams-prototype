//! MCP server: one tool per store operation. Stateless 2026-07-28 only.

use std::borrow::Cow;
use std::sync::{Arc, Mutex, MutexGuard};

use rmcp::{
    ErrorData as McpError, Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::doc::{Doc, PutInput};
use crate::error::StoreError;
use crate::schema::SchemaSummary;
use crate::store::{Changes, ListQuery, Page, Store};

fn to_mcp(e: StoreError) -> McpError {
    let data = serde_json::to_value(&e).ok();
    match e {
        StoreError::Sqlite { .. } => McpError::internal_error(e.to_string(), data),
        _ => McpError::invalid_params(e.to_string(), data),
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct IdParams {
    /// Document id.
    pub id: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteParams {
    /// Document id.
    pub id: String,
    /// The current revision being deleted.
    pub parent: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct HistoryParams {
    pub id: String,
    /// Max revisions to return, newest first.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchParams {
    /// Free text. Empty text lists documents with the filters instead.
    pub query: String,
    #[serde(flatten)]
    pub filter: ListQuery,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ChangesParams {
    /// Return revisions committed after this sequence. Start at 0.
    pub since: Option<i64>,
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RegisterSchemaParams {
    /// A JSON Schema document with `$id`, `title` and `description`.
    pub schema: Value,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct SchemaList {
    pub schemas: Vec<SchemaSummary>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct HistoryResult {
    pub revisions: Vec<Doc>,
}

#[derive(Clone)]
pub struct Vault {
    store: Arc<Mutex<Store>>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl Vault {
    pub fn new(store: Store) -> Self {
        Self {
            store: Arc::new(Mutex::new(store)),
            tool_router: Self::tool_router(),
        }
    }

    fn lock(&self) -> Result<MutexGuard<'_, Store>, McpError> {
        self.store
            .lock()
            .map_err(|e| McpError::internal_error(format!("store lock poisoned: {e}"), None))
    }

    #[tool(description = "Create or update a document. Omit _id to create with a generated id. \
        To update, pass _parent = the current _rev. Set _type to a registered schema id to validate the body. \
        Blessed fields: title (string), content (string), tags (array of strings).")]
    fn put_doc(&self, Parameters(input): Parameters<PutInput>) -> Result<Json<Doc>, McpError> {
        self.lock()?.put(input).map(Json).map_err(to_mcp)
    }

    #[tool(description = "Get the current revision of a document by _id.")]
    fn get_doc(&self, Parameters(p): Parameters<IdParams>) -> Result<Json<Doc>, McpError> {
        self.lock()?.get(&p.id).map(Json).map_err(to_mcp)
    }

    #[tool(description = "Delete a document by writing a tombstone. parent must be its current _rev.")]
    fn delete_doc(&self, Parameters(p): Parameters<DeleteParams>) -> Result<Json<Doc>, McpError> {
        self.lock()?.delete(&p.id, &p.parent).map(Json).map_err(to_mcp)
    }

    #[tool(description = "List current documents, most recently modified first. Filter by type and/or tag. \
        Page with `before` = previous page's `next`.")]
    fn list_docs(&self, Parameters(q): Parameters<ListQuery>) -> Result<Json<Page>, McpError> {
        self.lock()?.list(&q).map(Json).map_err(to_mcp)
    }

    #[tool(description = "Full-text search over title, content and tags of current documents, best match first. \
        Optional type and tag filters. An empty query lists instead.")]
    fn search_docs(&self, Parameters(p): Parameters<SearchParams>) -> Result<Json<Page>, McpError> {
        self.lock()?.search(&p.query, &p.filter).map(Json).map_err(to_mcp)
    }

    #[tool(description = "Revision history of a document from the current revision back to genesis.")]
    fn doc_history(&self, Parameters(p): Parameters<HistoryParams>) -> Result<Json<HistoryResult>, McpError> {
        self.lock()?
            .history(&p.id, p.limit)
            .map(|revisions| Json(HistoryResult { revisions }))
            .map_err(to_mcp)
    }

    #[tool(description = "Change feed: every revision (including tombstones) committed after `since`, in order. \
        Each result carries _seq; continue from last_seq.")]
    fn changes(&self, Parameters(p): Parameters<ChangesParams>) -> Result<Json<Changes>, McpError> {
        self.lock()?
            .changes(p.since.unwrap_or(0), p.limit)
            .map(Json)
            .map_err(to_mcp)
    }

    #[tool(description = "Register a JSON Schema. Its $id becomes a _type. Schemas are immutable: \
        re-registering the same body is a no-op, a different body at the same $id is an error.")]
    fn register_schema(&self, Parameters(p): Parameters<RegisterSchemaParams>) -> Result<Json<SchemaSummary>, McpError> {
        self.lock()?.register_schema(p.schema).map(Json).map_err(to_mcp)
    }

    #[tool(description = "Get a registered JSON Schema by id.")]
    fn get_schema(&self, Parameters(p): Parameters<IdParams>) -> Result<Json<Value>, McpError> {
        self.lock()?.get_schema(&p.id).map(Json).map_err(to_mcp)
    }

    #[tool(description = "List registered schemas (id, title, description).")]
    fn list_schemas(&self) -> Result<Json<SchemaList>, McpError> {
        self.lock()?
            .list_schemas()
            .map(|schemas| Json(SchemaList { schemas }))
            .map_err(to_mcp)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Vault {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION")))
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_instructions(
                "Subconscious: a versioned document vault. Documents have _id, _rev, optional _type (a registered \
                 JSON Schema id), and free-form bodies with blessed fields title, content, tags. Updates must name \
                 the current _rev as _parent.",
            )
    }

    /// Accept only the stateless 2026-07-28 protocol.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }
}

/// Run the MCP server over stdio until the client disconnects.
pub async fn serve(store: Store) -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let service = Vault::new(store).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
