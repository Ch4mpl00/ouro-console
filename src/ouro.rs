//! Translation at the console/runtime boundary. Matching and commands stay in ouro.
use crate::client::*;
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use ouro_durable::{Clock, RunStore};
use ouro_runtime::{control as api, ows};
use serde::Serialize;
use serde_json::Value;
use std::sync::Arc;

pub struct OuroControl {
    control: api::Control,
    read_only: bool,
}
impl OuroControl {
    /// Every observation and command goes through ouro's control API; the
    /// console never reads the store's rows or key layout.
    pub fn new(store: Arc<dyn RunStore>, clock: Arc<dyn Clock>, read_only: bool) -> Self {
        Self {
            control: api::Control::new(store, clock),
            read_only,
        }
    }
    /// Writable adapters migrate on open. Read-only adapters never migrate or
    /// write; they refuse an unmigrated or newer store with ouro's
    /// `SchemaError`, which surfaces as `ControlError::Unavailable`.
    pub async fn connect(spec: &str, read_only: bool) -> Result<Self> {
        let invalid =
            || ControlError::Invalid("store must be sqlite:<path> or postgres:<url>".into());
        let sqlite = spec.strip_prefix("sqlite:").filter(|p| !p.is_empty());
        let postgres = spec.strip_prefix("postgres:").filter(|p| !p.is_empty());
        let store: Arc<dyn RunStore> = if let Some(path) = sqlite {
            let path = path.to_owned();
            let opened = tokio::task::spawn_blocking(move || {
                if read_only {
                    ouro_durable::sqlite::open_read_only(path)
                } else {
                    ouro_durable::sqlite::open(path)
                }
            })
            .await
            .map_err(|e| ControlError::Internal(e.to_string()))?;
            Arc::new(opened.map_err(storage_error)?)
        } else if let Some(url) = postgres {
            let opened = if read_only {
                ouro_durable::postgres::connect_read_only(url).await
            } else {
                ouro_durable::postgres::connect(url).await
            };
            Arc::new(opened.map_err(storage_error)?)
        } else {
            return Err(invalid());
        };
        Ok(Self::new(
            store,
            Arc::new(ouro_durable::SystemClock),
            read_only,
        ))
    }
}

