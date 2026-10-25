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
    web::{WebConfig, router},
};
use serde_json::json;
use tower::ServiceExt;

async fn setup(read_only: bool, backend: &Backend) -> (Router, Fixture) {
    let fixture = Fixture::new(backend).await;
    let session = Session {
        client: fixture.client.clone(),
        actor: "test-operator".into(),
        read_only,
    };
    (
        router(
            session,
            WebConfig::new(
                "127.0.0.1:8090".parse().unwrap(),
                None,
                Some("https://traces.example/trace/{trace_id}".into()),
            )
            .unwrap(),
        ),
        fixture,
    )
}
async fn request(
    app: &Router,
    method: &str,
    path: &str,
    body: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, String) {
    let mut req = HttpRequest::builder()
        .method(method)
        .uri(path)
        .header("host", "localhost:8090")
        .header("content-type", "application/x-www-form-urlencoded");
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    let response = app
        .clone()
        .oneshot(req.body(Body::from(body.to_owned())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let text = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    (status, text)
}
fn form(fields: &[(&str, &str)]) -> String {
    serde_urlencoded::to_string(fields).unwrap()
}
async fn every_page_renders_key_content_and_global_controls(backend: Backend) {
    let (app, fixture) = setup(false, &backend).await;
    for (path, needles) in [
        (
            "/runs?limit=100",
            vec![
                "run-001",
                "running",
                "waiting",
                "failed",
                "completed",
                "cancelled",
                "ready",
                "every 2s",
                "Run activity",
            ],
        ),
        (
            "/runs/run-002",
            vec![
                "Lease holder / expiry",
                "worker-a",
                "Jump budget",
                "Call journal",
                "not_executed",
                "unknown",
                "run-007",
                "run-008",
                "run-009",
                "https://traces.example/trace/0123456789abcdef0123456789abcdef",
                "Usage",
                "Input tokens",
                "Output tokens",
                "gen_ai.usage.input_tokens",
                "gen_ai.usage.output_tokens",
                "model-a",
                "Annotations",
            ],
        ),
        (
            "/runs/run-003",
            vec![
                "review.decision",
                "Pending waits",
                "Matcher and progress",
                "Timeout",
            ],
        ),
        (
            "/pending",
            vec![
                "Waiting for a decision",
                "review.decision",
                "transform.signal",
                "Approve",
                "Reject",
                "JSON payload",
                "Expectations",
            ],
        ),
        (
            "/definitions",
            vec![
                "Versions",
                "Active version",
                "automatically tripped",
                "manually paused",
                "demo-operator",
                "resume after review",
                "Schedule",
                "every",
                "suspended",
                "Overlap skip",
            ],
        ),
        (
            "/workers",
            vec![
                "live",
                "stale",
                "Last beat",
                "Runs held",
                "Application version",
                "1.4.0",
                "1.3.2",
                "host-a",
                "host-b",
            ],
        ),
        (
            "/journal",
            vec![
                "demo-pause",
                "maintenance window",
                "demo-operator",
                "Changed",
            ],
        ),
        (
            "/emergency",
            vec![
                "Emergency stop is clear",
                "Stop all execution",
                "Reason",
                "confirmed",
            ],
        ),
    ] {
        let (status, html) = request(&app, "GET", path, "", &[]).await;
        assert_eq!(status, StatusCode::OK, "{path}: {html}");
        for needle in needles {
            if fixture.is_real() && ["worker-a", "not_executed", "unknown"].contains(&needle) {
                continue;
            }
            assert!(html.contains(needle), "{path} missing {needle}");
        }
        assert!(html.contains("/emergency"), "missing global stop on {path}");
        if path == "/workers" {
            assert!(html.contains(&fixture.live_worker), "{html}");
            assert!(html.contains(&fixture.stale_worker), "{html}");
        }
        if fixture.is_real() {
            assert!(!html.contains("Fake backend"));
        }
        assert!(html.contains("theme-toggle"));
        assert!(!html.contains("cdn."));
    }
    let (status, _) = request(&app, "GET", "/runs/run-missing", "", &[]).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(
        request(&app, "GET", "/", "", &[]).await.0,
        StatusCode::SEE_OTHER
    );
    assert_eq!(
        request(&app, "GET", "/missing", "", &[]).await.0,
        StatusCode::NOT_FOUND
    );
}
async fn filters_paging_and_polling_preserve_query(backend: Backend) {
    let (app, fixture) = setup(false, &backend).await;
    let (_, html) = request(
        &app,
        "GET",
        "/runs?state=running&definition=review&age_minutes=60&limit=100",
        "",
        &[],
    )
    .await;
    assert!(html.contains("run-002"));
    assert!(!html.contains("run-003"));
    assert!(html.contains("since_ms="));
    let (_, all) = request(
        &app,
        "GET",
        "/runs?definition=&state=&age_minutes=1440&history=false",
        "",
        &[],
    )
    .await;
    assert!(all.contains("Run activity"), "{all}");
    let (_, html) = request(&app, "GET", "/runs?limit=2", "", &[]).await;
    assert!(html.contains("Next page"));
    let link = html
        .split("href=\"")
        .filter_map(|s| s.split('"').next())
        .find(|s| s.contains("cursor="))
        .unwrap()
        .replace("&amp;", "&");
    let (code, next) = request(&app, "GET", &link, "", &[]).await;
    assert_eq!(code, StatusCode::OK, "{next}");
    assert!(next.contains("First page"));
    let (code, fragment) = request(&app, "GET", &link, "", &[("hx-request", "true")]).await;
    assert_eq!(code, StatusCode::OK);
    assert!(fragment.starts_with("<section"));
    assert!(!fragment.contains("<html"));
    assert_eq!(
        request(&app, "GET", "/runs?age_minutes=-1", "", &[])
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    // Definition and worker pages follow the backend's continuation.
    let follow = |html: &str| {
        html.split("href=\"")
            .filter_map(|s| s.split('"').next())
            .find(|s| s.contains("cursor="))
            .expect("next page link")
            .replace("&amp;", "&")
    };
    let title = |name: &str| format!("<a href=\"/definitions/{name}\">{name}</a>");
    let (_, first) = request(&app, "GET", "/definitions?limit=3", "", &[]).await;
    assert!(first.contains(&title("archive")) && !first.contains(&title("transform")));
    let (code, rest) = request(&app, "GET", &follow(&first), "", &[]).await;
    assert_eq!(code, StatusCode::OK, "{rest}");
    assert!(rest.contains(&title("transform")) && !rest.contains(&title("archive")));
    let workers = [fixture.live_worker.as_str(), fixture.stale_worker.as_str()];
    let (_, first) = request(&app, "GET", "/workers?limit=1", "", &[]).await;
    let (code, rest) = request(&app, "GET", &follow(&first), "", &[]).await;
    assert_eq!(code, StatusCode::OK, "{rest}");
    for id in workers {
        assert!(
            first.contains(id) != rest.contains(id),
            "{id} on exactly one page"
        );
    }
    let (_, history) = request(&app, "GET", "/runs?history=true&limit=100", "", &[]).await;
    assert!(history.contains("Terminal history"));
    assert!(history.contains("run-005"));
    assert!(!history.contains("class=\"fw-semibold\" href=\"/runs/run-002\""));
}
async fn each_action_mutates_and_returns_attributed_receipt(backend: Backend) {
    for (action, target) in [
        ("cancel", ("run", "run-002")),
        ("cancel-tree", ("run", "run-002")),
        ("pause", ("definition", "review")),
        ("resume", ("definition", "cleanup")),
        ("stop-all", ("x", "")),
        ("resume-all", ("x", "")),
        ("respond", ("run", "run-003")),
    ] {
        let (app, fixture) = setup(false, &backend).await;
        if action == "resume-all" {
            fixture
                .emergency_stop(
                    &Request {
                        id: "seed".into(),
                        actor: "seed".into(),
                        reason: "test".into(),
                    },
                    true,
                )
                .await
                .unwrap();
        }
        let body = form(&[
            ("reason", "investigate <sample>"),
            ("request_id", action),
            ("confirmed", "true"),
            target,
            ("wait", fixture.wait_key.as_str()),
            ("decision", "approve"),
            ("note", "maintenance"),
        ]);
        let (code, html) = request(&app, "POST", &format!("/actions/{action}"), &body, &[]).await;
        assert_eq!(code, StatusCode::OK, "{action}: {html}");
        assert!(html.contains("Command accepted"));
        assert!(html.contains("test-operator"));
        assert!(html.contains("investigate &lt;sample&gt;"));
        assert!(!html.contains("investigate <sample>"));
        let entry = fixture.entry(action).await.unwrap().unwrap();
        assert_eq!(entry.request.actor, "test-operator");
        match action {
            "cancel" | "cancel-tree" => {
                for id in ["run-002", "run-007", "run-008"] {
                    assert_eq!(
                        fixture.inspect(id).await.unwrap().unwrap().run.state,
                        RunState::Cancelled
                    );
                }
                assert_eq!(
                    fixture.inspect("run-009").await.unwrap().unwrap().run.state,
                    RunState::Completed
                );
            }
            "pause" => assert!(matches!(
                fixture.definition("review").await.unwrap().unwrap().tripped,
                Some(Trip::Manual { .. })
            )),
            "resume" => assert!(
                fixture
                    .definition("cleanup")
                    .await
                    .unwrap()
                    .unwrap()
                    .tripped
                    .is_none()
            ),
            "stop-all" => assert!(fixture.stopped().await.unwrap()),
            "resume-all" => assert!(!fixture.stopped().await.unwrap()),
            "respond" => {
                fixture.drive_answer().await;
                assert!(
                    fixture
                        .inspect("run-003")
                        .await
                        .unwrap()
                        .unwrap()
                        .waits
                        .is_empty()
                );
            }
            _ => unreachable!(),
        }
        let (code, _) = request(&app, "POST", &format!("/actions/{action}"), &body, &[]).await;
        assert_eq!(code, StatusCode::OK, "retry {action}");
    }
}
async fn all_mutations_reject_missing_reason_confirmation_or_request_id(backend: Backend) {
    let (app, fixture) = setup(false, &backend).await;
    for action in [
        "cancel",
        "cancel-tree",
        "pause",
        "resume",
        "stop-all",
        "resume-all",
        "respond",
    ] {
        for missing in ["reason", "confirmed", "request_id"] {
            let fields = [
                ("reason", "test"),
                ("confirmed", "true"),
                ("request_id", "test"),
                ("run", "run-003"),
                ("definition", "review"),
                ("wait", fixture.wait_key.as_str()),
                ("decision", "approve"),
            ]
            .into_iter()
            .filter(|(k, _)| *k != missing)
            .collect::<Vec<_>>();
            let (status, html) = request(
                &app,
                "POST",
                &format!("/actions/{action}"),
                &form(&fields),
                &[],
            )
            .await;
            assert_eq!(
                status,
                StatusCode::BAD_REQUEST,
                "{action} missing {missing}: {html}"
            );
        }
    }
    assert_eq!(
        request(
            &app,
            "POST",
            "/actions/stop-all",
            &form(&[
                ("reason", "   "),
                ("confirmed", "true"),
                ("request_id", "blank")
            ]),
            &[]
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
}
async fn json_answers_reject_invalid_payloads_and_breaker_resume_conflicts(backend: Backend) {
    let (app, fixture) = setup(false, &backend).await;
    let body = |payload| {
        form(&[
            ("reason", "reviewed"),
            ("confirmed", "true"),
            ("request_id", "answer-json"),
            ("run", "run-003"),
            ("wait", fixture.wait_key.as_str()),
            ("decision", "json"),
            ("payload", payload),
        ])
    };
    assert_eq!(
        request(&app, "POST", "/actions/respond", &body("{"), &[])
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(
            &app,
            "POST",
            "/actions/respond",
            &body("{\"answer\":42}"),
            &[]
        )
        .await
        .0,
        StatusCode::OK
    );
    assert_eq!(
        {
            let inspection = fixture.inspect("run-003").await.unwrap().unwrap();
            if fixture.is_real() {
                inspection.mailbox[0]["envelope"]["data"]["answer"].clone()
            } else {
                inspection.mailbox[0]["data"]["answer"].clone()
            }
        },
        42
    );
    assert_eq!(
        request(&app, "POST", "/actions/respond", &body("{}"), &[])
            .await
            .0,
        StatusCode::CONFLICT
    );
    let body = form(&[
        ("reason", "test"),
        ("confirmed", "true"),
        ("request_id", "automatic"),
        ("definition", "archive"),
    ]);
    assert_eq!(
        request(&app, "POST", "/actions/resume", &body, &[]).await.0,
        StatusCode::CONFLICT
    );
}
async fn read_only_hides_and_denies_every_action(backend: Backend) {
    let (app, fixture) = setup(true, &backend).await;
    for path in [
        "/runs",
        "/runs/run-002",
        "/pending",
        "/definitions",
        "/workers",
        "/journal",
        "/emergency",
    ] {
        let (code, html) = request(&app, "GET", path, "", &[]).await;
        assert_eq!(code, StatusCode::OK);
        assert!(!html.contains("/actions/"));
        assert!(!html.contains("href=\"/emergency\""));
        assert!(html.contains("Emergency stop clear"));
    }
    for action in [
        "cancel",
        "cancel-tree",
        "pause",
        "resume",
        "stop-all",
        "resume-all",
        "respond",
    ] {
        assert_eq!(
            request(&app, "POST", &format!("/actions/{action}"), "", &[])
                .await
                .0,
            StatusCode::FORBIDDEN
        );
    }
    assert!(!fixture.stopped().await.unwrap());
}
async fn bearer_auth_covers_pages_assets_and_mcp_and_remote_bind_requires_it(backend: Backend) {
    assert!(WebConfig::new("0.0.0.0:8090".parse().unwrap(), None, None).is_err());
    let fixture = Fixture::new(&backend).await;
    let session = Session {
        client: fixture.client.clone(),
        actor: "remote-operator".into(),
        read_only: false,
    };
    let app = router(
        session,
        WebConfig::new(
            "0.0.0.0:8090".parse().unwrap(),
            Some("test-secret".into()),
            None,
        )
        .unwrap(),
    );
    for path in ["/runs", "/assets/console.css", "/mcp"] {
        for headers in [vec![], vec![("authorization", "Bearer wrong")]] {
            assert_eq!(
                request(&app, "GET", path, "", &headers).await.0,
                StatusCode::UNAUTHORIZED
            );
        }
    }
    let (code, html) = request(
        &app,
        "GET",
        "/runs",
        "",
        &[("authorization", "Bearer test-secret")],
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(!html.contains("test-secret"));
    assert_eq!(
        request(
            &app,
            "POST",
            "/actions/stop-all",
            "",
            &[
                ("authorization", "Bearer test-secret"),
                ("origin", "https://evil.example")
            ]
        )
        .await
        .0,
        StatusCode::FORBIDDEN
    );
}
async fn embedded_assets_and_trace_configuration_are_valid(backend: Backend) {
    let (app, _) = setup(false, &backend).await;
    for path in [
        "/assets/console.js",
        "/assets/console.css",
        "/assets/vendor/htmx/htmx.min.js",
        "/assets/vendor/htmx/LICENSE",
        "/assets/vendor/tabler/tabler.min.css",
        "/assets/vendor/tabler/tabler.min.js",
        "/assets/vendor/tabler/LICENSE",
        "/assets/vendor/tabler/LICENSE.bootstrap",
        "/assets/vendor/tabler/LICENSE.popper",
        "/assets/vendor/tabler-icons/tabler-icons.min.css",
    ] {
        let (code, _) = request(&app, "GET", path, "", &[]).await;
        assert_eq!(code, StatusCode::OK, "{path}");
    }
    for bad in [
        "javascript:{trace_id}",
        "https://traces.example/no-placeholder",
    ] {
        assert!(WebConfig::new("127.0.0.1:8090".parse().unwrap(), None, Some(bad.into())).is_err());
    }
}

async fn runs_show_tokens_and_the_definition_page_shows_versions_and_totals(backend: Backend) {
    let (app, fixture) = setup(false, &backend).await;
    fixture
        .respond(
            &Request {
                id: "answer".into(),
                actor: "tests".into(),
                reason: "finish a run".into(),
            },
            "run-003",
            &fixture.wait_key,
            json!({"specversion":"1.0","source":"tests","id":"answer","type":"review.decision","data":{"decision":"approve"}}),
        )
        .await
        .unwrap();
    fixture.drive_answer().await;
    let (code, runs) = request(&app, "GET", "/runs?history=true&limit=100", "", &[]).await;
    assert_eq!(code, StatusCode::OK, "{runs}");
    assert!(runs.contains("<th>Tokens</th>"), "{runs}");
    assert!(runs.contains(" in / ") && runs.contains(" out"), "{runs}");
    assert!(runs.contains(" tokens on this page"), "{runs}");
    let (code, page) = request(&app, "GET", "/definitions/review", "", &[]).await;
    assert_eq!(code, StatusCode::OK, "{page}");
    for wanted in [
        "Versions",
        "activated",
        "by demo-operator",
        "promote after review",
        "Last 24 hours",
        "gen_ai.usage.input_tokens",
        "gen_ai.usage.output_tokens",
    ] {
        assert!(page.contains(wanted), "{wanted}: {page}");
    }
    let (code, _) = request(&app, "GET", "/definitions/review?hours=1", "", &[]).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        request(&app, "GET", "/definitions/review?hours=0", "", &[])
            .await
            .0,
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        request(&app, "GET", "/definitions/missing", "", &[])
            .await
            .0,
        StatusCode::NOT_FOUND
    );
}
common::backend_tests!(
    runs_show_tokens_and_the_definition_page_shows_versions_and_totals,
    every_page_renders_key_content_and_global_controls,
    filters_paging_and_polling_preserve_query,
    each_action_mutates_and_returns_attributed_receipt,
    all_mutations_reject_missing_reason_confirmation_or_request_id,
    json_answers_reject_invalid_payloads_and_breaker_resume_conflicts,
    read_only_hides_and_denies_every_action,
    bearer_auth_covers_pages_assets_and_mcp_and_remote_bind_requires_it,
    embedded_assets_and_trace_configuration_are_valid
);
