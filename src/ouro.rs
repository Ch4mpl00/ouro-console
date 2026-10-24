//! Translation at the console/runtime boundary. Matching and commands stay in ouro.
use crate::client::*;
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ouro_durable::{AtomicBackend, Clock, RunStore, TransactionalStore};
use ouro_runtime::{control as api, ows};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

pub struct OuroControl {
    control: api::Control,
    store: Arc<dyn RunStore>,
    catalog: Arc<dyn AtomicBackend>,
    read_only: bool,
}
impl OuroControl {
    /// One backend supplies both the control service and the definition catalog.
    pub fn new(backend: Arc<dyn AtomicBackend>, clock: Arc<dyn Clock>, read_only: bool) -> Self {
        let store: Arc<dyn RunStore> = Arc::new(TransactionalStore::new(backend.clone()));
        Self {
            control: api::Control::new(store.clone(), clock),
            store,
            catalog: backend,
            read_only,
        }
    }
    /// Writable adapters migrate on open. Read-only adapters require an existing schema.
    pub async fn connect(spec: &str, read_only: bool) -> Result<Self> {
        if spec
            .strip_prefix("sqlite:")
            .or_else(|| spec.strip_prefix("postgres:"))
            .is_none_or(|s| s.is_empty())
        {
            return Err(ControlError::Invalid(
                "store must be sqlite:<path> or postgres:<url>".into(),
            ));
        }
        let backend: Arc<dyn AtomicBackend> = if read_only {
            Arc::new(
                crate::readonly::ReadBackend::open(spec)
                    .await
                    .map_err(storage_error)?,
            )
        } else if let Some(path) = spec.strip_prefix("sqlite:").filter(|p| !p.is_empty()) {
            let path = path.to_owned();
            Arc::new(
                tokio::task::spawn_blocking(move || {
                    ouro_durable::sqlite::SqliteBackend::open(path)
                })
                .await
                .map_err(|e| ControlError::Internal(e.to_string()))?
                .map_err(storage_error)?,
            )
        } else if let Some(url) = spec.strip_prefix("postgres:").filter(|p| !p.is_empty()) {
            Arc::new(
                ouro_durable::postgres::PostgresBackend::connect(url)
                    .await
                    .map_err(storage_error)?,
            )
        } else {
            return Err(ControlError::Invalid(
                "store must be sqlite:<path> or postgres:<url>".into(),
            ));
        };
        Ok(Self::new(
            backend,
            Arc::new(ouro_durable::SystemClock),
            read_only,
        ))
    }
}

