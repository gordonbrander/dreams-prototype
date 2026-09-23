//! MCP server: one tool per document operation. Stateless 2026-07-28 only.

use std::borrow::Cow;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

use rmcp::{
    ErrorData as McpError, Json, ServerHandler, ServiceExt,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{Implementation, ProtocolVersion, ServerCapabilities, ServerConfig},
    tool, tool_handler, tool_router,
    transport::stdio,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::doc::{Doc, PutInput};
use crate::error::StoreError;
use crate::store::{Changes, ConflictPage, History, ListQuery, Page, Store};

/// How a host starts this server on the vault at `db`: the `command` and
/// `args` of one entry in an MCP config's `mcpServers`.
pub fn server_entry(exe: &Path, db: &Path, actor: Option<&str>) -> Value {
    let mut args = vec![json!("--db"), json!(db)];
    if let Some(actor) = actor {
        args.extend([json!("--actor"), json!(actor)]);
    }
    args.push(json!("serve"));
    json!({ "command": exe, "args": args })
}

fn to_mcp(e: StoreError) -> McpError {
    let data = serde_json::to_value(&e).ok();
    match e {
        StoreError::Sqlite { .. } => McpError::internal_error(e.to_string(), data),
        _ => McpError::invalid_params(e.to_string(), data),
    }
}

/// What an agent sends to write a document. The body is one declared JSON
/// object, not flattened fields: a client sends only declared parameters as
/// typed JSON, so flattened arrays and objects arrive as strings.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DocInput {
    /// Document id. Omit to create a new document with a generated UUID v7.
    #[serde(rename = "_id")]
    pub id: Option<String>,
    /// The current `_rev` of the document being updated. Omit for a create.
    #[serde(rename = "_parent")]
    pub parent: Option<String>,
    /// `doc://<id>` or `doc://<id>?rev=<rev>` of a schema document. The body is validated
    /// against it. An unpinned reference is pinned to the schema's current revision at write.
    #[serde(rename = "_type")]
    pub type_id: Option<String>,
    /// The document body, a JSON object. Blessed fields: `title` (string), `content` (string),
    /// `tags` (array of strings). Keys starting with `_` are rejected.
    #[serde(default)]
    pub body: Map<String, Value>,
}

