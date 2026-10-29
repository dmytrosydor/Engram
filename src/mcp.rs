//! MCP server over stdio: `search`, `get`, `write`.
//!
//! One process per Claude session. The session's space comes from the folder Claude
//! starts it in; every call opens its own short-lived connection (WAL allows many readers).

use std::path::PathBuf;

use anyhow::Result;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{
    ErrorData as McpError, ServerHandler, ServiceExt, schemars, tool, tool_handler, tool_router,
};
use serde::Deserialize;

use crate::memory::{self, Query, Scope, Write};
use crate::{db, embed, index, spaces};

const INSTRUCTIONS: &str = "Long-term memory across sessions. \
search before answering about past work, decisions or conventions; get for full text; \
write one durable fact per call (decision, gotcha, rule, preference), not progress notes.";

#[derive(Clone)]
pub struct Server {
    db: PathBuf,
    space: String,
    visible: Vec<String>,
    session: String,
    tool_router: ToolRouter<Self>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Words or a question.
    pub q: String,
    /// "project" (default: this project + shared + global), "all", or "archive".
    pub scope: Option<String>,
    /// Only this kind: rule, preference, decision, fact, gotcha, lesson, state, task, person, event.
    pub kind: Option<String>,
    /// Default 8.
    pub limit: Option<usize>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct GetArgs {
    /// Item ids from search results.
    pub ids: Vec<i64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct WriteArgs {
    /// rule, preference, decision, fact, gotcha, lesson, state, task, person, event.
    pub kind: String,
    /// The gist, at most 120 characters.
    pub title: String,
    /// One fact with its evidence, at most 1500 characters.
    pub body: String,
    /// Where it came from, e.g. "pr:owner/repo#12" or "session".
    pub source: Option<String>,
    /// For state and task: YYYY-MM-DD after which it is archived.
    pub valid_until: Option<String>,
    /// Omit first; if similar items exist, repeat with "add", "update:<id>" or "supersede:<id>".
    pub mode: Option<String>,
    /// "global" for things true in every project; default is this project.
    pub space: Option<String>,
}

#[tool_router]
impl Server {
    pub fn new(cwd: &std::path::Path) -> Result<Self> {
        let db = db::db_path();
        db::init(&db)?;
        let conn = db::connect(&db)?;
        spaces::ensure_builtin(&conn)?;
        let space = spaces::resolve(&conn, cwd)?;
        let visible = spaces::visible(&conn, &space)?;
        let session = format!("mcp:{}:{}", std::process::id(), crate::store::now(&conn)?);
        Ok(Self {
            db,
            space,
            visible,
            session,
            tool_router: Self::tool_router(),
        })
    }

    #[tool(description = "Search memory. Returns one line per item: #id kind space date | title")]
    async fn search(
        &self,
        Parameters(a): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let me = self.clone();
        run(move || me.do_search(a)).await
    }

    #[tool(description = "Full text of items by id, with their links")]
    async fn get(&self, Parameters(a): Parameters<GetArgs>) -> Result<CallToolResult, McpError> {
        let me = self.clone();
        run(move || {
            let conn = db::connect(&me.db)?;
            memory::get(&conn, &a.ids, &me.session, Some(&me.visible))
        })
        .await
    }

    #[tool(description = "Save one durable fact. Refuses near-duplicates until you pick a mode")]
    async fn write(
        &self,
        Parameters(a): Parameters<WriteArgs>,
    ) -> Result<CallToolResult, McpError> {
        let me = self.clone();
        run(move || me.do_write(a)).await
    }
}

impl Server {
    fn do_search(&self, a: SearchArgs) -> Result<String> {
        let conn = db::connect(&self.db)?;
        let scope = Scope::parse(a.scope.as_deref())?;
        let spaces = memory::spaces_for(&conn, &self.space, scope)?;
        let vector = embed::embed_one(&a.q).ok();
        let hits = memory::search(
            &conn,
            &Query {
                text: &a.q,
                vector: vector.as_deref(),
                spaces: &spaces,
                scope,
                kind: a.kind.as_deref(),
                limit: a.limit.unwrap_or(8).clamp(1, 50),
                as_of: None,
            },
        )?;
        memory::log_recall(&conn, &self.session, &self.space, &a.q, &hits)?;
        let mut out: Vec<String> = hits.iter().map(memory::Hit::line).collect();
        if out.is_empty() {
            out.push("nothing found".into());
        }
        if vector.is_none() {
            out.push("(keyword search only: embedding model unavailable)".into());
        }
        Ok(out.join("\n"))
    }

    fn do_write(&self, a: WriteArgs) -> Result<String> {
        let conn = db::connect(&self.db)?;
        let space = a.space.as_deref().unwrap_or(&self.space);
        let vector = embed::embed_one(&index::item_text(a.title.trim(), a.body.trim())).ok();
        let written = memory::write(
            &conn,
            &Write {
                kind: &a.kind,
                title: &a.title,
                body: &a.body,
                space,
                source: a.source.as_deref(),
                valid_until: a.valid_until.as_deref(),
                mode: a.mode.as_deref(),
                vector: vector.as_deref(),
                visible: &self.visible,
                // global items reach every session's core: a person confirms them first
                status: if space == "global" {
                    "proposed"
                } else {
                    "active"
                },
            },
            &self.session,
        )?;
        if !matches!(
            written,
            memory::Written::Similar(_) | memory::Written::Exists(_)
        ) {
            // cores are cheap; global and _shared items, or an edit anywhere, touch many
            if space == "global"
                || space.ends_with("/_shared")
                || matches!(written, memory::Written::Updated(_))
            {
                crate::core::rebuild_all(&conn)?;
            } else {
                crate::core::rebuild(&conn, space)?;
            }
        }
        let mut msg = written.message();
        if space == "global"
            && let memory::Written::Created(id) | memory::Written::Superseded { new: id, .. } =
                written
        {
            crate::review::add(
                &conn,
                "activate",
                &[id],
                &serde_json::json!({}),
                "global write from a session",
                "open",
            )?;
            msg.push_str(
                " (global: waits for review in the engram queue before it reaches every session)",
            );
        }
        Ok(msg)
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for Server {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("engram", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!(
                "{INSTRUCTIONS} This session's space: {}.",
                self.space
            ))
    }
}

/// Runs blocking DB / model work off the async runtime; errors go back to Claude as text.
async fn run(
    f: impl FnOnce() -> Result<String> + Send + 'static,
) -> Result<CallToolResult, McpError> {
    match tokio::task::spawn_blocking(f).await {
        Ok(Ok(text)) => Ok(CallToolResult::success(vec![ContentBlock::text(text)])),
        Ok(Err(e)) => Ok(CallToolResult::error(vec![ContentBlock::text(format!(
            "{e:#}"
        ))])),
        Err(e) => Err(McpError::internal_error(e.to_string(), None)),
    }
}

pub fn serve() -> Result<()> {
    // Claude Code starts the server in the project folder; prefer its explicit hint
    let dir = match std::env::var_os("CLAUDE_PROJECT_DIR") {
        Some(d) => std::path::PathBuf::from(d),
        None => std::env::current_dir()?,
    };
    let server = Server::new(&dir)?;
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    rt.block_on(async {
        let service = server.serve(rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}
