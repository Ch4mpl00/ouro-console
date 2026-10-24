//! rmcp's streamable HTTP transport is mounted by web::router at /mcp.
use crate::client::{Command, ControlError, Cursor, Runs, Session, Waits};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub const WRITES: [&str; 7] = [
    "respond",
    "cancel_run",
    "cancel_tree",
    "pause_definition",
    "resume_definition",
    "stop_all",
    "resume_all",
];
#[derive(Clone)]
pub struct Mcp {
    session: Session,
    router: ToolRouter<Self>,
}
impl Mcp {
    pub fn new(session: Session) -> Self {
        let mut router = Self::tool_router();
        if session.read_only {
            for name in WRITES {
                router.remove_route(name);
            }
        }
        Self { session, router }
    }
    pub fn tools(&self) -> Vec<Tool> {
        self.router.list_all()
    }
    async fn command(
        &self,
        meta: Mutation,
        command: Command,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        output(
            self.session
                .execute(meta.request_id, meta.reason, command)
                .await,
        )
    }
}
#[derive(Deserialize, JsonSchema)]
pub struct ListRuns {
    #[serde(flatten)]
    pub filter: Runs,
    pub cursor: Option<Cursor>,
    pub limit: Option<usize>,
    #[serde(default)]
    pub history: bool,
}
#[derive(Deserialize, JsonSchema)]
pub struct ListPending {
    #[serde(flatten)]
    pub filter: Waits,
    pub cursor: Option<Cursor>,
    pub limit: Option<usize>,
}
#[derive(Deserialize, JsonSchema)]
pub struct RunInput {
    pub run: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct Mutation {
    /// Required operator justification, not just whitespace.
    pub reason: String,
    /// Reuse the same ID when retrying an ambiguous reply.
    pub request_id: Option<String>,
}
#[derive(Deserialize, JsonSchema)]
pub struct RunMutation {
    pub run: String,
    #[serde(flatten)]
    pub meta: Mutation,
}
#[derive(Deserialize, JsonSchema)]
pub struct DefinitionMutation {
    pub definition: String,
    pub note: Option<String>,
    #[serde(flatten)]
    pub meta: Mutation,
}
#[derive(Deserialize, JsonSchema)]
pub struct RespondInput {
    pub run: String,
    pub wait: String,
    /// Complete CloudEvent: source, id, type and data; must match the persisted wait.
    pub event: Value,
    #[serde(flatten)]
    pub meta: Mutation,
}
fn output<T: serde::Serialize>(
    result: crate::client::Result<T>,
) -> std::result::Result<CallToolResult, ErrorData> {
    match result {
        Ok(value) => Ok(CallToolResult::success(vec![
            ContentBlock::json(value)
                .map_err(|e| ErrorData::internal_error(e.to_string(), None))?,
        ])),
        Err(e) => Ok(CallToolResult::error(vec![ContentBlock::text(
            e.to_string(),
        )])),
    }
}
#[tool_router]
impl Mcp {
    #[tool(
        description = "Page runs by definition, state, parent and admission time; history requires since_ms and before_ms.",
        annotations(read_only_hint = true)
    )]
    async fn list_runs(
        &self,
        Parameters(p): Parameters<ListRuns>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let client = &self.session.client;
        output(if p.history {
            client
                .history(&p.filter, p.cursor.as_ref(), p.limit.unwrap_or(50))
                .await
        } else {
            client
                .list(&p.filter, p.cursor.as_ref(), p.limit.unwrap_or(50))
                .await
        })
    }
    #[tool(
        description = "Inspect state, lease, waits, mailbox, calls, owned children and jump budget.",
        annotations(read_only_hint = true)
    )]
    async fn inspect_run(
        &self,
        Parameters(p): Parameters<RunInput>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        output(self.session.client.inspect(&p.run).await)
    }
    #[tool(
        description = "Read a run's call journal with delivery and outcome states.",
        annotations(read_only_hint = true)
    )]
    async fn run_log(
        &self,
        Parameters(p): Parameters<RunInput>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        output(self.session.client.inspect(&p.run).await.and_then(|i| {
            i.map(|i| i.calls)
                .ok_or_else(|| ControlError::NotFound("run not found".into()))
        }))
    }
    #[tool(
        description = "Page pending listens with filters, correlation expectations, progress and deadlines.",
        annotations(read_only_hint = true)
    )]
    async fn list_pending(
        &self,
        Parameters(p): Parameters<ListPending>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        output(
            self.session
                .client
                .waits(&p.filter, p.cursor.as_ref(), p.limit.unwrap_or(50))
                .await,
        )
    }
    #[tool(
        description = "Respond to the exact persisted wait with a matching CloudEvent; records actor and reason."
    )]
    async fn respond(
        &self,
        Parameters(p): Parameters<RespondInput>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.command(
            p.meta,
            Command::Respond {
                run: p.run,
                wait: p.wait,
                event: p.event,
            },
        )
        .await
    }
    #[tool(
        description = "List immutable definition versions, active version, breaker, manual pause and schedule.",
        annotations(read_only_hint = true)
    )]
    async fn list_definitions(&self) -> std::result::Result<CallToolResult, ErrorData> {
        output(self.session.client.definitions().await)
    }
    #[tool(
        description = "List worker heartbeats, RSS, CPU and leased runs.",
        annotations(read_only_hint = true)
    )]
    async fn list_workers(&self) -> std::result::Result<CallToolResult, ErrorData> {
        output(self.session.client.workers().await)
    }
    #[tool(
        description = "Cancel a run and all transitive owned descendants atomically. Cannot retract dispatched effects."
    )]
    async fn cancel_run(
        &self,
        Parameters(p): Parameters<RunMutation>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.command(p.meta, Command::Cancel { run: p.run }).await
    }
    #[tool(description = "Cancel the owned run tree atomically; same semantics as cancel_run.")]
    async fn cancel_tree(
        &self,
        Parameters(p): Parameters<RunMutation>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.command(p.meta, Command::Cancel { run: p.run }).await
    }
    #[tool(
        description = "Manually pause admissions and schedules for a definition; existing runs finish."
    )]
    async fn pause_definition(
        &self,
        Parameters(p): Parameters<DefinitionMutation>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.command(
            p.meta,
            Command::Pause {
                definition: p.definition,
                note: p.note.unwrap_or_default(),
            },
        )
        .await
    }
    #[tool(
        description = "Clear a manual pause. An automatic breaker trip requires version activation."
    )]
    async fn resume_definition(
        &self,
        Parameters(p): Parameters<DefinitionMutation>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.command(
            p.meta,
            Command::Resume {
                definition: p.definition,
            },
        )
        .await
    }
    #[tool(
        description = "Set the store-wide emergency stop. Workers release for recovery at a boundary or heartbeat."
    )]
    async fn stop_all(
        &self,
        Parameters(p): Parameters<Mutation>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.command(p, Command::Stop { enabled: true }).await
    }
    #[tool(
        description = "Clear the store-wide emergency stop; resume work from retained evidence."
    )]
    async fn resume_all(
        &self,
        Parameters(p): Parameters<Mutation>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.command(p, Command::Stop { enabled: false }).await
    }
}
#[tool_handler(router = self.router)]
impl ServerHandler for Mcp {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("ouro-console", env!("CARGO_PKG_VERSION")))
            .with_instructions("ouro operations console. Fake data is process-local and resets on restart. Mutations require reason; use request_id for safe retry.")
    }
}