/// Failures from ouro's control API (or a store open), by variant. Unknown
/// storage failures are never classified as retryable.
pub(crate) fn storage_error(error: anyhow::Error) -> ControlError {
    control_error(api::ControlError::from(error))
}
fn control_error(error: api::ControlError) -> ControlError {
    use api::ControlError as E;
    match error {
        E::NotFound { .. } => ControlError::NotFound(error.to_string()),
        E::RequestConflict { .. }
        | E::ResponseConflict { .. }
        | E::InvalidState(_)
        | E::WaitClosed { .. }
        | E::Stopped => ControlError::Conflict(error.to_string()),
        E::WaitMismatch { .. } | E::InvalidInput(_) => ControlError::Invalid(error.to_string()),
        E::ReadOnly => ControlError::ReadOnly,
        E::Storage(source) => match source.downcast_ref::<ouro_durable::SchemaError>() {
            Some(schema) => ControlError::Unavailable(format!("store unavailable: {schema}")),
            // Display preserves only the outer message (e.g. tokio-postgres's "db error").
            // Keep the sources so operators can see the database's actual diagnosis.
            None => ControlError::Internal(format!("{source:#}")),
        },
        // `ControlError` is non-exhaustive: a variant this build does not know.
        other => ControlError::Internal(other.to_string()),
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
        Command::Hold { run, note } => api::Command::Hold {
            run: run.clone(),
            note: note.clone(),
        },
        Command::Release { run } => api::Command::Release { run: run.clone() },
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
        api::Command::Hold { run, note } => Command::Hold { run, note },
        api::Command::Release { run } => Command::Release { run },
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
/// ouro's list queries put a marker where a run's host data is; `inspect` of
/// the run itself returns the data.
fn host(admission: &ouro_durable::Admission) -> Option<HostData> {
    admission.host().map(|data| match data.as_str() {
        Some(api::REDACTED) => HostData::Hidden,
        _ => HostData::Shown(data.clone()),
    })
}
fn saved(run: ouro_durable::SavedRun) -> Result<SavedRun> {
    let interrupted = matches!(run.outcome, Some(ows::run::Status::Interrupted(_)));
    let error = match run.outcome {
        Some(ows::run::Status::Faulted(e) | ows::run::Status::Interrupted(e)) => Some(value(e)?),
        _ => None,
    };
    let host = host(&run.admission);
    Ok(SavedRun {
        id: run.admission.id,
        definition: run.admission.name,
        version: run.admission.version.to_string(),
        state: state(run.state),
        admitted_ms: run.admitted_ms,
        ended_ms: run.ended_ms,
        parent: run.child.map(|c| c.parent),
        error,
        annotations: run.annotations,
        held: run.held.map(|h| Hold {
            actor: h.actor,
            reason: h.reason,
            note: h.note,
            at_ms: h.at_ms,
        }),
        interrupted,
        host,
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
                annotations: call
                    .annotations
                    .into_iter()
                    .map(|(name, a)| {
                        let value = match a {
                            ows::durable::Annotation::Number(n) => Value::Number(n),
                            ows::durable::Annotation::Text(t) => Value::String(t),
                        };
                        (name, value)
                    })
                    .collect(),
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
        annotations: inspection.annotations,
        children: inspection
            .children
            .into_iter()
            .map(saved)
            .collect::<Result<_>>()?,
        budget_spent: inspection.budget_spent,
        budget_limit: inspection.budget_limit,
        trace_id: inspection.trace_id,
        span_id: inspection.span_id,
        pinned: inspection.pinned,
        digest: inspection.digest,
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
            .map_err(control_error)
    }
    async fn stopped(&self) -> Result<bool> {
        self.control.stopped().await.map_err(control_error)
    }
    async fn inspect(&self, run: &str) -> Result<Option<Inspection>> {
        self.control
            .inspect(run)
            .await
            .map_err(control_error)?
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
                .map_err(control_error)?,
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
                .map_err(control_error)?,
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
                .map_err(control_error)?,
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
            .map_err(control_error)?;
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
            .map_err(control_error)?
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
                .map_err(control_error)?,
            |e| Ok(entry(e)),
        )
    }
    async fn definitions(&self, c: Option<&Cursor>, limit: usize) -> Result<Page<Definition>> {
        page(
            self.control
                .definitions(cursor(c)?.as_ref(), limit)
                .await
                .map_err(control_error)?,
            definition,
        )
    }
    async fn definition(&self, name: &str) -> Result<Option<Definition>> {
        self.control
            .definition(name)
            .await
            .map_err(control_error)?
            .map(definition)
            .transpose()
    }
    async fn workers(&self, c: Option<&Cursor>, limit: usize) -> Result<Page<Worker>> {
        page(
            self.control
                .workers(cursor(c)?.as_ref(), limit)
                .await
                .map_err(control_error)?,
            |status| {
                let w = status.worker;
                Ok(Worker {
                    id: w.id,
                    host: w.host,
                    application: w.application,
                    started_ms: w.started_ms,
                    beat_ms: w.beat_ms,
                    expires_ms: w.expires_ms,
                    held: w.held,
                    stale: status.stale,
                })
            },
        )
    }
    async fn totals(
        &self,
        definition: &str,
        since_ms: i64,
        before_ms: i64,
    ) -> Result<DefinitionTotals> {
        let totals = self
            .control
            .totals(definition, since_ms, before_ms)
            .await
            .map_err(control_error)?;
        Ok(DefinitionTotals {
            definition: totals.definition,
            since_ms: totals.since_ms,
            before_ms: totals.before_ms,
            runs: totals.runs,
            annotations: totals.annotations,
        })
    }
}
fn definition(summary: api::DefinitionSummary) -> Result<Definition> {
    let tripped = summary
        .tripped
        .map(|t| {
            let breaker = || -> Result<BreakerTrip> {
                Ok(BreakerTrip {
                    faults: t.faults,
                    threshold: t.threshold,
                    run: t.run.clone(),
                    error: value(&t.reason)?.to_string(),
                    at_ms: t.at_ms,
                })
            };
            match t.manual.clone() {
                // A pause alone has no breaker report; beside a breaker trip
                // it keeps it.
                Some(m) => Ok(Trip::Manual {
                    actor: m.actor,
                    reason: m.reason,
                    note: m.note,
                    at_ms: t.at_ms,
                    digest: t.digest.clone(),
                    breaker: t.breaker.then(breaker).transpose()?,
                }),
                None => Ok(Trip::Automatic {
                    breaker: breaker()?,
                    digest: t.digest.clone(),
                }),
            }
        })
        .transpose()?;
    let schedules = summary
        .schedules
        .into_iter()
        .map(|s| Schedule {
            kind: match s.kind {
                api::ScheduleKind::Every => "every",
                api::ScheduleKind::Cron => "cron",
                api::ScheduleKind::After => "after",
                api::ScheduleKind::At => "at",
                api::ScheduleKind::On => "on",
            }
            .into(),
            next_due_ms: s.next_due_ms,
            overlap: match s.overlap {
                ouro_durable::Overlap::Skip => "skip",
                ouro_durable::Overlap::Allow => "allow",
                ouro_durable::Overlap::Queue => "queue",
                ouro_durable::Overlap::Replace => "replace",
            }
            .into(),
            catchup: match s.catchup {
                api::ScheduleCatchup::Latest => "latest",
                api::ScheduleCatchup::None => "none",
            }
            .into(),
            queued: s.queued.map(|q| QueuedSlot {
                run: q.run,
                version: q.version.to_string(),
            }),
        })
        .collect();
    Ok(Definition {
        name: summary.name,
        versions: summary
            .versions
            .into_iter()
            .map(|v| Version {
                version: v.version.to_string(),
                activated_ms: v.activated_ms,
                activated_by: v.activated_by,
                activation_note: v.activation_note,
                digest: v.digest,
                breaker: match v.breaker {
                    ows::model::Breaker::Inherit => Breaker::Inherit,
                    ows::model::Breaker::Off => Breaker::Off,
                    ows::model::Breaker::Threshold(n) => Breaker::Threshold(n),
                },
            })
            .collect(),
        active: summary.active.map(|v| v.to_string()),
        epoch: summary.epoch,
        faults: summary.consecutive_faults,
        tripped,
        schedules,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use api::{ControlError as E, Entity};

    #[test]
    fn variants_map_to_console_errors() {
        let id = || "x".to_owned();
        let run = || (id(), id());
        assert!(matches!(
            control_error(E::NotFound {
                entity: Entity::Run,
                id: id()
            }),
            ControlError::NotFound(_)
        ));
        for conflict in [
            E::RequestConflict { id: id() },
            E::ResponseConflict { identity: id() },
            E::InvalidState(id()),
            E::WaitClosed {
                run: run().0,
                wait: run().1,
            },
            E::Stopped,
        ] {
            assert!(matches!(control_error(conflict), ControlError::Conflict(_)));
        }
        for invalid in [
            E::InvalidInput(id()),
            E::WaitMismatch {
                run: run().0,
                wait: run().1,
            },
        ] {
            assert!(matches!(control_error(invalid), ControlError::Invalid(_)));
        }
        assert!(matches!(control_error(E::ReadOnly), ControlError::ReadOnly));
    }

    #[test]
    fn schema_errors_are_unavailable_and_other_storage_errors_internal() {
        let schema = anyhow::Error::new(ouro_durable::SchemaError::Unmigrated).context("opening");
        assert!(matches!(
            storage_error(schema),
            ControlError::Unavailable(_)
        ));
        let error = anyhow::Error::new(std::io::Error::other("underlying storage failure"))
            .context("db error")
            .context("reading definitions");
        let error = storage_error(error);
        assert!(matches!(error, ControlError::Internal(_)));
        assert_eq!(
            error.to_string(),
            "reading definitions: db error: underlying storage failure"
        );
    }
}
