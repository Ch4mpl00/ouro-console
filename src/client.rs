//! Console-owned async boundary. The real ouro adapter will translate these DTOs.
use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{collections::BTreeMap, sync::Arc};

pub type Result<T> = std::result::Result<T, ControlError>;

#[derive(Debug, thiserror::Error)]
pub enum ControlError {
    #[error("{0}")]
    Invalid(String),
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Conflict(String),
    #[error("read-only mode: commands are disabled")]
    ReadOnly,
    #[error("{0}")]
    Internal(String),
}

#[derive(
    Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema, clap::ValueEnum,
)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Ready,
    Running,
    Waiting,
    Completed,
    Failed,
    Cancelled,
}
impl RunState {
    pub const ALL: [Self; 6] = [
        Self::Ready,
        Self::Running,
        Self::Waiting,
        Self::Completed,
        Self::Failed,
        Self::Cancelled,
    ];
    pub fn terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::Running => "running",
            Self::Waiting => "waiting",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Request {
    pub id: String,
    pub actor: String,
    pub reason: String,
}
impl Request {
    pub fn validate(&self) -> Result<()> {
        if [&self.id, &self.actor, &self.reason]
            .iter()
            .any(|s| s.trim().is_empty())
        {
            return Err(ControlError::Invalid(
                "request id, actor and reason must be nonempty".into(),
            ));
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "command", rename_all = "snake_case")]
pub enum Command {
    Cancel {
        run: String,
    },
    Pause {
        definition: String,
        note: String,
    },
    Resume {
        definition: String,
    },
    Stop {
        enabled: bool,
    },
    Respond {
        run: String,
        wait: String,
        event: Value,
    },
}
impl Command {
    pub fn run(&self) -> Option<&str> {
        match self {
            Self::Cancel { run } | Self::Respond { run, .. } => Some(run),
            _ => None,
        }
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
pub enum Reply {
    Changed(bool),
    Responded(Option<String>),
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Entry {
    pub request: Request,
    pub command: Command,
    pub at_ms: i64,
    pub reply: Reply,
}
/// Opaque continuation, bound to the full query. Never infer exhaustion from an empty page.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct Cursor(pub String);
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Page<T> {
    pub items: Vec<T>,
    pub next: Option<Cursor>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Runs {
    pub definition: Option<String>,
    pub state: Option<RunState>,
    pub parent: Option<String>,
    pub since_ms: Option<i64>,
    pub before_ms: Option<i64>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct Waits {
    pub definition: Option<String>,
    pub run: Option<String>,
    pub event_type: Option<String>,
    pub since_ms: Option<i64>,
    pub before_ms: Option<i64>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Counts {
    pub states: BTreeMap<String, usize>,
    pub next: Option<Cursor>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SavedRun {
    pub id: String,
    pub definition: String,
    pub version: String,
    pub state: RunState,
    pub admitted_ms: i64,
    pub ended_ms: Option<i64>,
    pub parent: Option<String>,
    pub error: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Lease {
    pub holder: String,
    pub expires_ms: i64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CallEntry {
    pub key: String,
    pub function: String,
    pub delivery: String,
    pub state: String,
    pub arguments: Value,
    pub result: Option<Value>,
}
/// Presentation projection, not a substitute for ouro's expression-based matcher.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PendingWait {
    pub types: Vec<String>,
    pub strategy: String,
    pub filters: Value,
    pub expectations: Value,
    pub progress: Value,
    pub position: u64,
    pub since_ms: i64,
    pub timeout_ms: Option<i64>,
    pub redacted: bool,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Wait {
    pub run: String,
    pub definition: String,
    pub key: String,
    pub waiting: PendingWait,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Inspection {
    pub run: SavedRun,
    pub lease: Option<Lease>,
    pub waits: BTreeMap<String, PendingWait>,
    pub mailbox: Vec<Value>,
    pub calls: Vec<CallEntry>,
    pub children: Vec<SavedRun>,
    pub budget_spent: u64,
    pub budget_limit: Option<u64>,
    pub trace_id: Option<String>,
    pub span_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Trip {
    Manual {
        actor: String,
        reason: String,
        note: String,
    },
    Automatic {
        faults: u32,
        threshold: u32,
        run: String,
        error: String,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Definition {
    pub name: String,
    pub versions: Vec<String>,
    pub active: String,
    pub epoch: u64,
    pub faults: u32,
    pub tripped: Option<Trip>,
    pub schedule: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Worker {
    pub id: String,
    pub heartbeat_ms: i64,
    pub rss_bytes: u64,
    pub cpu_percent: f64,
    pub runs_held: Vec<String>,
}

#[async_trait]
pub trait ControlClient: Send + Sync {
    async fn execute(&self, request: &Request, command: &Command) -> Result<Reply>;
    async fn stopped(&self) -> Result<bool>;
    async fn inspect(&self, run: &str) -> Result<Option<Inspection>>;
    async fn list(
        &self,
        filter: &Runs,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<SavedRun>>;
    async fn history(
        &self,
        filter: &Runs,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<SavedRun>>;
    async fn waits(
        &self,
        filter: &Waits,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<Wait>>;
    async fn counts(
        &self,
        filter: &Runs,
        history: bool,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Counts>;
    async fn entry(&self, id: &str) -> Result<Option<Entry>>;
    async fn journal(
        &self,
        run: Option<&str>,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<Entry>>;
    async fn definitions(&self) -> Result<Vec<Definition>>;
    async fn workers(&self) -> Result<Vec<Worker>>;
    async fn cancel(&self, request: &Request, run: &str) -> Result<Reply> {
        self.execute(request, &Command::Cancel { run: run.into() })
            .await
    }
    async fn cancel_tree(&self, request: &Request, run: &str) -> Result<Reply> {
        self.cancel(request, run).await
    }
    async fn pause(&self, request: &Request, definition: &str, note: &str) -> Result<Reply> {
        self.execute(
            request,
            &Command::Pause {
                definition: definition.into(),
                note: note.into(),
            },
        )
        .await
    }
    async fn resume(&self, request: &Request, definition: &str) -> Result<Reply> {
        self.execute(
            request,
            &Command::Resume {
                definition: definition.into(),
            },
        )
        .await
    }
    async fn emergency_stop(&self, request: &Request, enabled: bool) -> Result<Reply> {
        self.execute(request, &Command::Stop { enabled }).await
    }
    async fn respond(
        &self,
        request: &Request,
        run: &str,
        wait: &str,
        event: Value,
    ) -> Result<Reply> {
        self.execute(
            request,
            &Command::Respond {
                run: run.into(),
                wait: wait.into(),
                event,
            },
        )
        .await
    }
}
/// One authorization/attribution path shared by HTTP, MCP, and scripts.
#[derive(Clone)]
pub struct Session {
    pub client: Arc<dyn ControlClient>,
    pub actor: String,
    pub read_only: bool,
}
impl Session {
    pub async fn execute(
        &self,
        id: Option<String>,
        reason: String,
        command: Command,
    ) -> Result<Entry> {
        if self.read_only {
            return Err(ControlError::ReadOnly);
        }
        let request = Request {
            id: id.unwrap_or_else(|| uuid::Uuid::new_v4().to_string()),
            actor: self.actor.clone(),
            reason,
        };
        request.validate()?;
        let reply = self.client.execute(&request, &command).await?;
        // Return the original attribution and timestamp on an idempotent replay.
        Ok(self.client.entry(&request.id).await?.unwrap_or(Entry {
            request,
            command,
            at_ms: now_ms(),
            reply,
        }))
    }
}
pub fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
