//! stdio MCP server for scode.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, Content, ServerCapabilities, ServerInfo},
    schemars, tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler, ServiceExt,
};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

use crate::corpus::TokenMode;
use crate::index::{index_and_maybe_write, MemoryStore};

#[derive(Clone)]
pub struct ScodeMcp {
    store: Arc<Mutex<MemoryStore>>,
    tool_router: ToolRouter<Self>,
}

impl ScodeMcp {
    pub fn new() -> Self {
        Self {
            store: Arc::new(Mutex::new(MemoryStore::new())),
            tool_router: Self::tool_router(),
        }
    }
}

impl Default for ScodeMcp {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct IndexArgs {
    /// Path to a source tree or frozen-gavs.toml
    pub input: String,
    /// Optional output directory. Omit for memory-only.
    #[serde(default)]
    pub out: Option<String>,
    /// Token mode: idents (default) or all
    #[serde(default = "default_token_mode")]
    pub token_mode: String,
    /// Fetch missing sources jars only when not already in a local Maven/Gradle cache
    #[serde(default)]
    pub fetch: bool,
    /// Memory slot id (default)
    #[serde(default = "default_memory_id")]
    pub memory_id: String,
    /// Optional cache dir for downloaded jars
    #[serde(default)]
    pub cache_dir: Option<String>,
    /// Local Maven repository root (default: $SCODE_LOCAL_REPO / $M2_REPO / ~/.m2/repository)
    #[serde(default)]
    pub local_repo: Option<String>,
    /// Remote Maven-layout base URL for fetch when the jar is not local
    #[serde(default)]
    pub repository: Option<String>,
}

fn default_token_mode() -> String {
    "idents".into()
}

fn default_memory_id() -> String {
    "default".into()
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    pub query: String,
    /// Index path, "memory", or omit for default memory
    #[serde(default)]
    pub index: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct SearchMultiArgs {
    pub queries: Vec<String>,
    #[serde(default)]
    pub index: Option<String>,
    #[serde(default)]
    pub limit: Option<usize>,
    #[serde(default)]
    pub per_query_limit: Option<usize>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct StatsArgs {
    #[serde(default)]
    pub index: Option<String>,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct LoadArgs {
    /// Disk index directory to load into the session
    pub path: String,
}

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct UnloadArgs {
    /// "memory" / memory_id, or a disk path previously loaded
    pub target: String,
}

fn text_ok(v: impl Serialize) -> Result<CallToolResult, McpError> {
    let s = serde_json::to_string_pretty(&v)
        .map_err(|e| McpError::internal_error(format!("json encode: {e}"), None))?;
    Ok(CallToolResult::success(vec![Content::text(s)]))
}

fn map_err(e: anyhow::Error) -> McpError {
    McpError::internal_error(e.to_string(), None)
}

#[tool_router]
impl ScodeMcp {
    #[tool(
        description = "Build a δ inverted index from a source tree or frozen-gavs.toml. Omit out for memory-only."
    )]
    async fn scode_index(
        &self,
        Parameters(args): Parameters<IndexArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mode = TokenMode::parse(&args.token_mode).map_err(map_err)?;
        let out = args.out.as_ref().map(PathBuf::from);
        let cache = args.cache_dir.as_ref().map(PathBuf::from);
        let local_repo = args.local_repo.as_ref().map(PathBuf::from);
        let index = index_and_maybe_write(
            Path::new(&args.input),
            out.as_deref(),
            mode,
            args.fetch,
            cache.as_deref(),
            args.repository.as_deref(),
            local_repo.as_deref(),
        )
        .map_err(map_err)?;

        let stats = index.stats();
        let mut store = self.store.lock().await;
        let arc = store.insert_memory(&args.memory_id, index);
        if let Some(ref dir) = out {
            store.insert_path(dir, arc);
        }

        text_ok(serde_json::json!({
            "ok": true,
            "memory_id": args.memory_id,
            "persisted": out.is_some(),
            "stats": stats,
        }))
    }

    #[tool(description = "Locate identifier occurrences by exact simple name.")]
    async fn scode_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let index = {
            let mut store = self.store.lock().await;
            store.resolve(args.index.as_deref()).map_err(map_err)?
        };
        let hits = index.search(&args.query, args.limit).map_err(map_err)?;
        let from_memory = match args.index.as_deref() {
            None | Some("") | Some("memory") => true,
            Some(p) => !Path::new(p).exists(),
        };
        text_ok(serde_json::json!({
            "query": args.query,
            "from_memory": from_memory,
            "hits": hits,
            "count": hits.len(),
        }))
    }

    #[tool(description = "Locate multiple names (OR), dedup hits, tag matched_queries.")]
    async fn scode_search_multi(
        &self,
        Parameters(args): Parameters<SearchMultiArgs>,
    ) -> Result<CallToolResult, McpError> {
        let index = {
            let mut store = self.store.lock().await;
            store.resolve(args.index.as_deref()).map_err(map_err)?
        };
        let res = index
            .search_multi(&args.queries, args.limit, args.per_query_limit)
            .map_err(map_err)?;
        text_ok(serde_json::json!({
            "queries": args.queries,
            "hits": res.hits,
            "count": res.hits.len(),
        }))
    }

    #[tool(description = "Return index statistics.")]
    async fn scode_stats(
        &self,
        Parameters(args): Parameters<StatsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let index = {
            let mut store = self.store.lock().await;
            store.resolve(args.index.as_deref()).map_err(map_err)?
        };
        text_ok(index.stats())
    }

    #[tool(description = "Load a disk index directory into the MCP session (RAM).")]
    async fn scode_load(
        &self,
        Parameters(args): Parameters<LoadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut store = self.store.lock().await;
        let idx = store
            .load_path(Path::new(&args.path))
            .map_err(map_err)?;
        text_ok(serde_json::json!({
            "ok": true,
            "path": args.path,
            "stats": idx.stats(),
        }))
    }

    #[tool(description = "Unload a memory slot or previously loaded disk path from the session.")]
    async fn scode_unload(
        &self,
        Parameters(args): Parameters<UnloadArgs>,
    ) -> Result<CallToolResult, McpError> {
        let mut store = self.store.lock().await;
        let target = args.target.as_str();
        let removed = if target == "memory" {
            store.unload_memory("default")
        } else {
            let path = Path::new(target);
            if path.exists() || store.has_loaded_path(path) {
                store.unload_path(path)
            } else {
                store.unload_memory(target)
            }
        };
        text_ok(serde_json::json!({ "ok": removed, "target": args.target }))
    }
}

#[tool_handler]
impl ServerHandler for ScodeMcp {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "scode: gap+Elias-δ inverted index for Java/Kotlin identifier name locate over sources keep-sets."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

/// Run the MCP server on stdio.
pub async fn run_stdio() -> anyhow::Result<()> {
    let server = ScodeMcp::new();
    let service = server.serve(rmcp::transport::stdio()).await?;
    service.waiting().await?;
    Ok(())
}