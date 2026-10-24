//! In-memory demo with atomic commands, query-bound cursors and idempotent audit.
use crate::client::*;
use async_trait::async_trait;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Mutex,
};

pub struct FakeControl {
    data: Mutex<Data>,
}
struct Data {
    runs: BTreeMap<String, Inspection>,
    definitions: Vec<Definition>,
    workers: Vec<Worker>,
    stopped: bool,
    journal: BTreeMap<String, Entry>,
    responses: BTreeMap<(String, String), (Command, Reply)>,
}
impl Default for FakeControl {
    fn default() -> Self {
        Self::new()
    }
}
impl FakeControl {
    pub fn new() -> Self {
        let now = now_ms();
        let mut runs = BTreeMap::new();
        for (index, state) in RunState::ALL.into_iter().enumerate() {
            let id = format!("run-{:03}", index + 1);
            let definition = if state == RunState::Failed {
                "archive"
            } else {
                "review"
            };
            let admitted_ms = now - i64::try_from(index + 1).unwrap_or(1) * 90_000;
            runs.insert(
                id.clone(),
                Inspection {
                    run: SavedRun {
                        id: id.clone(),
                        definition: definition.into(),
                        version: "v2".into(),
                        state,
                        admitted_ms,
                        ended_ms: state.terminal().then_some(admitted_ms + 30_000),
                        parent: None,
                        error: (state == RunState::Failed).then(
                            || json!({"type":"timeout","detail":"upstream deadline exceeded"}),
                        ),
                    },
                    lease: (state == RunState::Running).then(|| Lease {
                        holder: "worker-a".into(),
                        expires_ms: now + 30_000,
                    }),
                    waits: BTreeMap::new(),
                    mailbox: vec![],
                    calls: ["completed", "pending", "failed", "not_executed", "unknown"]
                        .into_iter()
                        .enumerate()
                        .map(|(i, s)| CallEntry {
                            key: format!("/tasks/step-{i}"),
                            function: format!("task_{i}"),
                            delivery: if s == "unknown" {
                                "at_most_once"
                            } else {
                                "at_least_once"
                            }
                            .into(),
                            state: s.into(),
                            arguments: json!({"item":"sample"}),
                            result: (s == "completed").then(|| json!({"ok":true})),
                            annotations: if s == "completed" {
                                BTreeMap::from([
                                    ("gen_ai.request.model".into(), json!("model-a")),
                                    ("gen_ai.usage.input_tokens".into(), json!(1200)),
                                    ("gen_ai.usage.output_tokens".into(), json!(340)),
                                    ("cost_usd".into(), json!(0.012)),
                                ])
                            } else {
                                BTreeMap::new()
                            },
                        })
                        .collect(),
                    annotations: BTreeMap::from([
                        ("gen_ai.usage.input_tokens".into(), 1200.into()),
                        ("gen_ai.usage.output_tokens".into(), 340.into()),
                        (
                            "cost_usd".into(),
                            serde_json::Number::from_f64(0.012).expect("finite"),
                        ),
                    ]),
                    children: vec![],
                    budget_spent: 7,
                    budget_limit: Some(100),
                    trace_id: Some("0123456789abcdef0123456789abcdef".into()),
                    span_id: Some("0123456789abcdef".into()),
                },
            );
        }
        let wait = PendingWait {
            types: vec!["review.decision".into()],
            strategy: "all".into(),
            filters: json!({"type":"review.decision"}),
            expectations: json!({}),
            progress: json!({"filled":[],"remaining":1}),
            position: "0".into(),
            since_ms: now - 210_000,
            timeout_ms: Some(now + 3_600_000),
            redacted: false,
        };
        if let Some(run) = runs.get_mut("run-003") {
            run.waits.insert("/decision".into(), wait);
        }
        // A three-level owned tree, including an ended child.
        for (id, parent, state) in [
            ("run-007", "run-002", RunState::Waiting),
            ("run-008", "run-007", RunState::Ready),
            ("run-009", "run-002", RunState::Completed),
        ] {
            let mut child = runs["run-001"].clone();
            child.run.id = id.into();
            child.run.parent = Some(parent.into());
            child.run.state = state;
            child.run.ended_ms = state.terminal().then_some(now - 20_000);
            child.run.definition = "transform".into();
            child.run.version = "v1".into();
            child.lease = None;
            child.calls.clear();
            child.annotations.clear();
            if state == RunState::Waiting {
                child.waits.insert(
                    "/signal".into(),
                    PendingWait {
                        types: vec!["transform.signal".into()],
                        strategy: "any".into(),
                        filters: json!({"type":"transform.signal"}),
                        expectations: json!({"subject":"sample-42"}),
                        progress: json!({"remaining":1}),
                        position: "2".into(),
                        since_ms: now - 60_000,
                        timeout_ms: Some(now + 600_000),
                        redacted: false,
                    },
                );
            }
            runs.insert(id.into(), child);
        }
        let versions = |n: u64| {
            (1..=n)
                .map(|v| Version {
                    version: format!("v{v}"),
                    activated_ms: Some(now - 86_400_000 / i64::try_from(v).unwrap_or(1)),
                })
                .collect::<Vec<_>>()
        };
        let schedule = |kind: &str, next_due_ms: Option<i64>| Schedule {
            kind: kind.into(),
            next_due_ms,
            overlap: "skip".into(),
            catchup: "latest".into(),
            queued: None,
        };
        let definitions = vec![
            Definition {
                name: "review".into(),
                versions: versions(2),
                active: Some("v2".into()),
                epoch: 2,
                faults: 0,
                tripped: None,
                schedules: vec![schedule("every", Some(now + 300_000))],
            },
            Definition {
                name: "transform".into(),
                versions: versions(1),
                active: Some("v1".into()),
                epoch: 1,
                faults: 0,
                tripped: None,
                schedules: vec![],
            },
            Definition {
                name: "archive".into(),
                versions: versions(2),
                active: Some("v2".into()),
                epoch: 3,
                faults: 3,
                tripped: Some(Trip::Automatic {
                    faults: 3,
                    threshold: 3,
                    run: "run-005".into(),
                    error: "upstream deadline exceeded".into(),
                    at_ms: now - 400_000,
                }),
                // An automatic trip disarms the schedule: no next due time.
                schedules: vec![schedule("cron", None)],
            },
            Definition {
                name: "cleanup".into(),
                versions: versions(1),
                active: Some("v1".into()),
                epoch: 1,
                faults: 1,
                tripped: Some(Trip::Manual {
                    actor: "demo-operator".into(),
                    reason: "maintenance window".into(),
                    note: "resume after review".into(),
                    at_ms: now - 300_000,
                }),
                schedules: vec![Schedule {
                    queued: Some(QueuedSlot {
                        run: "cleanup-slot-1".into(),
                        version: "v1".into(),
                    }),
                    ..schedule("every", Some(now + 3_600_000))
                }],
            },
        ];
        let workers = vec![
            Worker {
                id: "worker-a".into(),
                host: Some("host-a".into()),
                application: Some("1.4.0".into()),
                started_ms: now - 7_200_000,
                beat_ms: now - 2_000,
                expires_ms: now + 28_000,
                held: 1,
                stale: false,
            },
            Worker {
                id: "worker-b".into(),
                host: Some("host-b".into()),
                application: Some("1.3.2".into()),
                started_ms: now - 86_000_000,
                beat_ms: now - 180_000,
                expires_ms: now - 150_000,
                held: 0,
                stale: true,
            },
        ];
        let request = Request {
            id: "demo-pause".into(),
            actor: "demo-operator".into(),
            reason: "maintenance window".into(),
        };
        let entry = Entry {
            request: request.clone(),
            command: Command::Pause {
                definition: "cleanup".into(),
                note: "resume after review".into(),
            },
            at_ms: now - 300_000,
            reply: Reply::Changed(true),
        };
        Self {
            data: Mutex::new(Data {
                runs,
                definitions,
                workers,
                stopped: false,
                journal: BTreeMap::from([(request.id, entry)]),
                responses: BTreeMap::new(),
            }),
        }
    }
    fn data(&self) -> Result<std::sync::MutexGuard<'_, Data>> {
        self.data
            .lock()
            .map_err(|_| ControlError::Internal("fake state lock poisoned".into()))
    }
    fn query(
        &self,
        filter: &Runs,
        history: bool,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<SavedRun>> {
        if history && (filter.since_ms.is_none() || filter.before_ms.is_none()) {
            return Err(ControlError::Invalid(
                "history requires a bounded time window".into(),
            ));
        }
        window(filter.since_ms, filter.before_ms)?;
        let data = self.data()?;
        let rows = data
            .runs
            .values()
            .map(|i| &i.run)
            .filter(|r| {
                let time = if history {
                    r.ended_ms
                } else {
                    Some(r.admitted_ms)
                };
                time.is_some_and(|t| in_window(t, filter.since_ms, filter.before_ms))
                    && filter
                        .definition
                        .as_ref()
                        .is_none_or(|d| d == &r.definition)
                    && filter
                        .parent
                        .as_ref()
                        .is_none_or(|p| r.parent.as_ref() == Some(p))
            })
            .map(|r| {
                (
                    key(
                        if history {
                            r.ended_ms.unwrap_or_default()
                        } else {
                            r.admitted_ms
                        },
                        &r.id,
                    ),
                    r.clone(),
                )
            })
            .collect();
        let mut page = paginate(&json!(["runs", history, filter]), rows, cursor, limit)?;
        page.items
            .retain(|r| filter.state.is_none_or(|s| s == r.state));
        Ok(page)
    }
}
#[derive(Serialize, Deserialize)]
struct Position {
    query: Value,
    after: String,
}
fn key(ms: i64, id: &str) -> String {
    format!("{:020}:{id}", i128::from(ms) - i128::from(i64::MIN))
}
fn window(since: Option<i64>, before: Option<i64>) -> Result<()> {
    if since.zip(before).is_some_and(|(s, b)| s >= b) {
        return Err(ControlError::Invalid(
            "since_ms must be before before_ms".into(),
        ));
    }
    Ok(())
}
fn in_window(time: i64, since: Option<i64>, before: Option<i64>) -> bool {
    since.is_none_or(|s| time >= s) && before.is_none_or(|b| time < b)
}
fn paginate<T>(
    query: &Value,
    mut rows: Vec<(String, T)>,
    cursor: Option<&Cursor>,
    limit: usize,
) -> Result<Page<T>> {
    if !(1..=1000).contains(&limit) {
        return Err(ControlError::Invalid("limit must be 1..=1000".into()));
    }
    let after = if let Some(cursor) = cursor {
        let bytes = URL_SAFE_NO_PAD
            .decode(&cursor.0)
            .map_err(|_| ControlError::Invalid("invalid cursor".into()))?;
        let position: Position = serde_json::from_slice(&bytes)
            .map_err(|_| ControlError::Invalid("invalid cursor".into()))?;
        if &position.query != query {
            return Err(ControlError::Invalid(
                "cursor belongs to another query".into(),
            ));
        }
        Some(position.after)
    } else {
        None
    };
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.retain(|(k, _)| after.as_ref().is_none_or(|a| k > a));
    let more = rows.len() > limit;
    rows.truncate(limit);
    let next = if more {
        rows.last().map(|(k, _)| {
            Cursor(
                URL_SAFE_NO_PAD.encode(
                    serde_json::to_vec(&Position {
                        query: query.clone(),
                        after: k.clone(),
                    })
                    .expect("serializable cursor"),
                ),
            )
        })
    } else {
        None
    };
    Ok(Page {
        items: rows.into_iter().map(|(_, v)| v).collect(),
        next,
    })
}
#[async_trait]
impl ControlClient for FakeControl {
    fn backend_label(&self) -> &'static str {
        "Fake backend · demo data resets on restart"
    }
    async fn execute(&self, request: &Request, command: &Command) -> Result<Reply> {
        request.validate()?;
        let mut data = self.data()?;
        if let Some(entry) = data.journal.get(&request.id) {
            if &entry.request != request || &entry.command != command {
                return Err(ControlError::Conflict(
                    "request id reused with different command or attribution".into(),
                ));
            }
            return Ok(entry.reply.clone());
        }
        let reply = match command {
            Command::Cancel { run } => {
                if !data.runs.contains_key(run) {
                    return Err(ControlError::NotFound(format!("run {run} not found")));
                }
                let mut ids = BTreeSet::from([run.clone()]);
                loop {
                    let before = ids.len();
                    for inspect in data.runs.values() {
                        if inspect.run.parent.as_ref().is_some_and(|p| ids.contains(p)) {
                            ids.insert(inspect.run.id.clone());
                        }
                    }
                    if ids.len() == before {
                        break;
                    }
                }
                let mut changed = false;
                for id in ids {
                    if let Some(inspect) = data.runs.get_mut(&id)
                        && !inspect.run.state.terminal()
                    {
                        inspect.run.state = RunState::Cancelled;
                        inspect.run.ended_ms = Some(now_ms());
                        inspect.lease = None;
                        inspect.waits.clear();
                        changed = true;
                    }
                }
                Reply::Changed(changed)
            }
            Command::Pause { definition, note } => {
                let d = data
                    .definitions
                    .iter_mut()
                    .find(|d| &d.name == definition)
                    .ok_or_else(|| ControlError::NotFound("definition not found".into()))?;
                let changed = d.tripped.is_none();
                if changed {
                    d.tripped = Some(Trip::Manual {
                        actor: request.actor.clone(),
                        reason: request.reason.clone(),
                        note: note.clone(),
                        at_ms: now_ms(),
                    });
                }
                Reply::Changed(changed)
            }
            Command::Resume { definition } => {
                let d = data
                    .definitions
                    .iter_mut()
                    .find(|d| &d.name == definition)
                    .ok_or_else(|| ControlError::NotFound("definition not found".into()))?;
                if matches!(d.tripped, Some(Trip::Automatic { .. })) {
                    return Err(ControlError::Conflict(
                        "automatic breaker trip requires activation of a version".into(),
                    ));
                }
                let changed = d.tripped.take().is_some();
                Reply::Changed(changed)
            }
            Command::Stop { enabled } => {
                let changed = data.stopped != *enabled;
                data.stopped = *enabled;
                Reply::Changed(changed)
            }
            Command::Respond { run, wait, event } => {
                let field = |name: &str| {
                    event
                        .get(name)
                        .and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty())
                        .ok_or_else(|| {
                            ControlError::Invalid(format!("event needs nonempty {name}"))
                        })
                };
                let identity = (field("source")?.to_owned(), field("id")?.to_owned());
                field("type")?;
                if let Some((original, reply)) = data.responses.get(&identity) {
                    if original != command {
                        return Err(ControlError::Conflict(
                            "event identity reused with different wait or payload".into(),
                        ));
                    }
                    reply.clone()
                } else {
                    let inspect = data
                        .runs
                        .get_mut(run)
                        .ok_or_else(|| ControlError::NotFound("run not found".into()))?;
                    let pending = inspect.waits.get(wait).ok_or_else(|| {
                        ControlError::Conflict("wait is closed or unknown".into())
                    })?;
                    if pending.timeout_ms.is_some_and(|t| t <= now_ms()) {
                        return Err(ControlError::Conflict("wait timed out".into()));
                    }
                    if pending.redacted {
                        return Err(ControlError::Conflict(
                            "wait requires host secret resolver".into(),
                        ));
                    }
                    if !pending
                        .types
                        .iter()
                        .any(|t| t == "*" || Some(t.as_str()) == event["type"].as_str())
                        || pending
                            .expectations
                            .as_object()
                            .is_some_and(|m| m.iter().any(|(k, v)| event.get(k) != Some(v)))
                    {
                        return Err(ControlError::Invalid(
                            "event does not match persisted wait".into(),
                        ));
                    }
                    inspect.waits.remove(wait);
                    inspect.mailbox.push(event.clone());
                    if inspect.waits.is_empty() {
                        inspect.run.state = RunState::Ready;
                    }
                    let reply = Reply::Responded(Some(run.clone()));
                    data.responses
                        .insert(identity, (command.clone(), reply.clone()));
                    reply
                }
            }
        };
        data.journal.insert(
            request.id.clone(),
            Entry {
                request: request.clone(),
                command: command.clone(),
                at_ms: now_ms(),
                reply: reply.clone(),
            },
        );
        Ok(reply)
    }
    async fn stopped(&self) -> Result<bool> {
        Ok(self.data()?.stopped)
    }
    async fn inspect(&self, run: &str) -> Result<Option<Inspection>> {
        let data = self.data()?;
        Ok(data.runs.get(run).cloned().map(|mut i| {
            i.children = data
                .runs
                .values()
                .filter(|r| r.run.parent.as_deref() == Some(run))
                .map(|r| r.run.clone())
                .collect();
            i
        }))
    }
    async fn list(
        &self,
        filter: &Runs,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<SavedRun>> {
        self.query(filter, false, cursor, limit)
    }
    async fn history(
        &self,
        filter: &Runs,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<SavedRun>> {
        self.query(filter, true, cursor, limit)
    }
    async fn waits(
        &self,
        filter: &Waits,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<Wait>> {
        window(filter.since_ms, filter.before_ms)?;
        let data = self.data()?;
        let rows = data
            .runs
            .values()
            .flat_map(|i| {
                i.waits.iter().map(move |(k, w)| Wait {
                    run: i.run.id.clone(),
                    definition: i.run.definition.clone(),
                    key: k.clone(),
                    waiting: w.clone(),
                })
            })
            .filter(|w| {
                filter.run.as_ref().is_none_or(|r| r == &w.run)
                    && filter
                        .definition
                        .as_ref()
                        .is_none_or(|d| d == &w.definition)
                    && filter
                        .event_type
                        .as_ref()
                        .is_none_or(|t| w.waiting.types.contains(t))
                    && in_window(w.waiting.since_ms, filter.since_ms, filter.before_ms)
            })
            .map(|w| (key(w.waiting.since_ms, &format!("{}:{}", w.run, w.key)), w))
            .collect();
        paginate(&json!(["waits", filter]), rows, cursor, limit)
    }
    async fn counts(
        &self,
        filter: &Runs,
        history: bool,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Counts> {
        if filter.definition.is_none() {
            return Err(ControlError::Invalid("counts require a definition".into()));
        }
        let page = self.query(filter, history, cursor, limit)?;
        let mut states = BTreeMap::new();
        for run in page.items {
            *states.entry(run.state.label().into()).or_default() += 1;
        }
        Ok(Counts {
            states,
            next: page.next,
        })
    }
    async fn entry(&self, id: &str) -> Result<Option<Entry>> {
        Ok(self.data()?.journal.get(id).cloned())
    }
    async fn journal(
        &self,
        run: Option<&str>,
        cursor: Option<&Cursor>,
        limit: usize,
    ) -> Result<Page<Entry>> {
        let data = self.data()?;
        let rows = data
            .journal
            .values()
            .filter(|e| run.is_none_or(|r| e.command.run() == Some(r)))
            .map(|e| (key(e.at_ms, &e.request.id), e.clone()))
            .collect();
        paginate(&json!(["journal", run]), rows, cursor, limit)
    }
    async fn definitions(&self, cursor: Option<&Cursor>, limit: usize) -> Result<Page<Definition>> {
        let rows = self
            .data()?
            .definitions
            .iter()
            .map(|d| (d.name.clone(), d.clone()))
            .collect();
        paginate(&json!(["definitions"]), rows, cursor, limit)
    }
    async fn definition(&self, name: &str) -> Result<Option<Definition>> {
        Ok(self
            .data()?
            .definitions
            .iter()
            .find(|d| d.name == name)
            .cloned())
    }
    async fn workers(&self, cursor: Option<&Cursor>, limit: usize) -> Result<Page<Worker>> {
        let data = self.data()?;
        let now = now_ms();
        let rows = data
            .workers
            .iter()
            .cloned()
            .map(|mut w| {
                // Held runs and staleness follow the demo's leases and clock.
                let held = data
                    .runs
                    .values()
                    .filter(|i| i.lease.as_ref().is_some_and(|l| l.holder == w.id))
                    .count();
                w.held = u64::try_from(held).unwrap_or(u64::MAX);
                w.stale = w.expires_ms <= now;
                (w.id.clone(), w)
            })
            .collect();
        paginate(&json!(["workers"]), rows, cursor, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn timeout_and_redaction_reject_responses_without_audit_or_delivery() {
        for redacted in [false, true] {
            let c = FakeControl::new();
            {
                let mut data = c.data().unwrap();
                let wait = data
                    .runs
                    .get_mut("run-003")
                    .unwrap()
                    .waits
                    .get_mut("/decision")
                    .unwrap();
                if redacted {
                    wait.redacted = true;
                } else {
                    wait.timeout_ms = Some(now_ms() - 1);
                }
            }
            let request = Request {
                id: "refused".into(),
                actor: "tests".into(),
                reason: "reviewed".into(),
            };
            let event = json!({"source":"tests","id":"1","type":"review.decision"});
            assert!(matches!(
                c.respond(&request, "run-003", "/decision", event).await,
                Err(ControlError::Conflict(_))
            ));
            assert!(c.entry("refused").await.unwrap().is_none());
            assert!(
                c.inspect("run-003")
                    .await
                    .unwrap()
                    .unwrap()
                    .mailbox
                    .is_empty()
            );
        }
    }
}