// At this pin operator errors are anyhow strings, not exported typed errors.
// Match only known messages; never classify unknown I/O/ambiguous commits as retryable.
pub(crate) fn storage_error(error: anyhow::Error) -> ControlError {
    let message = error.to_string();
    match message.as_str() {
        "control request identity conflict"
        | "response event identity conflict"
        | "event identity conflict"
        | "a breaker trip requires activation"
        | "wait is no longer pending"
        | "wait ended or timed out"
        | "response event already consumed by another wait"
        | "wait matching requires redacted secrets" => ControlError::Conflict(message),
        "unknown definition" => ControlError::NotFound(message),
        "control request requires id, actor and reason"
        | "response requires event source and id"
        | "event does not match wait"
        | "definition has no active version"
        | "history requires a bounded time window"
        | "counts require a definition"
        | "invalid time window"
        | "cursor belongs to another query" => ControlError::Invalid(message),
        _ if message.starts_with("unknown run ") => ControlError::NotFound(message),
        _ if message.starts_with("page limit must be ") => ControlError::Invalid(message),
        _ => ControlError::Internal(message),
    }
}
fn value(value: impl Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| ControlError::Internal(e.to_string()))
}
// The cursor's structure belongs to ouro. Encode/decode its serialization only.
fn cursor(cursor: Option<&Cursor>) -> Result<Option<api::Cursor>> {
    cursor
        .map(|c| {
            let raw = URL_SAFE_NO_PAD
                .decode(&c.0)
                .map_err(|_| ControlError::Invalid("invalid cursor".into()))?;
            serde_json::from_slice(&raw).map_err(|_| ControlError::Invalid("invalid cursor".into()))
        })
        .transpose()
}
fn next(cursor: Option<api::Cursor>) -> Result<Option<Cursor>> {
    cursor
        .map(|c| {
            serde_json::to_vec(&c)
                .map(|v| Cursor(URL_SAFE_NO_PAD.encode(v)))
                .map_err(|e| ControlError::Internal(e.to_string()))
        })
        .transpose()
}
fn page<T, U>(page: api::Page<T>, convert: impl Fn(T) -> Result<U>) -> Result<Page<U>> {
    Ok(Page {
        items: page.items.into_iter().map(convert).collect::<Result<_>>()?,
        next: next(page.next)?,
    })
}
fn command(command: &Command) -> api::Command {
    match command {
        Command::Cancel { run } => api::Command::Cancel { run: run.clone() },
        Command::Pause { definition, note } => api::Command::Pause {
            definition: definition.clone(),
            note: note.clone(),
        },
        Command::Resume { definition } => api::Command::Resume {
            definition: definition.clone(),
        },
        Command::Stop { enabled } => api::Command::Stop { enabled: *enabled },
        Command::Respond { run, wait, event } => api::Command::Respond {
            run: run.clone(),
            wait: wait.clone(),
            event: event.clone(),
        },
    }
}
fn reply(reply: api::Reply) -> Reply {
    match reply {
        api::Reply::Changed(v) => Reply::Changed(v),
        api::Reply::Responded(v) => Reply::Responded(v),
    }
}
fn entry(entry: api::Entry) -> Entry {
    let command = match entry.command {
        api::Command::Cancel { run } => Command::Cancel { run },
        api::Command::Pause { definition, note } => Command::Pause { definition, note },
        api::Command::Resume { definition } => Command::Resume { definition },
        api::Command::Stop { enabled } => Command::Stop { enabled },
        api::Command::Respond { run, wait, event } => Command::Respond { run, wait, event },
    };
    Entry {
        request: Request {
            id: entry.request.id,
            actor: entry.request.actor,
            reason: entry.request.reason,
        },
        command,
        at_ms: entry.at_ms,
        reply: reply(entry.reply),
    }
}
fn state(state: ouro_durable::RunState) -> RunState {
    match state {
        ouro_durable::RunState::Ready => RunState::Ready,
        ouro_durable::RunState::Running => RunState::Running,
        ouro_durable::RunState::Waiting => RunState::Waiting,
        ouro_durable::RunState::Completed => RunState::Completed,
        ouro_durable::RunState::Failed => RunState::Failed,
        ouro_durable::RunState::Cancelled => RunState::Cancelled,
    }
}
fn runs(filter: &Runs) -> api::Runs {
    api::Runs {
        definition: filter.definition.clone(),
        state: filter.state.map(|s| match s {
            RunState::Ready => ouro_durable::RunState::Ready,
            RunState::Running => ouro_durable::RunState::Running,
            RunState::Waiting => ouro_durable::RunState::Waiting,
            RunState::Completed => ouro_durable::RunState::Completed,
            RunState::Failed => ouro_durable::RunState::Failed,
            RunState::Cancelled => ouro_durable::RunState::Cancelled,
        }),
        parent: filter.parent.clone(),
        since_ms: filter.since_ms,
        before_ms: filter.before_ms,
    }
}
fn saved(run: ouro_durable::SavedRun) -> Result<SavedRun> {
    let error = match run.outcome {
        Some(ows::run::Status::Faulted(e) | ows::run::Status::Interrupted(e)) => Some(value(e)?),
        _ => None,
    };
    Ok(SavedRun {
        id: run.admission.id,
        definition: run.admission.name,
        version: run.admission.version.to_string(),
        state: state(run.state),
        admitted_ms: run.admitted_ms,
        ended_ms: run.ended_ms,
        parent: run.child.map(|c| c.parent),
        error,
    })
}
fn waiting(wait: ows::events::PendingWait) -> Result<PendingWait> {
    let strategy = match &wait.strategy {
        ows::model::Strategy::One(_) => "one",
        ows::model::Strategy::Any { .. } => "any",
        ows::model::Strategy::All(_) => "all",
    };
    Ok(PendingWait {
        types: wait.types,
        strategy: strategy.into(),
        filters: value(wait.strategy)?,
        expectations: value(&wait.progress.expectations)?,
        progress: value(wait.progress)?,
        position: wait.position,
        since_ms: wait.since_ms,
        timeout_ms: wait.timeout_ms,
        redacted: wait.redacted,
    })
}
fn inspection(inspection: api::Inspection) -> Result<Inspection> {
    use ows::durable::{CallState, Record};
    let snapshot = inspection.snapshot;
    let lease = snapshot
        .run
        .fence
        .as_ref()
        .zip(snapshot.run.lease_until_ms)
        .map(|(f, expires_ms)| Lease {
            holder: f.owner.clone(),
            expires_ms,
        });
    let calls = inspection
        .calls
        .into_iter()
        .map(|call| {
            let (arguments, result) = match snapshot.records.get(&call.key) {
                Some(Record::Call {
                    arguments, state, ..
                }) => {
                    let result = match state {
                        CallState::Pending => None,
                        CallState::Done(Ok(v)) => Some(v.clone()),
                        CallState::Done(Err(e)) | CallState::NotExecuted(e) => Some(value(e)?),
                    };
                    (arguments.clone(), result)
                }
                _ => {
                    return Err(ControlError::Internal(
                        "call journal has no corresponding record".into(),
                    ));
                }
            };
            Ok(CallEntry {
                key: call.key,
                function: call.function,
                delivery: match call.delivery {
                    ows::durable::Delivery::AtLeastOnce => "at_least_once",
                    ows::durable::Delivery::AtMostOnce => "at_most_once",
                }
                .into(),
                state: match call.state {
                    ows::durable::CallStatus::Pending => "pending",
                    ows::durable::CallStatus::Completed => "completed",
                    ows::durable::CallStatus::Failed => "failed",
                    ows::durable::CallStatus::NotExecuted => "not_executed",
                    ows::durable::CallStatus::Unknown => "unknown",
                }
                .into(),
                arguments,
                result,
            })
        })
        .collect::<Result<_>>()?;
    Ok(Inspection {
        run: saved(snapshot.run)?,
        lease,
        waits: inspection
            .waits
            .into_iter()
            .map(|(k, w)| Ok((k, waiting(w)?)))
            .collect::<Result<_>>()?,
        mailbox: snapshot
            .events
            .into_iter()
            .map(value)
            .collect::<Result<_>>()?,
        calls,
        children: inspection
            .children
            .into_iter()
            .map(saved)
            .collect::<Result<_>>()?,
        budget_spent: inspection.budget_spent,
        budget_limit: inspection.budget_limit,
        trace_id: inspection.trace_id,
        span_id: inspection.span_id,
    })
}
#[async_trait]
impl ControlClient for OuroControl {
    fn backend_label(&self) -> &'static str {
        "ouro store"
    }
    async fn execute(&self, request: &Request, cmd: &Command) -> Result<Reply> {
        if self.read_only {
            return Err(ControlError::ReadOnly);
        }
        request.validate()?;
        self.control
            .execute(
                &api::Request {
                    id: request.id.clone(),
                    actor: request.actor.clone(),
                    reason: request.reason.clone(),
                },
                &command(cmd),
            )
            .await
            .map(reply)
            .map_err(storage_error)
    }
    async fn stopped(&self) -> Result<bool> {
        self.control.stopped().await.map_err(storage_error)
    }
    async fn inspect(&self, run: &str) -> Result<Option<Inspection>> {
        self.control
            .inspect(run)
            .await
            .map_err(storage_error)?
            .map(inspection)
            .transpose()
    }
    async fn list(
        &self,
        filter: &Runs,
        c: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<SavedRun>> {
        page(
            self.control
                .list(&runs(filter), cursor(c)?.as_ref(), limit)
                .await
                .map_err(storage_error)?,
            saved,
        )
    }
    async fn history(
        &self,
        filter: &Runs,
        c: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<SavedRun>> {
        page(
            self.control
                .history(&runs(filter), cursor(c)?.as_ref(), limit)
                .await
                .map_err(storage_error)?,
            saved,
        )
    }
    async fn waits(&self, filter: &Waits, c: Option<&Cursor>, limit: usize) -> Result<Page<Wait>> {
        let filter = api::Waits {
            definition: filter.definition.clone(),
            run: filter.run.clone(),
            event_type: filter.event_type.clone(),
            since_ms: filter.since_ms,
            before_ms: filter.before_ms,
        };
        page(
            self.control
                .waits(&filter, cursor(c)?.as_ref(), limit)
                .await
                .map_err(storage_error)?,
            |w| {
                Ok(Wait {
                    run: w.run,
                    definition: w.definition,
                    key: w.key,
                    waiting: waiting(w.waiting)?,
                })
            },
        )
    }
    async fn counts(
        &self,
        filter: &Runs,
        history: bool,
        c: Option<&Cursor>,
        limit: usize,
    ) -> Result<Counts> {
        let counts = self
            .control
            .counts(&runs(filter), history, cursor(c)?.as_ref(), limit)
            .await
            .map_err(storage_error)?;
        Ok(Counts {
            states: counts.states,
            next: next(counts.next)?,
        })
    }
    async fn entry(&self, id: &str) -> Result<Option<Entry>> {
        Ok(self
            .control
            .entry(id)
            .await
            .map_err(storage_error)?
            .map(entry))
    }
    async fn journal(
        &self,
        run: Option<&str>,
        c: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<Entry>> {
        page(
            self.control
                .journal(run, cursor(c)?.as_ref(), limit)
                .await
                .map_err(storage_error)?,
            |e| Ok(entry(e)),
        )
    }
    async fn definitions(&self) -> Result<Vec<Definition>> {
        // RunStore exposes definitions(name), but no name enumeration at this pin.
        // Scan only catalog keys in bounded chunks; decode versions through RunStore.
        let mut tx = self.catalog.begin().await.map_err(storage_error)?;
        let mut names = Vec::new();
        let mut start = String::new();
        loop {
            let rows = tx
                .scan("definitions", &start, None, 256)
                .await
                .map_err(storage_error)?;
            if rows.is_empty() {
                break;
            }
            for ((_, name), _) in rows {
                start = format!("{name}\0");
                names.push(name);
            }
        }
        tx.rollback().await;
        let mut result = Vec::new();
        for name in names {
            let definitions = self.store.definitions(&name).await.map_err(storage_error)?;
            let schedule = definitions
                .active
                .and_then(|v| definitions.versions.get(&v))
                .map(|d| {
                    let workflow = ows::model::parse_yaml(&d.yaml)
                        .map_err(|e| ControlError::Internal(e.to_string()))?;
                    Ok(workflow.definition.get("schedule").map(Value::to_string))
                })
                .transpose()?
                .flatten();
            let tripped = definitions
                .tripped
                .map(|t| match t.manual {
                    Some(m) => Ok(Trip::Manual {
                        actor: m.actor,
                        reason: m.reason,
                        note: m.note,
                    }),
                    None => Ok(Trip::Automatic {
                        faults: t.faults,
                        threshold: t.threshold,
                        run: t.run,
                        error: value(t.reason)?.to_string(),
                    }),
                })
                .transpose()?;
            result.push(Definition {
                name,
                versions: definitions
                    .versions
                    .keys()
                    .map(ToString::to_string)
                    .collect(),
                active: definitions
                    .active
                    .map_or_else(|| "none".into(), |v| v.to_string()),
                epoch: definitions.epoch,
                faults: definitions.consecutive_faults,
                tripped,
                schedule,
            });
        }
        Ok(result)
    }
    async fn workers(&self) -> Result<Workers> {
        Ok(Workers::Unavailable {
            reason: "Worker heartbeats are not available yet (ouro#20).".into(),
        })
    }
}
