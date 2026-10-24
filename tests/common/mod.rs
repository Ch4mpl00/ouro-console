#![allow(dead_code)] // Shared helpers are compiled independently by each integration suite.
use ouro_console::{client::*, fake::FakeControl, ouro::OuroControl};
use ouro_durable::{AtomicBackend, Clock, RunStore, TransactionalStore};
use ouro_runtime::{
    Config, Function, FunctionContext, Registry, Runtime, control::Control, ows::run::Status,
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
pub struct Fixture {
    pub client: Arc<dyn ControlClient>,
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
                |v: Value, _: FunctionContext| async { Ok(v) },
            ))
            .unwrap();
        let tracer = ouro_runtime::opentelemetry::global::BoxedTracer::new(Box::new(
            ouro_runtime::opentelemetry::trace::noop::NoopTracer::new(),
        ));
        let runtime = Runtime::new(
            store.clone(),
            Arc::new(registry),
            Config {
                breaker_threshold: 1,
                ..Config::default()
            },
            clock.clone(),
            tracer,
        )
        .unwrap();
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
            runtime.start(id, name, json!({})).await.unwrap();
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
            client: Arc::new(OuroControl::new(backend.clone(), clock, false)),
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