impl From<DocInput> for PutInput {
    fn from(d: DocInput) -> Self {
        PutInput { id: d.id, parent: d.parent, type_id: d.type_id, body: d.body }
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetParams {
    /// Document id.
    pub id: String,
    /// Also list tombstoned leaves other than the winner, as _deleted_conflicts.
    #[serde(default)]
    pub deleted_conflicts: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ConflictsParams {
    /// Cursor: the previous page's `next`.
    pub after: Option<String>,
    /// Page size, 1..=1000. Default 50.
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RevParams {
    /// A revision id, for example `2-ab12...`.
    pub rev: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DeleteParams {
    /// Document id.
    pub id: String,
    /// The current revision being deleted.
    pub parent: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ResolveParams {
    /// Document id.
    pub id: String,
    /// The merged document, written on the winning revision, with its fields in
    /// `body`. Omit _parent (or set it to the winner's _rev). Omit the whole field to keep the winner as it is.
    pub merged: Option<DocInput>,
    /// The _conflicts you read. When given, resolve fails if they changed since.
    pub conflicts: Option<Vec<String>>,
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

    #[tool(description = "Create or update a document. Put the fields in `body`, a JSON object. \
        Omit _id to create with a generated id. To update, pass _parent = the current _rev. \
        Set _type to doc://<id> of a schema document to validate the body; it is pinned to \
        doc://<id>?rev=<rev> at write. Blessed body fields: title (string), content (string), \
        tags (array of strings).")]
    fn put_doc(&self, Parameters(input): Parameters<DocInput>) -> Result<Json<Doc>, McpError> {
        self.lock()?.put(input.into()).map(Json).map_err(to_mcp)
    }

    #[tool(description = "Get the current revision of a document by _id. When sync made concurrent edits, \
        _conflicts lists the other live revisions; read them with get_rev and settle them with resolve_doc.")]
    fn get_doc(&self, Parameters(p): Parameters<GetParams>) -> Result<Json<Doc>, McpError> {
        let store = self.lock()?;
        let mut doc = store.get(&p.id).map_err(to_mcp)?;
        if p.deleted_conflicts {
            doc.deleted_conflicts = store.deleted_conflicts(&doc.id, &doc.rev).map_err(to_mcp)?;
        }
        Ok(Json(doc))
    }

    #[tool(description = "List documents with conflicts, in id order: each with its winning _rev and the \
        other live revisions. Page with `after` = previous page's `next`.")]
    fn list_conflicts(&self, Parameters(p): Parameters<ConflictsParams>) -> Result<Json<ConflictPage>, McpError> {
        self.lock()?.conflicted(p.after.as_deref(), p.limit).map(Json).map_err(to_mcp)
    }

    #[tool(description = "Get one revision by its _rev, current or historical.")]
    fn get_rev(&self, Parameters(p): Parameters<RevParams>) -> Result<Json<Doc>, McpError> {
        self.lock()?.get_rev(&p.rev).map(Json).map_err(to_mcp)
    }

    #[tool(description = "Delete a document by writing a tombstone. parent must be its current _rev, \
        or a revision listed in _conflicts to discard only that one.")]
    fn delete_doc(&self, Parameters(p): Parameters<DeleteParams>) -> Result<Json<Doc>, McpError> {
        self.lock()?.delete(&p.id, &p.parent).map(Json).map_err(to_mcp)
    }

    #[tool(description = "Resolve a document's conflicts: optionally write `merged` on the winning revision, \
        then tombstone every revision listed in _conflicts, in one step. Returns the new current revision.")]
    fn resolve_doc(&self, Parameters(p): Parameters<ResolveParams>) -> Result<Json<Doc>, McpError> {
        self.lock()?.resolve(&p.id, p.merged.map(Into::into), p.conflicts.as_deref()).map(Json).map_err(to_mcp)
    }

    #[tool(description = "List current documents, most recently modified first. Filter by type (a doc:// \
        reference; without ?rev= it matches every pinned revision of that schema) and/or tag. \
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
    fn doc_history(&self, Parameters(p): Parameters<HistoryParams>) -> Result<Json<History>, McpError> {
        self.lock()?.history(&p.id, p.limit).map(Json).map_err(to_mcp)
    }

    #[tool(description = "Change feed: every revision (including tombstones) committed after `since`, in order. \
        Each result carries _seq; continue from last_seq.")]
    fn changes(&self, Parameters(p): Parameters<ChangesParams>) -> Result<Json<Changes>, McpError> {
        self.lock()?
            .changes(p.since.unwrap_or(0), p.limit)
            .map(Json)
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
                "Subconscious: a versioned document vault. Documents have _id, _rev, optional _type, and free-form \
                 bodies with blessed fields title, content, tags. Updates must name the current _rev as _parent. \
                 A schema is a document whose body is a JSON Schema, by convention under schemas/. _type is \
                 doc://<id> of a schema and is pinned to doc://<id>?rev=<rev> at write; list_docs with \
                 type=doc://<id> matches every pinned revision. Scheduled agent tasks are documents typed \
                 doc://schemas/task: `runner` is a doc:// reference to a runner document (list them with \
                 type=doc://schemas/runner), `every` is an interval like 15m, optional `when` {glob, tag, type, \
                 ids} fires only on matching changes, `prompt` is the text the agent receives. Each firing writes a \
                 document typed doc://schemas/run. Runner, run, and seeded schema documents are read-only over MCP.",
            )
    }

    /// Accept only the stateless 2026-07-28 protocol.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[ProtocolVersion::V_2026_07_28])
    }
}

/// Run the MCP server over stdio until the client disconnects. The caller
/// sets the store's actor and protected types.
pub async fn serve(store: Store) -> anyhow::Result<()> {
    crate::cli::install_tracing("warn");
    let service = Vault::new(store).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn doc_input_declares_body_as_an_object() {
        let schema = serde_json::to_value(schemars::schema_for!(DocInput)).unwrap();
        assert_eq!(schema["properties"]["body"]["type"], "object");
    }

    #[test]
    fn doc_input_keeps_json_values_in_the_body() {
        let d: DocInput = serde_json::from_value(json!({
            "_id": "b1",
            "body": {"tags": ["a", "b"], "properties": {"url": {"type": "string"}}}
        }))
        .unwrap();
        let input = PutInput::from(d);
        assert_eq!(input.id.as_deref(), Some("b1"));
        assert_eq!(input.body["tags"], json!(["a", "b"]));
        assert_eq!(input.body["properties"]["url"]["type"], "string");
    }
}
