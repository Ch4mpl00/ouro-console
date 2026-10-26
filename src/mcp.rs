//! rmcp's streamable HTTP transport is mounted by web::router at /mcp.
use crate::client::{Command, ControlError, Cursor, Runs, Session, Waits, totals_window};
use rmcp::{
    ErrorData, ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::*,
    tool, tool_handler, tool_router,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

pub const WRITES: [&str; 9] = [
    "respond",
    "hold_run",
    "release_run",
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
pub struct ListPage {
    pub cursor: Option<Cursor>,
    pub limit: Option<usize>,
}
#[derive(Deserialize, JsonSchema)]
pub struct DefinitionInput {
    pub definition: String,
}
#[derive(Deserialize, JsonSchema)]
pub struct DefinitionTotalsInput {
    pub definition: String,
    /// Window start in epoch milliseconds; defaults to 24 hours before `before_ms`.
    pub since_ms: Option<i64>,
    /// Window end in epoch milliseconds; defaults to now.
    pub before_ms: Option<i64>,
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
pub struct HoldMutation {
    pub run: String,
    /// Optional note shown beside the hold.
    pub note: Option<String>,
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
        description = "Inspect state, hold (who held it), interruption, version digest and pinned flag, host data, lease, waits, mailbox, calls with their annotations, per-run annotation totals (gen_ai.usage.input_tokens/output_tokens), owned children, jump budget and pinned trace/span ids.",
        annotations(read_only_hint = true)
    )]
    async fn inspect_run(
        &self,
        Parameters(p): Parameters<RunInput>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        output(self.session.client.inspect(&p.run).await)
    }
    #[tool(
        description = "Read a run's call journal with delivery, outcome states and per-call annotations.",
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
        description = "Page definitions in name order: versions with content digest and breaker policy, active version, epoch, breaker trip (with digest and manual pause attribution) and schedules with next due times.",
        annotations(read_only_hint = true)
    )]
    async fn list_definitions(
        &self,
        Parameters(p): Parameters<ListPage>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        output(
            self.session
                .client
                .definitions(p.cursor.as_ref(), p.limit.unwrap_or(50))
                .await,
        )
    }
    #[tool(
        description = "Read one definition's summary: versions, active version, epoch, breaker and schedules.",
        annotations(read_only_hint = true)
    )]
    async fn get_definition(
        &self,
        Parameters(p): Parameters<DefinitionInput>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        output(
            self.session
                .client
                .definition(&p.definition)
                .await
                .and_then(|d| {
                    d.ok_or_else(|| ControlError::NotFound("definition not found".into()))
                }),
        )
    }
    #[tool(
        description = "Annotation totals (tokens, cost) summed over a definition's runs that ended in a window (default: the last 24 hours); ouro rounds the window out to whole hours.",
        annotations(read_only_hint = true)
    )]
    async fn get_definition_totals(
        &self,
        Parameters(p): Parameters<DefinitionTotalsInput>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        let (since, before) = totals_window(p.since_ms, p.before_ms);
        output(
            self.session
                .client
                .totals(&p.definition, since, before)
                .await,
        )
    }
    #[tool(
        description = "Page worker heartbeat rows: liveness (stale when no beat before expiry), last beat, runs held, application version and host.",
        annotations(read_only_hint = true)
    )]
    async fn list_workers(
        &self,
        Parameters(p): Parameters<ListPage>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        output(
            self.session
                .client
                .workers(p.cursor.as_ref(), p.limit.unwrap_or(50))
                .await,
        )
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
    #[tool(
        description = "Hold a run and its live owned descendants: nothing is claimed or due until released. A call already dispatched still records its answer. Idempotent; ended runs are left alone."
    )]
    async fn hold_run(
        &self,
        Parameters(p): Parameters<HoldMutation>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.command(
            p.meta,
            Command::Hold {
                run: p.run,
                note: p.note.unwrap_or_default(),
            },
        )
        .await
    }
    #[tool(
        description = "Release a held run and its held descendants; they continue from the same place."
    )]
    async fn release_run(
        &self,
        Parameters(p): Parameters<RunMutation>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.command(p.meta, Command::Release { run: p.run }).await
    }
    #[tool(description = "Cancel the owned run tree atomically; same semantics as cancel_run.")]
    async fn cancel_tree(
        &self,
        Parameters(p): Parameters<RunMutation>,
    ) -> std::result::Result<CallToolResult, ErrorData> {
        self.command(p.meta, Command::Cancel { run: p.run }).await
    }
    #[tool(
        description = "Manually pause admissions and schedules for a definition; existing runs finish. A manual pause refuses every version, whatever its content."
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
            .with_server_info(Implementation::new(
                "ouro-console",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "ouro operations console. Mutations require reason; use request_id for safe retry.",
            )
    }
}
