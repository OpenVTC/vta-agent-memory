//! The MCP surface: five memory tools over stdio.
//!
//! Narrow on purpose. `vta-mcp` already exposes the VTA's whole management
//! console — including `vta_call`, which can reach `vta/memory/*` today. This
//! server is not a smaller copy of that; it is the memory *product*: tools
//! shaped like remembering and recalling rather than like a key/value store,
//! with the retrieval story (rank, summarise, fetch on demand) that the raw
//! Trust Tasks do not have.
//!
//! The tool descriptions matter as much as the code. They are the only thing
//! that tells a model that `memory_recall` is cheap and `memory_get` is the
//! follow-up, that a `description` is what recall ranks on, or that forgetting
//! is not reversible.

use std::sync::Arc;

use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use schemars::JsonSchema;
use serde::Deserialize;

use crate::lazy::LazyStore;
use crate::record::{MemoryKey, MemoryRecord, MemoryType};
use crate::store::Store;

/// Wrap a serializable value as pretty JSON tool output.
///
/// Returning a `CallToolResult` rather than a typed `Json<T>` avoids rmcp
/// deriving an output schema: `serde_json::Value` has no fixed object shape and
/// the MCP spec rejects one that claims to.
fn ok_json(value: impl serde::Serialize) -> Result<CallToolResult, McpError> {
    let text = serde_json::to_string_pretty(&value)
        .map_err(|e| McpError::internal_error(format!("serialising result: {e}"), None))?;
    Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
}

/// Surface an anyhow chain to the model with its context intact — the VTA's
/// refusals (`permissionDenied`, `not found`) are the useful part.
fn to_mcp(e: anyhow::Error) -> McpError {
    McpError::internal_error(format!("{e:#}"), None)
}

