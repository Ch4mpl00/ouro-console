use crate::{
    client::*,
    fake::FakeControl,
    ouro::OuroControl,
    web::{self, WebConfig},
};
use clap::{Args, Parser, Subcommand};
use serde::Deserialize;
use serde_json::Value;
use std::{net::SocketAddr, path::PathBuf, sync::Arc};

#[derive(Debug, Parser)]
#[command(version, about = "Operations console for ouro")]
pub struct Cli {
    #[arg(
        long,
        global = true,
        conflicts_with = "store",
        help = "Use realistic process-local demo data"
    )]
    pub fake: bool,
    #[arg(
        long,
        global = true,
        env = "OURO_STORE",
        help = "sqlite:<path> or postgres:<url>"
    )]
    pub store: Option<String>,
    #[arg(long, global = true, env = "OURO_CONSOLE_ACTOR")]
    pub actor: Option<String>,
    #[arg(long, global = true, env = "OURO_CONSOLE_TOKEN_FILE")]
    pub token_file: Option<PathBuf>,
    #[arg(long, global = true)]
    pub read_only: bool,
    #[arg(
        long,
        global = true,
        env = "OURO_CONSOLE_TRACE_URL",
        help = "HTTP(S) trace URL template containing {trace_id}"
    )]
    pub trace_url: Option<String>,
    #[command(subcommand)]
    pub command: Action,
}
#[derive(Debug, Subcommand)]
pub enum Action {
    Serve {
        #[arg(long, default_value = "127.0.0.1:8090")]
        bind: SocketAddr,
    },
    Runs(RunArgs),
    History(RunArgs),
    Inspect {
        run: String,
    },
    RunLog {
        run: String,
    },
    Pending(PendingArgs),
    Definitions(PageArgs),
    Definition {
        name: String,
    },
    Workers(PageArgs),
    Journal {
        #[arg(long)]
        run: Option<String>,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Counts {
        #[arg(long)]
        definition: String,
        #[arg(long)]
        history: bool,
        #[arg(long)]
        since_ms: Option<i64>,
        #[arg(long)]
        before_ms: Option<i64>,
        #[arg(long)]
        cursor: Option<String>,
        #[arg(long, default_value_t = 50)]
        limit: usize,
    },
    Status,
    CancelRun {
        run: String,
        #[command(flatten)]
        meta: MutationArgs,
    },
    CancelTree {
        run: String,
        #[command(flatten)]
        meta: MutationArgs,
    },
    PauseDefinition {
        definition: String,
        #[arg(long, default_value = "")]
        note: String,
        #[command(flatten)]
        meta: MutationArgs,
    },
    ResumeDefinition {
        definition: String,
        #[command(flatten)]
        meta: MutationArgs,
    },
    StopAll(MutationArgs),
    ResumeAll(MutationArgs),
    Respond {
        run: String,
        wait: String,
        #[arg(long, help = "Complete matching CloudEvent as JSON")]
        event: Value,
        #[command(flatten)]
        meta: MutationArgs,
    },
}
#[derive(Debug, Args)]
pub struct MutationArgs {
    #[arg(long)]
    pub reason: String,
    #[arg(long)]
    pub request_id: Option<String>,
}
#[derive(Debug, Args)]
pub struct RunArgs {
    #[arg(long)]
    pub definition: Option<String>,
    #[arg(long, value_enum)]
    pub state: Option<RunState>,
    #[arg(long)]
    pub parent: Option<String>,
    #[arg(long)]
    pub since_ms: Option<i64>,
    #[arg(long)]
    pub before_ms: Option<i64>,
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}
impl RunArgs {
    fn filter(&self) -> Runs {
        Runs {
            definition: self.definition.clone(),
            state: self.state,
            parent: self.parent.clone(),
            since_ms: self.since_ms,
            before_ms: self.before_ms,
        }
    }
}
#[derive(Debug, Args)]
pub struct PageArgs {
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}
#[derive(Debug, Args)]
pub struct PendingArgs {
    #[arg(long)]
    pub definition: Option<String>,
    #[arg(long)]
    pub run: Option<String>,
    #[arg(long)]
    pub event_type: Option<String>,
    #[arg(long)]
    pub since_ms: Option<i64>,
    #[arg(long)]
    pub before_ms: Option<i64>,
    #[arg(long)]
    pub cursor: Option<String>,
    #[arg(long, default_value_t = 50)]
    pub limit: usize,
}
/// A file is JSON {"token":"...","actor":"..."}; never serialize it into a page or logs.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TokenConfig {
    pub token: String,
    pub actor: Option<String>,
}
impl TokenConfig {
    pub fn load(path: Option<&std::path::Path>, env_token: Option<String>) -> Result<Option<Self>> {
        let config = if let Some(path) = path {
            let text = std::fs::read_to_string(path)
                .map_err(|e| ControlError::Invalid(format!("cannot read token file: {e}")))?;
            Some(serde_json::from_str::<Self>(&text).map_err(|_| {
                ControlError::Invalid(
                    "token file must be JSON with token and optional actor".into(),
                )
            })?)
        } else {
            env_token.map(|token| Self { token, actor: None })
        };
        if let Some(c) = &config
            && (c.token.is_empty() || c.token.chars().any(char::is_whitespace))
        {
            return Err(ControlError::Invalid(
                "bearer token must be nonempty without whitespace".into(),
            ));
        }
        Ok(config)
    }
}
pub async fn run(cli: Cli) -> Result<()> {
    let auth = TokenConfig::load(
        cli.token_file.as_deref(),
        std::env::var("OURO_CONSOLE_TOKEN").ok(),
    )?;
    let actor = cli
        .actor
        .or_else(|| auth.as_ref().and_then(|c| c.actor.clone()))
        .unwrap_or_else(|| "local-operator".into());
    if actor.trim().is_empty() {
        return Err(ControlError::Invalid("actor must be nonempty".into()));
    }
    let client = connect(cli.fake, cli.store.as_deref(), cli.read_only).await?;
    eprintln!("Backend: {}", client.backend_label());
    let session = Session {
        client,
        actor,
        read_only: cli.read_only,
    };
    if let Action::Serve { bind } = cli.command {
        let config = WebConfig::new(bind, auth.map(|c| c.token), cli.trace_url)?;
        let listener = tokio::net::TcpListener::bind(bind)
            .await
            .map_err(|e| ControlError::Internal(e.to_string()))?;
        eprintln!(
            "ouro-console listening on http://{}",
            listener
                .local_addr()
                .map_err(|e| ControlError::Internal(e.to_string()))?
        );
        axum::serve(listener, web::router(session, config))
            .with_graceful_shutdown(async {
                let _ = tokio::signal::ctrl_c().await;
            })
            .await
            .map_err(|e| ControlError::Internal(e.to_string()))?;
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&execute(&session, cli.command).await?)
                .map_err(|e| ControlError::Internal(e.to_string()))?
        );
    }
    Ok(())
}
/// Compose the same backend for scripts, web and MCP. No implicit demo fallback.
pub async fn connect(
    fake: bool,
    store: Option<&str>,
    read_only: bool,
) -> Result<Arc<dyn ControlClient>> {
    match (fake, store) {
        (true, None) => Ok(Arc::new(FakeControl::new())),
        (false, Some(spec)) => Ok(Arc::new(OuroControl::connect(spec, read_only).await?)),
        _ => Err(ControlError::Invalid(
            "choose --fake or --store sqlite:<path>|postgres:<url> (OURO_STORE)".into(),
        )),
    }
}
fn json<T: serde::Serialize>(value: T) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| ControlError::Internal(e.to_string()))
}
/// Script dispatch is separate from parsing and accepts any ControlClient via Session.
pub async fn execute(session: &Session, action: Action) -> Result<Value> {
    let c = &session.client;
    let (meta, command) = match action {
        Action::Runs(a) => {
            return json(
                c.list(&a.filter(), a.cursor.clone().map(Cursor).as_ref(), a.limit)
                    .await?,
            );
        }
        Action::History(a) => {
            return json(
                c.history(&a.filter(), a.cursor.clone().map(Cursor).as_ref(), a.limit)
                    .await?,
            );
        }
        Action::Inspect { run } => {
            return json(
                c.inspect(&run)
                    .await?
                    .ok_or_else(|| ControlError::NotFound("run not found".into()))?,
            );
        }
        Action::RunLog { run } => {
            return json(
                c.inspect(&run)
                    .await?
                    .ok_or_else(|| ControlError::NotFound("run not found".into()))?
                    .calls,
            );
        }
        Action::Pending(a) => {
            return json(
                c.waits(
                    &Waits {
                        definition: a.definition,
                        run: a.run,
                        event_type: a.event_type,
                        since_ms: a.since_ms,
                        before_ms: a.before_ms,
                    },
                    a.cursor.map(Cursor).as_ref(),
                    a.limit,
                )
                .await?,
            );
        }
        Action::Definitions(p) => {
            return json(
                c.definitions(p.cursor.map(Cursor).as_ref(), p.limit)
                    .await?,
            );
        }
        Action::Definition { name } => {
            return json(
                c.definition(&name)
                    .await?
                    .ok_or_else(|| ControlError::NotFound("definition not found".into()))?,
            );
        }
        Action::Workers(p) => {
            return json(c.workers(p.cursor.map(Cursor).as_ref(), p.limit).await?);
        }
        Action::Journal { run, cursor, limit } => {
            return json(
                c.journal(run.as_deref(), cursor.map(Cursor).as_ref(), limit)
                    .await?,
            );
        }
        Action::Counts {
            definition,
            history,
            since_ms,
            before_ms,
            cursor,
            limit,
        } => {
            return json(
                c.counts(
                    &Runs {
                        definition: Some(definition),
                        since_ms,
                        before_ms,
                        ..Default::default()
                    },
                    history,
                    cursor.map(Cursor).as_ref(),
                    limit,
                )
                .await?,
            );
        }
        Action::Status => return json(c.stopped().await?),
        Action::CancelRun { run, meta } | Action::CancelTree { run, meta } => {
            (meta, Command::Cancel { run })
        }
        Action::PauseDefinition {
            definition,
            note,
            meta,
        } => (meta, Command::Pause { definition, note }),
        Action::ResumeDefinition { definition, meta } => (meta, Command::Resume { definition }),
        Action::StopAll(meta) => (meta, Command::Stop { enabled: true }),
        Action::ResumeAll(meta) => (meta, Command::Stop { enabled: false }),
        Action::Respond {
            run,
            wait,
            event,
            meta,
        } => (meta, Command::Respond { run, wait, event }),
        Action::Serve { .. } => {
            return Err(ControlError::Invalid(
                "serve is handled at the composition root".into(),
            ));
        }
    };
    json(
        session
            .execute(meta.request_id, meta.reason, command)
            .await?,
    )
}
