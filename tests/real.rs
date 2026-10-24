mod common;
use axum::{
    body::Body,
    http::{Request as HttpRequest, StatusCode},
};
use common::{Backend, Fixture};
use http_body_util::BodyExt;
use ouro_console::{
    cli,
    client::*,
    ouro::OuroControl,
    web::{WebConfig, router},
};
use ouro_durable::RunState as StoredState;
use serde_json::{Value, json};
use std::sync::Arc;
use tower::ServiceExt;

async fn action(app: &axum::Router, name: &str, fields: &[(&str, &str)]) -> (StatusCode, String) {
    let response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri(format!("/actions/{name}"))
                .header("host", "localhost:8090")
                .header("content-type", "application/x-www-form-urlencoded")
                .body(Body::from(serde_urlencoded::to_string(fields).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    (
        response.status(),
        String::from_utf8(
            response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .to_vec(),
        )
        .unwrap(),
    )
}
async fn http_cancel_and_respond_are_observed_by_the_runtime(backend: Backend) {
    let fixture = Fixture::new(&backend).await;
    let real = fixture.real.as_ref().unwrap();
    // Independent console connections exercise persistence, not shared process state.
    let client = cli::connect(false, fixture.spec.as_deref(), false)
        .await
        .unwrap();
    let app = router(
        Session {
            client,
            actor: "http-operator".into(),
            read_only: false,
        },
        WebConfig::new("127.0.0.1:8090".parse().unwrap(), None, None).unwrap(),
    );
    let fields = [
        ("request_id", "http-cancel"),
        ("reason", "stop owned work"),
        ("confirmed", "true"),
        ("run", "run-002"),
    ];
    let (code, html) = action(&app, "cancel", &fields).await;
    assert_eq!(code, StatusCode::OK, "{html}");
    for id in ["run-002", "run-007", "run-008"] {
        assert_eq!(
            real.control
                .inspect(id)
                .await
                .unwrap()
                .unwrap()
                .snapshot
                .run
                .state,
            StoredState::Cancelled
        );
    }
    assert_eq!(
        real.control
            .inspect("run-009")
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .run
            .state,
        StoredState::Completed
    );
    let entry = real.control.entry("http-cancel").await.unwrap().unwrap();
    assert_eq!(entry.request.actor, "http-operator");
    assert_eq!(action(&app, "cancel", &fields).await.0, StatusCode::OK);
    assert_eq!(
        real.control.entry("http-cancel").await.unwrap().unwrap(),
        entry
    );
    let (code, html) = action(
        &app,
        "respond",
        &[
            ("request_id", "http-answer"),
            ("reason", "reviewed"),
            ("confirmed", "true"),
            ("run", "run-003"),
            ("wait", &fixture.wait_key),
            ("decision", "approve"),
        ],
    )
    .await;
    assert_eq!(code, StatusCode::OK, "{html}");
    assert_eq!(
        real.control
            .entry("http-answer")
            .await
            .unwrap()
            .unwrap()
            .request
            .actor,
        "http-operator"
    );
    fixture.drive_answer().await;
    assert_eq!(
        real.control
            .inspect("run-003")
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .run
            .state,
        StoredState::Completed
    );
    // A retry after the wait has closed still returns the committed receipt.
    assert_eq!(
        action(
            &app,
            "respond",
            &[
                ("request_id", "http-answer"),
                ("reason", "reviewed"),
                ("confirmed", "true"),
                ("run", "run-003"),
                ("wait", &fixture.wait_key),
                ("decision", "approve")
            ]
        )
        .await
        .0,
        StatusCode::OK
    );
}
async fn opaque_paging_mappings_and_error_classification(backend: Backend) {
    let fixture = Fixture::new(&backend).await;
    let filter = Runs {
        definition: Some("review".into()),
        ..Default::default()
    };
    let mut cursor = None;
    let mut ids = Vec::new();
    loop {
        let page = fixture.list(&filter, cursor.as_ref(), 2).await.unwrap();
        ids.extend(page.items.into_iter().map(|r| r.id));
        cursor = page.next;
        if cursor.is_none() {
            break;
        }
    }
    ids.sort();
    ids.dedup();
    assert_eq!(
        ids,
        [
            "run-001", "run-002", "run-003", "run-006", "run-007", "run-008"
        ]
    );
    let page = fixture.list(&filter, None, 1).await.unwrap();
    assert!(matches!(
        fixture.list(&Runs::default(), page.next.as_ref(), 1).await,
        Err(ControlError::Invalid(_))
    ));
    assert!(matches!(
        fixture
            .list(&filter, Some(&Cursor("bad token".into())), 1)
            .await,
        Err(ControlError::Invalid(_))
    ));
    assert!(matches!(
        fixture.list(&filter, None, 0).await,
        Err(ControlError::Invalid(_))
    ));
    assert!(matches!(
        fixture.history(&filter, None, 10).await,
        Err(ControlError::Invalid(_))
    ));
    assert!(matches!(
        fixture.counts(&Runs::default(), false, None, 10).await,
        Err(ControlError::Invalid(_))
    ));
    let history = Runs {
        since_ms: Some(0),
        before_ms: Some(now_ms() + 1000),
        ..filter.clone()
    };
    assert_eq!(
        fixture
            .history(&history, None, 100)
            .await
            .unwrap()
            .items
            .len(),
        1
    );
    assert_eq!(
        fixture
            .counts(&history, true, None, 100)
            .await
            .unwrap()
            .states["cancelled"],
        1
    );
    assert_eq!(
        fixture
            .counts(&filter, false, None, 100)
            .await
            .unwrap()
            .states["running"],
        1
    );
    let definitions = fixture.definitions().await.unwrap();
    let review = definitions.iter().find(|d| d.name == "review").unwrap();
    assert_eq!(review.versions, ["1", "2"]);
    assert_eq!(review.active, "2");
    assert_eq!(review.epoch, 2);
    assert_eq!(
        definitions
            .iter()
            .find(|d| d.name == "cleanup")
            .unwrap()
            .schedule
            .as_deref(),
        Some(r#"{"every":"PT1H"}"#)
    );
    let empty = fixture
        .list(
            &Runs {
                state: Some(RunState::Failed),
                ..filter.clone()
            },
            None,
            1,
        )
        .await
        .unwrap();
    assert!(empty.items.is_empty() && empty.next.is_some());
    let pending = fixture.waits(&Waits::default(), None, 1).await.unwrap();
    let next_pending = fixture
        .waits(&Waits::default(), pending.next.as_ref(), 1)
        .await
        .unwrap();
    assert_ne!(
        (&pending.items[0].run, &pending.items[0].key),
        (&next_pending.items[0].run, &next_pending.items[0].key)
    );
    assert!(matches!(
        fixture
            .waits(
                &Waits {
                    run: Some("run-003".into()),
                    ..Default::default()
                },
                pending.next.as_ref(),
                1
            )
            .await,
        Err(ControlError::Invalid(_))
    ));
    let journal = fixture.journal(None, None, 1).await.unwrap();
    assert!(journal.next.is_some());
    assert_ne!(
        journal.items[0].request.id,
        fixture
            .journal(None, journal.next.as_ref(), 1)
            .await
            .unwrap()
            .items[0]
            .request
            .id
    );
    assert!(matches!(
        fixture
            .journal(Some("run-002"), journal.next.as_ref(), 1)
            .await,
        Err(ControlError::Invalid(_))
    ));
    let bounded = fixture.counts(&filter, false, None, 1).await.unwrap();
    assert_eq!(bounded.states.values().sum::<usize>(), 1);
    assert!(bounded.next.is_some());
    let active = fixture.inspect("run-002").await.unwrap().unwrap();
    let raw = fixture
        .real
        .as_ref()
        .unwrap()
        .control
        .inspect("run-002")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        active.lease.as_ref().unwrap().holder,
        raw.snapshot.run.fence.unwrap().owner
    );
    assert_eq!(
        active.lease.as_ref().unwrap().expires_ms,
        raw.snapshot.run.lease_until_ms.unwrap()
    );
    assert!(matches!(
        definitions
            .iter()
            .find(|d| d.name == "archive")
            .unwrap()
            .tripped,
        Some(Trip::Automatic {
            faults: 1,
            threshold: 1,
            ..
        })
    ));
    assert!(matches!(
        definitions
            .iter()
            .find(|d| d.name == "cleanup")
            .unwrap()
            .tripped,
        Some(Trip::Manual { .. })
    ));
    assert!(
        matches!(fixture.workers().await.unwrap(), Workers::Unavailable { reason } if reason.contains("ouro#20"))
    );
    let inspection = fixture.inspect("run-003").await.unwrap().unwrap();
    assert_eq!(inspection.run.version, "2");
    assert!(inspection.trace_id.is_none());
    assert_eq!(inspection.calls.len(), 1);
    assert_eq!(inspection.calls[0].arguments, json!({"marker":"original"}));
    assert_eq!(
        inspection.calls[0].result,
        Some(json!({"marker":"original"}))
    );
    assert_eq!(inspection.calls[0].state, "completed");
    assert!(!inspection.waits[&fixture.wait_key].position.is_empty());
    let request = Request {
        id: "conflict".into(),
        actor: "tests".into(),
        reason: "reviewed".into(),
    };
    assert!(matches!(
        fixture
            .respond(
                &request,
                "run-003",
                &fixture.wait_key,
                json!({"source":"tests","id":"bad","type":"wrong"})
            )
            .await,
        Err(ControlError::Invalid(_))
    ));
    assert!(fixture.entry("conflict").await.unwrap().is_none());
    fixture.emergency_stop(&request, true).await.unwrap();
    assert!(matches!(
        fixture.emergency_stop(&request, false).await,
        Err(ControlError::Conflict(_))
    ));
    assert!(matches!(
        fixture
            .resume(
                &Request {
                    id: "breaker".into(),
                    ..request.clone()
                },
                "archive"
            )
            .await,
        Err(ControlError::Conflict(_))
    ));
    assert!(matches!(
        fixture
            .pause(
                &Request {
                    id: "unknown".into(),
                    ..request
                },
                "missing",
                ""
            )
            .await,
        Err(ControlError::NotFound(_))
    ));
}
async fn read_only_connection_reads_without_mutating_or_migrating(backend: Backend) {
    let fixture = Fixture::new(&backend).await;
    let real = fixture.real.as_ref().unwrap();
    let before = real.backend.snapshot().await.unwrap();
    let client = Arc::new(
        OuroControl::connect(fixture.spec.as_deref().unwrap(), true)
            .await
            .unwrap(),
    );
    // Adapter itself refuses writes, even if the caller bypasses Session.
    let request = Request {
        id: "forbidden".into(),
        actor: "tests".into(),
        reason: "attempt".into(),
    };
    for command in [
        Command::Stop { enabled: true },
        Command::Cancel {
            run: "run-002".into(),
        },
        Command::Pause {
            definition: "review".into(),
            note: "".into(),
        },
        Command::Resume {
            definition: "cleanup".into(),
        },
        Command::Respond {
            run: "run-003".into(),
            wait: fixture.wait_key.clone(),
            event: json!({}),
        },
    ] {
        assert!(matches!(
            client.execute(&request, &command).await,
            Err(ControlError::ReadOnly)
        ));
    }
    assert!(!client.stopped().await.unwrap());
    assert_eq!(
        client
            .list(&Runs::default(), None, 100)
            .await
            .unwrap()
            .items
            .len(),
        10
    );
    assert_eq!(
        client
            .history(
                &Runs {
                    since_ms: Some(0),
                    before_ms: Some(now_ms() + 1000),
                    ..Default::default()
                },
                None,
                100
            )
            .await
            .unwrap()
            .items
            .len(),
        4
    );
    assert!(
        !client
            .waits(&Waits::default(), None, 100)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert_eq!(
        client.inspect("run-003").await.unwrap().unwrap().calls[0].arguments,
        json!({"marker":"original"})
    );
    assert!(client.inspect("missing").await.unwrap().is_none());
    assert_eq!(client.definitions().await.unwrap().len(), 4);
    assert!(
        !client
            .journal(None, None, 100)
            .await
            .unwrap()
            .items
            .is_empty()
    );
    assert!(client.entry("demo-pause").await.unwrap().is_some());
    assert_eq!(
        client
            .counts(
                &Runs {
                    definition: Some("review".into()),
                    ..Default::default()
                },
                false,
                None,
                100
            )
            .await
            .unwrap()
            .states["running"],
        1
    );
    let session = Session {
        client,
        actor: "observer".into(),
        read_only: true,
    };
    assert!(matches!(
        cli::execute(
            &session,
            cli::Action::StopAll(cli::MutationArgs {
                reason: "attempt".into(),
                request_id: None
            })
        )
        .await,
        Err(ControlError::ReadOnly)
    ));
    assert_eq!(real.backend.snapshot().await.unwrap(), before);
}
#[tokio::test]
async fn sqlite_http_cancel_and_respond_are_observed_by_the_runtime() {
    http_cancel_and_respond_are_observed_by_the_runtime(Backend::Sqlite).await;
}
#[tokio::test]
async fn sqlite_opaque_paging_mappings_and_error_classification() {
    opaque_paging_mappings_and_error_classification(Backend::Sqlite).await;
}
#[tokio::test]
async fn sqlite_read_only_connection_reads_without_mutating_or_migrating() {
    read_only_connection_reads_without_mutating_or_migrating(Backend::Sqlite).await;
}
#[tokio::test]
async fn read_only_never_creates_a_missing_database_or_initializes_a_schema() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("absent.sqlite");
    let spec = format!("sqlite:{}", path.display());
    assert!(OuroControl::connect(&spec, true).await.is_err());
    assert!(!path.exists());
    let empty = dir.path().join("empty.sqlite");
    rusqlite::Connection::open(&empty).unwrap();
    let before = std::fs::read(&empty).unwrap();
    assert!(
        OuroControl::connect(&format!("sqlite:{}", empty.display()), true)
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(&empty).unwrap(), before);
}
#[tokio::test]
async fn cli_processes_select_a_persistent_store_and_environment() {
    let fixture = Fixture::new(&Backend::Sqlite).await;
    let command = |args: &[&str]| {
        std::process::Command::new(env!("CARGO_BIN_EXE_ouro-console"))
            .env("OURO_STORE", fixture.spec.as_deref().unwrap())
            .env_remove("OURO_CONSOLE_TOKEN_FILE")
            .env_remove("OURO_CONSOLE_TOKEN")
            .args(args)
            .output()
            .unwrap()
    };
    let output = command(&[
        "cancel-run",
        "run-001",
        "--reason",
        "test process",
        "--request-id",
        "process-cancel",
    ]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        fixture
            .real
            .as_ref()
            .unwrap()
            .control
            .inspect("run-001")
            .await
            .unwrap()
            .unwrap()
            .snapshot
            .run
            .state,
        StoredState::Cancelled
    );
    let output = command(&["inspect", "run-001", "--read-only"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let json: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(json["run"]["state"], "cancelled");
    let output = command(&["stop-all", "--read-only", "--reason", "attempt"]);
    assert!(!output.status.success());
    assert!(!fixture.stopped().await.unwrap());
    assert!(cli::connect(false, None, false).await.is_err());
    assert!(
        cli::connect(true, fixture.spec.as_deref(), false)
            .await
            .is_err()
    );
}
#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (isolated schemas)"]
async fn postgres_real_store_contract_and_http_end_to_end() {
    let backend = Backend::postgres();
    http_cancel_and_respond_are_observed_by_the_runtime(backend.clone()).await;
    opaque_paging_mappings_and_error_classification(backend.clone()).await;
    read_only_connection_reads_without_mutating_or_migrating(backend.clone()).await;
    complex_wait_progress_and_response_replay_are_owned_by_ouro(backend.clone()).await;
    backend.cleanup().await;
}

async fn complex_wait_progress_and_response_replay_are_owned_by_ouro(backend: Backend) {
    use ouro_runtime::ows::run::Status;
    use tokio_util::sync::CancellationToken;
    let fixture = Fixture::new(&backend).await;
    let real = fixture.real.as_ref().unwrap();
    let yaml = "document: { dsl: '1.0.4', namespace: test, name: workflow, version: '1.0.0' }\ndo:\n  - decision:\n      listen:\n        to:\n          all:\n            - with: { type: first }\n              correlate: { ticket: { from: .ticket } }\n            - with: { type: second }\n              correlate: { ticket: { from: .ticket } }\n";
    let definition = real.runtime.save("multi", yaml).await.unwrap();
    real.runtime
        .activate("multi", definition.version)
        .await
        .unwrap();
    real.runtime
        .start("multi-run", "multi", json!({}))
        .await
        .unwrap();
    let token = CancellationToken::new();
    assert!(matches!(
        real.runtime.drive("multi-run", &token).await.unwrap(),
        Some(Status::Waiting { .. })
    ));
    let wait = fixture
        .waits(
            &Waits {
                run: Some("multi-run".into()),
                ..Default::default()
            },
            None,
            10,
        )
        .await
        .unwrap()
        .items
        .remove(0);
    assert_eq!(wait.waiting.strategy, "all");
    assert_eq!(wait.waiting.types, ["first", "second"]);
    assert!(wait.waiting.filters.get("All").is_some());
    let request = |id: &str| Request {
        id: id.into(),
        actor: "operator".into(),
        reason: "reviewed".into(),
    };
    let event = |id: &str, kind: &str, ticket| json!({"source":"tests","id":id,"type":kind,"ticket":ticket});
    fixture
        .respond(
            &request("first"),
            "multi-run",
            &wait.key,
            event("first", "first", 42),
        )
        .await
        .unwrap();
    assert!(matches!(
        real.runtime.drive("multi-run", &token).await.unwrap(),
        Some(Status::Waiting { .. })
    ));
    let progress = fixture.inspect("multi-run").await.unwrap().unwrap();
    assert_eq!(progress.waits[&wait.key].expectations["ticket"], 42);
    assert_ne!(progress.waits[&wait.key].position, wait.waiting.position);
    assert!(matches!(
        fixture
            .respond(
                &request("wrong"),
                "multi-run",
                &wait.key,
                event("wrong", "second", 43)
            )
            .await,
        Err(ControlError::Invalid(_))
    ));
    let answer = event("second", "second", 42);
    let reply = fixture
        .respond(&request("second"), "multi-run", &wait.key, answer.clone())
        .await
        .unwrap();
    assert!(matches!(
        real.runtime.drive("multi-run", &token).await.unwrap(),
        Some(Status::Completed(_))
    ));
    assert_eq!(
        fixture
            .respond(&request("retry"), "multi-run", &wait.key, answer)
            .await
            .unwrap(),
        reply
    );
    assert_eq!(
        fixture.entry("retry").await.unwrap().unwrap().request.id,
        "retry"
    );
    assert!(matches!(
        fixture
            .respond(
                &request("changed"),
                "multi-run",
                &wait.key,
                event("second", "second", 43)
            )
            .await,
        Err(ControlError::Conflict(_))
    ));
    assert!(fixture.entry("changed").await.unwrap().is_none());
}
#[tokio::test]
async fn sqlite_complex_wait_progress_and_response_replay_are_owned_by_ouro() {
    complex_wait_progress_and_response_replay_are_owned_by_ouro(Backend::Sqlite).await;
}