/// Parse a caller-supplied type string into a [`MemoryType`].
fn parse_type(raw: &str) -> Result<MemoryType, McpError> {
    MemoryType::parse(raw).ok_or_else(|| {
        McpError::invalid_params(
            format!("unknown memory type `{raw}`; expected user, feedback, project or reference"),
            None,
        )
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SaveParams {
    /// Short human name for this memory, e.g. "No PR attribution". Becomes the
    /// stable key; saving the same name again replaces the memory.
    pub name: String,
    /// One of: `user` (who the person is), `feedback` (how they want you to
    /// work — include the why), `project` (ongoing work and constraints not
    /// derivable from the code), `reference` (pointers to external resources).
    #[serde(rename = "type")]
    pub kind: String,
    /// One line saying what this is, written as the answer to "would I want
    /// this loaded right now?". This is what `memory_recall` ranks and returns,
    /// so a vague description makes the memory unfindable.
    pub description: String,
    /// The memory itself. Convert relative dates to absolute ones.
    pub body: String,
    /// Names of related memories. A link to one that does not exist yet is
    /// fine — it marks something worth writing later.
    #[serde(default)]
    pub links: Vec<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RecallParams {
    /// What you are looking for, in words. Omit to get the most recent
    /// memories instead of a search.
    #[serde(default)]
    pub query: Option<String>,
    /// Narrow to one type: `user`, `feedback`, `project`, `reference`.
    #[serde(default)]
    #[serde(rename = "type")]
    pub kind: Option<String>,
    /// Maximum results (default 8).
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetParams {
    /// The memory's key, as returned by `memory_recall` (e.g.
    /// `feedback/no-pr-attribution`).
    pub key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ForgetParams {
    /// The memory's key, as returned by `memory_recall`.
    pub key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListParams {
    /// Narrow to one type: `user`, `feedback`, `project`, `reference`.
    #[serde(default)]
    #[serde(rename = "type")]
    pub kind: Option<String>,
}

/// The MCP server.
///
/// Holds a [`LazyStore`], not a `Store`: the server must exist even when the
/// VTA does not, or the tools vanish and the model cannot say why. See
/// `crate::lazy`.
#[derive(Clone)]
pub struct MemoryMcp {
    lazy: Arc<LazyStore>,
}

#[tool_router]
impl MemoryMcp {
    pub fn new(lazy: Arc<LazyStore>) -> Self {
        Self { lazy }
    }

    /// The connected store, or a tool error naming the fix.
    async fn store(&self) -> Result<&Store, McpError> {
        self.lazy.store().await.map_err(to_mcp)
    }

    /// Default `limit` for recall. Falls back rather than failing: a caller who
    /// asked for a specific limit should not be refused because the config is
    /// unreadable — the store call underneath will report that properly.
    async fn recall_limit(&self) -> usize {
        self.lazy
            .config()
            .await
            .map(|c| c.recall_limit)
            .unwrap_or(8)
    }

    #[tool(
        description = "Save a durable memory to the user's Verifiable Trust Agent. Use for facts \
                       that should outlive this session: who the user is, guidance they have given \
                       about how to work (with the why), ongoing project constraints, and pointers \
                       to external resources. Do NOT save what the repository already records — \
                       code structure, git history, past fixes — or anything that only matters to \
                       this conversation. Saving the same name twice replaces the earlier memory."
    )]
    async fn memory_save(
        &self,
        Parameters(p): Parameters<SaveParams>,
    ) -> Result<CallToolResult, McpError> {
        let kind = parse_type(&p.kind)?;
        let record = MemoryRecord::new(kind, &p.name, &p.description, &p.body, p.links);
        let key = self.store().await?.save(&record).await.map_err(to_mcp)?;
        ok_json(serde_json::json!({
            "key": key.to_string(),
            "contextId": self.store().await?.context_id(),
            "saved": true,
        }))
    }

    #[tool(
        description = "Search the user's stored memories and return the best matches as compact \
                       summaries (key, name, type, description) — not their full text. This is the \
                       cheap call: use it first, then `memory_get` on the one or two keys that \
                       actually matter. With no query it returns the most recently updated \
                       memories instead of searching."
    )]
    async fn memory_recall(
        &self,
        Parameters(p): Parameters<RecallParams>,
    ) -> Result<CallToolResult, McpError> {
        let kind = p.kind.as_deref().map(parse_type).transpose()?;
        let query = p.query.unwrap_or_default();
        let limit = match p.limit {
            Some(n) => n,
            None => self.recall_limit().await,
        }
        .clamp(1, 100);
        let store = self.store().await?;
        let hits = store.recall(&query, kind, limit).await.map_err(to_mcp)?;
        let items: Vec<_> = hits
            .iter()
            .map(|h| h.entry.record.summary(&h.entry.key))
            .collect();
        ok_json(serde_json::json!({
            "contextId": store.context_id(),
            "count": items.len(),
            "memories": items,
        }))
    }

    #[tool(
        description = "Read one memory in full, by the key `memory_recall` returned. Use after \
                       recall when a summary is not enough."
    )]
    async fn memory_get(
        &self,
        Parameters(p): Parameters<GetParams>,
    ) -> Result<CallToolResult, McpError> {
        let key =
            MemoryKey::parse(&p.key).map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        match self.store().await?.get(&key).await.map_err(to_mcp)? {
            Some(entry) => ok_json(entry.record.full(&entry.key)),
            None => Err(McpError::invalid_params(
                format!(
                    "no memory `{key}` in context `{}`",
                    self.store().await?.context_id()
                ),
                None,
            )),
        }
    }

    #[tool(
        description = "Permanently delete one memory by key. Not reversible — there is no undo and \
                       no grace window. Only call this when the user has asked for it, or when a \
                       memory has been superseded by one you just saved."
    )]
    async fn memory_forget(
        &self,
        Parameters(p): Parameters<ForgetParams>,
    ) -> Result<CallToolResult, McpError> {
        let key =
            MemoryKey::parse(&p.key).map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        self.store().await?.forget(&key).await.map_err(to_mcp)?;
        ok_json(serde_json::json!({
            "key": key.to_string(),
            "contextId": self.store().await?.context_id(),
            "forgotten": true,
        }))
    }

    #[tool(
        description = "List every stored memory as a compact summary, optionally narrowed to one \
                       type. Use when the user asks what you remember; use `memory_recall` when \
                       you are looking for something specific."
    )]
    async fn memory_list(
        &self,
        Parameters(p): Parameters<ListParams>,
    ) -> Result<CallToolResult, McpError> {
        let kind = p.kind.as_deref().map(parse_type).transpose()?;
        let mut entries = self
            .store()
            .await?
            .list_of_type(kind)
            .await
            .map_err(to_mcp)?;
        entries.sort_by_key(|e| e.key.to_string());
        let items: Vec<_> = entries.iter().map(|e| e.record.summary(&e.key)).collect();
        ok_json(serde_json::json!({
            "contextId": self.store().await?.context_id(),
            "count": items.len(),
            "memories": items,
        }))
    }

    #[tool(
        description = "Report which VTA trust context these memories live in and how this machine \
                       authenticates to it. Useful when the user asks where their memories are \
                       stored, or when a memory call is being refused."
    )]
    async fn memory_context(&self) -> Result<CallToolResult, McpError> {
        // Deliberately answers without connecting. This is the tool somebody
        // reaches for *because* memory is failing, so needing a working
        // connection to explain a broken one would make it useless exactly when
        // it is wanted. Transport is reported only if a connection already
        // happened to be open.
        let cfg = self.lazy.config().await.map_err(to_mcp)?;
        let transport = if self.lazy.is_connected() {
            match self.lazy.store().await {
                Ok(s) => Some(format!("{:?}", s.client().trust_task_transport())),
                Err(_) => None,
            }
        } else {
            None
        };
        ok_json(serde_json::json!({
            "vtaDid": cfg.identity.vta_did(),
            "contextId": cfg.context_id,
            "identity": cfg.identity.label(),
            "connected": self.lazy.is_connected(),
            "trustTaskTransport": transport,
            "note": "Memories are gated on access to this trust context: an agent scoped to \
                     another context cannot read, write, or delete them.",
        }))
    }
}

