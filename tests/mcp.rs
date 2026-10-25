mod common;
use axum::{
    Router,
    body::Body,
    http::{Request as HttpRequest, StatusCode},
};
use common::{Backend, Fixture};
use http_body_util::BodyExt;
use ouro_console::{
    client::*,
    mcp::{Mcp, WRITES},
    web::{WebConfig, router},
};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use tower::ServiceExt;

async fn setup(read_only: bool, backend: &Backend) -> (Router, Fixture) {
    let fixture = Fixture::new(backend).await;
    let session = Session {
        client: fixture.client.clone(),
        actor: "mcp-operator".into(),
        read_only,
    };
    (
        router(
            session,
            WebConfig::new("127.0.0.1:8090".parse().unwrap(), None, None).unwrap(),
        ),
        fixture,
    )
}
async fn rpc(app: &Router, method: &str, params: Value) -> Value {
    let response = app
        .clone()
        .oneshot(
            HttpRequest::builder()
                .method("POST")
                .uri("/mcp")
                .header("host", "localhost:8090")
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream")
                .header("mcp-protocol-version", "2025-11-25")
                .body(Body::from(
                    json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}
async fn call(app: &Router, name: &str, args: Value) -> Value {
    rpc(app, "tools/call", json!({"name":name,"arguments":args})).await
}
async fn streamable_http_initializes_and_lists_all_schemas(backend: Backend) {
    let (app, _) = setup(false, &backend).await;
    let init = rpc(&app,"initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"tests","version":"1"}})).await;
    assert!(
        init["result"]["capabilities"]["tools"].is_object(),
        "{init}"
    );
    assert_eq!(init["result"]["serverInfo"]["name"], "ouro-console");
    let list = rpc(&app, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    let names: BTreeSet<_> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(
        names,
        BTreeSet::from([
            "list_runs",
            "inspect_run",
            "run_log",
            "list_pending",
            "respond",
            "list_definitions",
            "get_definition",
            "get_definition_totals",
            "list_workers",
            "cancel_run",
            "cancel_tree",
            "pause_definition",
            "resume_definition",
            "stop_all",
            "resume_all"
        ])
    );
    for tool in tools {
        assert_eq!(tool["inputSchema"]["type"], "object");
        if WRITES.contains(&tool["name"].as_str().unwrap()) {
            assert!(
                tool["inputSchema"]["required"]
                    .as_array()
                    .unwrap()
                    .contains(&json!("reason")),
                "{tool}"
            );
            assert_eq!(
                tool["inputSchema"]["properties"]["reason"]["type"],
                "string"
            );
        }
    }
}
async fn every_tool_can_be_called_over_http_and_writes_are_real(backend: Backend) {
    for (name, args) in [
        ("list_runs", json!({"state":"running","limit":100})),
        ("inspect_run", json!({"run":"run-002"})),
        ("run_log", json!({"run":"run-002"})),
        ("list_pending", json!({"event_type":"review.decision"})),
        ("list_definitions", json!({"limit": 10})),
        ("get_definition", json!({"definition": "cleanup"})),
        ("list_workers", json!({})),
        (
            "cancel_run",
            json!({"run":"run-002","reason":"test cancellation","request_id":"test"}),
        ),
        (
            "cancel_tree",
            json!({"run":"run-002","reason":"test tree","request_id":"test"}),
        ),
        (
            "pause_definition",
            json!({"definition":"review","note":"investigation","reason":"test pause","request_id":"test"}),
        ),
        (
            "resume_definition",
            json!({"definition":"cleanup","reason":"test resume","request_id":"test"}),
        ),
        (
            "stop_all",
            json!({"reason":"test stop","request_id":"test"}),
        ),
        (
            "resume_all",
            json!({"reason":"test resume","request_id":"test"}),
        ),
        (
            "respond",
            json!({"run":"run-003","wait":"/decision","event":{"source":"tests","id":"event-1","type":"review.decision","data":{"decision":"approve"}},"reason":"reviewed","request_id":"test"}),
        ),
    ] {
        let (app, fixture) = setup(false, &backend).await;
        if name == "resume_all" {
            fixture
                .emergency_stop(
                    &Request {
                        id: "seed".into(),
                        actor: "seed".into(),
                        reason: "seed".into(),
                    },
                    true,
                )
                .await
                .unwrap();
        }
        let mut args = args;
        if name == "respond" {
            args["wait"] = json!(fixture.wait_key);
        }
        let reply = call(&app, name, args).await;
        assert!(reply.get("error").is_none(), "{name}: {reply}");
        assert_ne!(reply["result"]["isError"], true, "{name}: {reply}");
        assert!(
            reply["result"]["content"]
                .as_array()
                .is_some_and(|c| !c.is_empty()),
            "{name}: {reply}"
        );
        if WRITES.contains(&name) {
            let entry = fixture.entry("test").await.unwrap().unwrap();
            assert_eq!(entry.request.actor, "mcp-operator");
            assert!(reply.to_string().contains("mcp-operator"));
        }
        match name {
            "list_runs" => {
                assert!(reply.to_string().contains("run-002"));
                assert!(!reply.to_string().contains("run-003"));
            }
            "inspect_run" | "run_log" => {
                let text = reply.to_string();
                assert!(text.contains("gen_ai.usage.input_tokens"), "{text}");
                assert!(text.contains("model-a"), "{text}");
                if name == "inspect_run" {
                    assert!(text.contains(common::TRACE_ID), "{text}");
                }
            }
            "list_definitions" => {
                let text = reply.to_string();
                for name in ["archive", "cleanup", "review", "transform"] {
                    assert!(text.contains(name), "{text}");
                }
            }
            "get_definition" => {
                let text = reply.to_string();
                assert!(
                    text.contains("demo-operator") && text.contains("every"),
                    "{text}"
                );
            }
            "list_workers" => {
                let content: Value =
                    serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap())
                        .unwrap();
                let items = content["items"].as_array().unwrap();
                let worker = |id: &str| items.iter().find(|w| w["id"] == id).unwrap().clone();
                let live = worker(&fixture.live_worker);
                assert_eq!(live["stale"], false, "{live}");
                assert_eq!(live["host"], "host-a");
                assert_eq!(live["application"], "1.4.0");
                assert_eq!(worker(&fixture.stale_worker)["stale"], true);
            }
            "cancel_run" | "cancel_tree" => assert_eq!(
                fixture.inspect("run-008").await.unwrap().unwrap().run.state,
                RunState::Cancelled
            ),
            "pause_definition" => assert!(
                fixture
                    .definition("review")
                    .await
                    .unwrap()
                    .unwrap()
                    .tripped
                    .is_some()
            ),
            "resume_definition" => assert!(
                fixture
                    .definition("cleanup")
                    .await
                    .unwrap()
                    .unwrap()
                    .tripped
                    .is_none()
            ),
            "stop_all" => assert!(fixture.stopped().await.unwrap()),
            "resume_all" => assert!(!fixture.stopped().await.unwrap()),
            "respond" => {
                fixture.drive_answer().await;
                assert_eq!(
                    fixture.inspect("run-003").await.unwrap().unwrap().run.state,
                    if fixture.is_real() {
                        RunState::Completed
                    } else {
                        RunState::Ready
                    }
                );
            }
            _ => {}
        }
    }
}
async fn mcp_rejects_missing_or_blank_reasons_for_every_write(backend: Backend) {
    let (app, fixture) = setup(false, &backend).await;
    for name in WRITES {
        let args = json!({"run":"run-003","wait":"/decision","definition":"review","event":{}});
        let reply = call(&app, name, args.clone()).await;
        assert!(
            reply.get("error").is_some() || reply["result"]["isError"] == true,
            "{name}: {reply}"
        );
        let mut args = args;
        args["reason"] = json!(" ");
        let mut args = args;
        if name == "respond" {
            args["wait"] = json!(fixture.wait_key);
        }
        let reply = call(&app, name, args).await;
        assert!(
            reply.get("error").is_some() || reply["result"]["isError"] == true,
            "{name}: {reply}"
        );
    }
}
async fn read_only_mcp_exposes_and_accepts_only_reads(backend: Backend) {
    let (app, fixture) = setup(true, &backend).await;
    let list = rpc(&app, "tools/list", json!({})).await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 8);
    for name in WRITES {
        assert!(!tools.iter().any(|t| t["name"] == name));
        let reply = call(&app, name, json!({"reason":"attempt"})).await;
        assert!(
            reply.get("error").is_some() || reply["result"]["isError"] == true,
            "{reply}"
        );
    }
    assert!(!fixture.stopped().await.unwrap());
    let totals = call(
        &app,
        "get_definition_totals",
        json!({"definition":"review"}),
    )
    .await;
    assert_ne!(totals["result"]["isError"], true, "{totals}");
    let window: Value =
        serde_json::from_str(totals["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(window["definition"], "review", "{totals}");
    assert!(window["annotations"].is_object(), "{totals}");
    let inverted = call(
        &app,
        "get_definition_totals",
        json!({"definition":"review","since_ms":10,"before_ms":5}),
    )
    .await;
    assert_eq!(inverted["result"]["isError"], true, "{inverted}");
    assert_eq!(
        call(&app, "get_definition", json!({"definition":"missing"})).await["result"]["isError"],
        true
    );
    let info = Mcp::new(Session {
        client: fixture.client,
        actor: "observer".into(),
        read_only: true,
    });
    assert_eq!(info.tools().len(), 8);
}

common::backend_tests!(
    streamable_http_initializes_and_lists_all_schemas,
    every_tool_can_be_called_over_http_and_writes_are_real,
    mcp_rejects_missing_or_blank_reasons_for_every_write,
    read_only_mcp_exposes_and_accepts_only_reads
);

#[tokio::test]
#[ignore = "requires TEST_DATABASE_URL (isolated schemas)"]
async fn postgres_errors_include_database_diagnosis_over_http() {
    let backend = Backend::postgres();
    let (app, _) = setup(false, &backend).await;
    // Deliberately pass an invalid Postgres text value to exercise a real error chain.
    let reply = call(&app, "inspect_run", json!({"run":"invalid\0id"})).await;

    assert_eq!(reply["result"]["isError"], true, "{reply}");
    assert_eq!(
        reply["result"]["content"][0]["text"],
        "db error: ERROR: invalid byte sequence for encoding \"UTF8\": 0x00",
        "{reply}"
    );
    backend.cleanup().await;
}
