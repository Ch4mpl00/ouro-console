#![allow(dead_code)] // Shared helpers are compiled independently by each integration suite.
use ouro_console::{client::*, fake::FakeControl, ouro::OuroControl};
use ouro_durable::{AtomicBackend, Clock, RunStore, TransactionalStore};
use ouro_runtime::{
    Config, Function, FunctionContext, Output, Registry, Runtime,
    control::Control,
    durable::Compatibility,
    opentelemetry::{
        Context,
        trace::{FutureExt, SpanContext, SpanId, TraceContextExt, TraceFlags, TraceId, TraceState},
    },
    ows::run::Status,
};
use serde_json::{Value, json};
use std::{
    ops::Deref,
    sync::{Arc, Mutex},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub enum Backend {
    Fake,
    Sqlite,
    Postgres {
        url: String,
        schemas: Arc<Mutex<Vec<String>>>,
    },
}
impl Backend {
    pub fn postgres() -> Self {
        Self::Postgres {
            url: std::env::var("TEST_DATABASE_URL")
                .expect("TEST_DATABASE_URL is required for ignored Postgres tests"),
            schemas: Arc::default(),
        }
    }
    pub async fn cleanup(&self) {
        if let Self::Postgres { url, schemas } = self {
            let (client, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
                .await
                .unwrap();
            let task = tokio::spawn(async move {
                connection.await.unwrap();
            });
            let names = std::mem::take(&mut *schemas.lock().unwrap());
            for name in names {
                client
                    .batch_execute(&format!("DROP SCHEMA {name} CASCADE"))
                    .await
                    .unwrap();
            }
            drop(client);
            task.await.unwrap();
        }
    }
}
/// The span context run-002 and run-003 are admitted in (the fake reports the same).
pub const TRACE_ID: &str = "0123456789abcdef0123456789abcdef";
pub const SPAN_ID: &str = "0123456789abcdef";
/// What every `echo` call answers beside its result, under the GenAI names.
pub const INPUT_TOKENS: i64 = 12;
pub const OUTPUT_TOKENS: i64 = 5;
/// A clock behind real time, so a runtime's heartbeat is already stale.
struct Behind(i64);
impl Clock for Behind {
    fn now_ms(&self) -> i64 {
        ouro_durable::SystemClock.now_ms() - self.0
    }
}
pub struct Fixture {
    pub client: Arc<dyn ControlClient>,
    /// Heartbeat rows: one that just beat and one that stopped beating.
    pub live_worker: String,
    pub stale_worker: String,
    pub real: Option<Real>,
    pub wait_key: String,
    pub spec: Option<String>,
    _directory: Option<tempfile::TempDir>,
}
pub struct Real {
    pub runtime: Runtime,
    pub control: Control,
    pub backend: Arc<dyn AtomicBackend>,
    pub store: Arc<dyn RunStore>,
}
impl Deref for Fixture {
    type Target = dyn ControlClient;
    fn deref(&self) -> &Self::Target {
        self.client.as_ref()
    }
}
impl Fixture {
    pub async fn new(kind: &Backend) -> Self {
        let (backend, spec, directory): (Arc<dyn AtomicBackend>, _, _) = match kind {
            Backend::Fake => {
                return Self {
                    client: Arc::new(FakeControl::new()),
                    live_worker: "worker-a".into(),
                    stale_worker: "worker-b".into(),
                    real: None,
                    wait_key: "/decision".into(),
                    spec: None,
                    _directory: None,
                };
            }
            Backend::Sqlite => {
                let dir = tempfile::tempdir().unwrap();
                let path = dir.path().join("runs.sqlite");
                let backend = ouro_durable::sqlite::SqliteBackend::open(&path).unwrap();
                (
                    Arc::new(backend),
                    format!("sqlite:{}", path.display()),
                    Some(dir),
                )
            }
            Backend::Postgres { url, schemas } => {
                // Each fixture owns a schema; never truncate an existing runtime store.
                let schema = format!("console_test_{}", uuid::Uuid::new_v4().simple());
                let (admin, connection) = tokio_postgres::connect(url, tokio_postgres::NoTls)
                    .await
                    .unwrap();
                let task = tokio::spawn(async move {
                    connection.await.unwrap();
                });
                admin
                    .batch_execute(&format!("CREATE SCHEMA {schema}"))
                    .await
                    .unwrap();
                drop(admin);
                task.await.unwrap();
                schemas.lock().unwrap().push(schema.clone());
                let mut parsed = url::Url::parse(url).unwrap();
                parsed
                    .query_pairs_mut()
                    .append_pair("options", &format!("-csearch_path={schema}"));
                let spec = format!("postgres:{parsed}");
                (
                    Arc::new(
                        ouro_durable::postgres::PostgresBackend::connect(parsed.as_str())
                            .await
                            .unwrap(),
                    ),
                    spec,
                    None,
                )
            }
        };
        let clock: Arc<dyn Clock> = Arc::new(ouro_durable::SystemClock);
        let store: Arc<dyn RunStore> = Arc::new(TransactionalStore::new(backend.clone()));
        let mut registry = Registry::new();
        registry
            .register(Function::new(
                "echo",
                json!({}),
                json!({}),
                |v: Value, _: FunctionContext| async {
                    Ok(Output::new(v)
                        .annotate("gen_ai.request.model", "model-a")
                        .annotate("gen_ai.usage.input_tokens", INPUT_TOKENS)
                        .annotate("gen_ai.usage.output_tokens", OUTPUT_TOKENS))
                },
            ))
            .unwrap();
        let registry = Arc::new(registry);
        let tracer = || {
            ouro_runtime::opentelemetry::global::BoxedTracer::new(Box::new(
                ouro_runtime::opentelemetry::trace::noop::NoopTracer::new(),
            ))
        };
        let config = |host: &str, version: &str| Config {
            breaker_threshold: 1,
            host_name: Some(host.into()),
            application: Compatibility {
                version: Some(version.into()),
                accepts: vec![],
            },
            ..Config::default()
        };
        let runtime = Runtime::new(
            store.clone(),
            registry.clone(),
            config("host-a", "1.4.0"),
            clock.clone(),
            tracer(),
        )
        .unwrap();
        runtime.heartbeat().await.unwrap();
        // A second worker beat ten minutes ago and never again.
        let gone = Runtime::new(
            store.clone(),
            registry,
            config("host-b", "1.3.2"),
            Arc::new(Behind(600_000)),
            tracer(),
        )
        .unwrap();
        gone.heartbeat().await.unwrap();
        let admitted = Context::new().with_remote_span_context(SpanContext::new(
            TraceId::from_hex(TRACE_ID).unwrap(),
            SpanId::from_hex(SPAN_ID).unwrap(),
            TraceFlags::SAMPLED,
            true,
            TraceState::default(),
        ));
        let workflow = |body: &str| {
            format!(
                "document: {{ dsl: '1.0.4', namespace: test, name: workflow, version: '1.0.0' }}\ndo:\n{body}\n"
            )
        };
        let review = workflow(
            "  - echo: { call: echo, with: { marker: original } }\n  - decision: { listen: { to: { one: { with: { type: review.decision } } } } }\n  - result: { set: { continued: true } }",
        );
        for (name, yaml) in [
            ("review", review.clone()),
            ("review", review),
            (
                "transform",
                workflow(
                    "  - signal: { listen: { to: { one: { with: { type: transform.signal } } } } }",
                ),
            ),
            (
                "archive",
                workflow(
                    "  - fail: { raise: { error: { type: 'urn:test:failure', status: 500 } } }",
                ),
            ),
            (
                "cleanup",
                workflow("  - done: { set: { done: true } }")
                    .replace("\ndo:", "\nschedule: { every: PT1H }\ndo:"),
            ),
        ] {
            let d = runtime.save(name, &yaml).await.unwrap();
            runtime.activate(name, d.version).await.unwrap();
        }
        let stop = CancellationToken::new();
        for (id, name) in [
            ("run-001", "review"),
            ("run-002", "review"),
            ("run-003", "review"),
            ("run-004", "cleanup"),
            ("run-005", "archive"),
            ("run-006", "review"),
            ("run-010", "transform"),
        ] {
            if matches!(id, "run-002" | "run-003") {
                runtime
                    .start(id, name, json!({}))
                    .with_context(admitted.clone())
                    .await
                    .unwrap();
            } else {
                runtime.start(id, name, json!({})).await.unwrap();
            }
        }
        for id in ["run-002", "run-003", "run-004", "run-005", "run-010"] {
            runtime.drive(id, &stop).await.unwrap();
        }
        runtime.cancel("run-006").await.unwrap();
        let control = Control::new(store.clone(), clock.clone());
        // Wake the parent so it can own children, then retain a real running lease.
        let wait = control
            .waits(
                &ouro_runtime::control::Waits {
                    run: Some("run-002".into()),
                    ..Default::default()
                },
                None,
                10,
            )
            .await
            .unwrap()
            .items
            .remove(0);
        control
            .respond(
                &operator("seed-parent"),
                "run-002",
                &wait.key,
                json!({"source":"tests","id":"seed-parent","type":"review.decision"}),
            )
            .await
            .unwrap();
        let fence = store
            .claim("run-002", clock.now_ms())
            .await
            .unwrap()
            .unwrap();
        runtime
            .start_child(&fence, "/child", "run-007", "review", json!({}))
            .await
            .unwrap();
        runtime
            .start_child(&fence, "/ended", "run-009", "cleanup", json!({}))
            .await
            .unwrap();
        runtime.drive("run-009", &stop).await.unwrap();
        let child_fence = store
            .claim("run-007", clock.now_ms())
            .await
            .unwrap()
            .unwrap();
        runtime
            .start_child(&child_fence, "/grandchild", "run-008", "review", json!({}))
            .await
            .unwrap();
        store.release(&child_fence, clock.now_ms()).await.unwrap();
        control
            .pause(
                &ouro_runtime::control::Request {
                    id: "demo-pause".into(),
                    actor: "demo-operator".into(),
                    reason: "maintenance window".into(),
                },
                "cleanup",
                "resume after review",
            )
            .await
            .unwrap();
        let wait_key = control
            .waits(
                &ouro_runtime::control::Waits {
                    run: Some("run-003".into()),
                    ..Default::default()
                },
                None,
                10,
            )
            .await
            .unwrap()
            .items
            .remove(0)
            .key;
        Self {
            client: Arc::new(OuroControl::new(store.clone(), clock, false)),
            live_worker: runtime.worker_id().into(),
            stale_worker: gone.worker_id().into(),
            real: Some(Real {
                runtime,
                control,
                backend,
                store,
            }),
            wait_key,
            spec: Some(spec),
            _directory: directory,
        }
    }
    pub fn is_real(&self) -> bool {
        self.real.is_some()
    }
    pub async fn drive_answer(&self) {
        if let Some(real) = &self.real {
            assert_eq!(
                real.runtime
                    .drive("run-003", &CancellationToken::new())
                    .await
                    .unwrap(),
                Some(Status::Completed(json!({"continued":true})))
            );
        }
    }
}
pub fn operator(id: &str) -> ouro_runtime::control::Request {
    ouro_runtime::control::Request {
        id: id.into(),
        actor: "tests".into(),
        reason: "test fixture".into(),
    }
}
#[allow(unused_macros)]
macro_rules! backend_tests {
    ($($name:ident),+ $(,)?) => {
        mod fake { $(#[tokio::test] async fn $name() { super::$name(crate::common::Backend::Fake).await; })+ }
        mod sqlite { $(#[tokio::test] async fn $name() { super::$name(crate::common::Backend::Sqlite).await; })+ }
        mod postgres { $(#[tokio::test] #[ignore = "requires TEST_DATABASE_URL (isolated schemas)"] async fn $name() {
            let backend = crate::common::Backend::postgres();
            super::$name(backend.clone()).await;
            backend.cleanup().await;
        })+ }
    };
}
#[allow(unused_imports)]
pub(crate) use backend_tests;