#[tool_handler]
impl ServerHandler for MemoryMcp {
    fn get_info(&self) -> ServerInfo {
        // `Implementation` / `InitializeResult` are `#[non_exhaustive]`, so
        // build them via constructors plus field assignment.
        let mut server_info = Implementation::from_build_env();
        server_info.name = "vta-agent-memory".to_string();
        server_info.version = env!("CARGO_PKG_VERSION").to_string();

        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(server_info)
            .with_instructions(
                "Durable memory for this user, stored in their own Verifiable Trust Agent \
                 rather than in this tool. Tools: memory_recall (cheap, ranked summaries — \
                 start here), memory_get (full text of one memory), memory_save, \
                 memory_forget (permanent), memory_list, memory_context. \
                 Memories are scoped to one VTA trust context, which is the isolation \
                 boundary: an agent scoped elsewhere cannot see them. Recall ranks on each \
                 memory's one-line description, so write descriptions that say why a memory \
                 would be worth loading.",
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exposed tool set is the product surface — a tool silently dropped or
    /// renamed changes what the model can do, without any other test failing.
    #[test]
    fn the_tool_router_exposes_exactly_the_memory_surface() {
        let router = MemoryMcp::tool_router();
        let expected = [
            "memory_save",
            "memory_recall",
            "memory_get",
            "memory_forget",
            "memory_list",
            "memory_context",
        ];
        let have: Vec<String> = router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect();
        for name in expected {
            assert!(router.has_route(name), "missing tool {name}; have {have:?}");
        }
        assert_eq!(have.len(), expected.len(), "unexpected tool set: {have:?}");
    }

    #[test]
    fn unknown_types_are_refused_with_the_valid_set() {
        let err = parse_type("secrets").unwrap_err();
        assert!(err.message.contains("user, feedback, project or reference"));
    }
}
