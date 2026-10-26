use crate::{client::*, mcp::Mcp};
use axum::{
    Router,
    body::Body,
    extract::{Form, Path, Query, Request as HttpRequest, State},
    http::{HeaderMap, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use maud::{DOCTYPE, Markup, html};
use rmcp::transport::streamable_http_server::{
    StreamableHttpServerConfig, StreamableHttpService, session::local::LocalSessionManager,
};
use rust_embed::RustEmbed;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    net::SocketAddr,
    sync::Arc,
};
use subtle::ConstantTimeEq;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;
#[derive(Clone)]
pub struct WebConfig {
    pub bind: SocketAddr,
    token: Option<String>,
    trace_url: Option<String>,
}
impl WebConfig {
    pub fn new(bind: SocketAddr, token: Option<String>, trace_url: Option<String>) -> Result<Self> {
        if !bind.ip().is_loopback() && token.is_none() {
            return Err(ControlError::Invalid(
                "non-loopback bind requires --token-file or OURO_CONSOLE_TOKEN".into(),
            ));
        }
        if token
            .as_ref()
            .is_some_and(|t| t.is_empty() || t.chars().any(char::is_whitespace))
        {
            return Err(ControlError::Invalid("invalid bearer token".into()));
        }
        if let Some(template) = &trace_url {
            let parsed = url::Url::parse(&template.replace("{trace_id}", "sample"))
                .map_err(|_| ControlError::Invalid("invalid trace URL".into()))?;
            if !template.contains("{trace_id}") || !matches!(parsed.scheme(), "http" | "https") {
                return Err(ControlError::Invalid(
                    "trace URL must be HTTP(S) and contain {trace_id}".into(),
                ));
            }
        }
        Ok(Self {
            bind,
            token,
            trace_url,
        })
    }
}
#[derive(Clone)]
struct App {
    session: Session,
    config: WebConfig,
}
pub fn router(session: Session, config: WebConfig) -> Router {
    let app = App {
        session: session.clone(),
        config: config.clone(),
    };
    let mcp = StreamableHttpService::new(
        move || Ok(Mcp::new(session.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default()
            .with_legacy_session_mode(false)
            .with_json_response(true)
            .disable_allowed_hosts(),
    );
    Router::new()
        .route("/", get(|| async { Redirect::to("/runs") }))
        .route("/runs", get(runs))
        .route("/runs/{id}", get(detail))
        .route("/pending", get(pending))
        .route("/definitions", get(definitions))
        .route("/definitions/{name}", get(definition_page))
        .route("/workers", get(workers))
        .route("/journal", get(journal))
        .route("/emergency", get(emergency))
        .route("/status", get(status))
        .route("/actions/{action}", post(action))
        .route("/assets/{*path}", get(asset))
        .nest_service("/mcp", mcp)
        .fallback(|| async { (StatusCode::NOT_FOUND, "Page not found") })
        .with_state(app)
        .layer(middleware::from_fn_with_state(config, guard))
}
async fn guard(State(config): State<WebConfig>, request: HttpRequest, next: Next) -> Response {
    if let Some(token) = &config.token {
        let supplied = request
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|h| h.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .unwrap_or_default();
        if !bool::from(supplied.as_bytes().ct_eq(token.as_bytes())) {
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer realm=\"ouro-console\"")],
                "Bearer token required",
            )
                .into_response();
        }
    }
    let host = request
        .headers()
        .get(header::HOST)
        .and_then(|h| h.to_str().ok())
        .unwrap_or_default();
    // Without auth, reject foreign Host values to protect the loopback operator surface.
    if config.token.is_none()
        && !host
            .parse::<axum::http::uri::Authority>()
            .ok()
            .is_some_and(|a| matches!(a.host(), "localhost" | "127.0.0.1" | "[::1]"))
    {
        return (StatusCode::FORBIDDEN, "Untrusted host").into_response();
    }
    if let Some(origin) = request.headers().get(header::ORIGIN) {
        let valid = origin
            .to_str()
            .ok()
            .and_then(|o| url::Url::parse(o).ok())
            .is_some_and(|o| {
                matches!(o.scheme(), "http" | "https")
                    && o[url::Position::BeforeHost..url::Position::AfterPort] == *host
            });
        if !valid {
            return (StatusCode::FORBIDDEN, "Cross-origin requests are disabled").into_response();
        }
    }
    let mut response = next.run(request).await;
    let headers = response.headers_mut();
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        "nosniff".parse().expect("static header"),
    );
    headers.insert(
        header::REFERRER_POLICY,
        "no-referrer".parse().expect("static header"),
    );
    headers.insert(header::CONTENT_SECURITY_POLICY,"default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'".parse().expect("static header"));
    headers.insert(
        header::CACHE_CONTROL,
        "no-store".parse().expect("static header"),
    );
    response
}
impl IntoResponse for ControlError {
    fn into_response(self) -> Response {
        let code = match &self {
            Self::Invalid(_) => StatusCode::BAD_REQUEST,
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Conflict(_) => StatusCode::CONFLICT,
            Self::ReadOnly => StatusCode::FORBIDDEN,
            Self::Unavailable(_) => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (code,html! { main class="container py-5" { h1 { "Request failed" } p { (self.to_string()) } a href="/runs" { "Back to runs" } } }).into_response()
    }
}
async fn asset(Path(path): Path<String>) -> Response {
    match Assets::get(&path) {
        Some(file) => {
            let mime = if path.ends_with(".css") {
                "text/css"
            } else if path.ends_with(".js") {
                "text/javascript"
            } else if path.ends_with(".woff2") {
                "font/woff2"
            } else if path.ends_with(".woff") {
                "font/woff"
            } else if path.ends_with(".ttf") {
                "font/ttf"
            } else {
                "text/plain"
            };
            (
                [(header::CONTENT_TYPE, mime)],
                Body::from(file.data.into_owned()),
            )
                .into_response()
        }
        None => StatusCode::NOT_FOUND.into_response(),
    }
}
fn icon(name: &str) -> Markup {
    html! { i class=(format!("ti ti-{name}")) aria-hidden="true" {} }
}
fn age(ms: i64) -> String {
    let seconds = (now_ms() - ms).max(0) / 1000;
    if seconds < 60 {
        format!("{seconds}s ago")
    } else if seconds < 3600 {
        format!("{}m ago", seconds / 60)
    } else if seconds < 86400 {
        format!("{}h ago", seconds / 3600)
    } else {
        format!("{}d ago", seconds / 86400)
    }
}
fn time(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| ms.to_string())
}
fn badge(state: RunState) -> Markup {
    let color = match state {
        RunState::Ready => "secondary",
        RunState::Running => "blue",
        RunState::Waiting => "yellow",
        RunState::Completed => "green",
        RunState::Failed => "red",
        RunState::Cancelled => "secondary",
    };
    html! { span class=(format!("badge bg-{color}-lt")) { (state.label()) } }
}
/// The hold marker beside a run's state: who froze it is in the tooltip.
fn held_badge(held: Option<&Hold>) -> Markup {
    html! { @if let Some(h) = held { " " span class="badge bg-orange-lt" title=(format!("held by {} · {}", h.actor, h.reason)) { "held" } } }
}
/// A content digest, shortened; the whole value is its tooltip.
fn digest_view(digest: &str) -> Markup {
    html! { @if digest.is_empty() { span class="text-secondary" { "none (version not in the catalog)" } }
    @else { code title=(digest) { (digest.chars().take(12).collect::<String>()) "…" } } }
}
/// A call's recorded state. Outcomes that are not a plain answer say what
/// they mean for the effect: not executed (none), unknown (it may have run
/// and is never sent again), interrupted (the worker stopped; an
/// at-least-once call is sent again on recovery).
fn call_state(call: &CallEntry, run_interrupted: bool) -> Markup {
    let (color, note) = match call.state.as_str() {
        "not_executed" => ("azure", Some("proven not executed: no effect")),
        "unknown" => ("orange", Some("may have run; never sent again")),
        "pending" if run_interrupted && call.delivery == "at_least_once" => (
            "purple",
            Some("interrupted: sent again when the run recovers"),
        ),
        "pending" => ("secondary", None),
        "failed" => ("red", None),
        _ => ("green", None),
    };
    html! { span class=(format!("badge bg-{color}-lt")) { code { (call.state) } }
    @if let Some(note) = note { div class="text-secondary small" { (note) } } }
}
fn host_view(host: &Option<HostData>) -> Markup {
    html! { @match host {
        None => { span class="text-secondary" { "none" } },
        Some(HostData::Hidden) => { span class="text-secondary" { "host data (hidden)" } },
        Some(HostData::Shown(data)) => { pre class="mb-0" { (data) } },
    } }
}
fn status_badge(stopped: bool) -> Markup {
    html! { span id="runtime-status" hx-get="/status" hx-trigger="every 2s" hx-swap="outerHTML" class=(if stopped { "badge bg-red-lt" } else { "badge bg-green-lt" }) { span class="status-dot" {} (if stopped { "Emergency stop SET" } else { "Emergency stop clear" }) } }
}
async fn status(State(app): State<App>) -> Result<Markup> {
    Ok(status_badge(app.session.client.stopped().await?))
}
async fn layout(app: &App, title: &str, subtitle: &str, content: Markup) -> Result<Markup> {
    let stopped = app.session.client.stopped().await?;
    Ok(html! { (DOCTYPE) html lang="en" data-bs-theme="light" {
        head { meta charset="utf-8"; meta name="viewport" content="width=device-width, initial-scale=1";
            meta name="htmx-config" content=r#"{"allowEval":false,"includeIndicatorStyles":false}"#;
            title { (title) " · ouro console" }
            link rel="stylesheet" href="/assets/vendor/tabler/tabler.min.css";
            link rel="stylesheet" href="/assets/vendor/tabler-icons/tabler-icons.min.css";
            link rel="stylesheet" href="/assets/console.css";
            script src="/assets/console.js" defer {} script src="/assets/vendor/htmx/htmx.min.js" defer {} script src="/assets/vendor/tabler/tabler.min.js" defer {}
        }
        body { div class="page" {
            header class="navbar navbar-expand-md sticky-top" { div class="container-xl console-header" {
                a class="navbar-brand" href="/runs" { span class="brand-mark" { "o" } "ouro" span class="text-secondary fw-normal" { " / console" } }
                div class="header-controls" { (status_badge(stopped))
                    @if !app.session.read_only { a class="btn btn-danger" href="/emergency" { (icon("hand-stop")) "Emergency stop" } }
                    button type="button" id="theme-toggle" class="btn btn-icon" aria-label="Toggle light and dark theme" { (icon("sun-moon")) }
                }
            } }
            div class="navbar navbar-expand-md console-nav" { nav class="container-xl" aria-label="Main navigation" {
                div class="nav nav-pills gap-1" { @for (label,href,ico) in [("Runs","/runs","activity"),("Decisions","/pending","message-question"),("Definitions","/definitions","stack-2"),("Workers","/workers","server"),("Operator journal","/journal","history")] {
                    a class=(if label == title { "nav-link active" } else { "nav-link" }) href=(href) { (icon(ico)) (label) }
                } }
            } }
            main class="page-wrapper" { div class="container-xl py-4" {
                div class="page-header mb-4" { div class="row align-items-center" { div class="col" {
                    div class="page-pretitle" { "OPERATIONS / " (if app.session.read_only { "READ ONLY" } else { "OPERATOR" }) }
                    h1 class="page-title" { (title) } p class="text-secondary mt-2 mb-0" { (subtitle) }
                } div class="col-auto" { span class="badge bg-azure-lt" { (app.session.client.backend_label()) } } } }
                (content)
            } footer class="container-xl py-3 text-secondary small" { "Actor: " (app.session.actor) " · ouro-console v0.2" } }
        } }
    } })
}
fn field(name: &str, value: &str) -> Markup {
    html! { input type="hidden" name=(name) value=(value); }
}
fn reason_form(action: &str, label: &str, fields: Markup, extra: Markup) -> Markup {
    html! { form method="post" action=(format!("/actions/{action}")) class="action-form" {
        (field("request_id",&uuid::Uuid::new_v4().to_string())) (fields) (extra)
        label class="form-label mt-2" { "Reason" input class="form-control" name="reason" required maxlength="4096" placeholder="Why is this action needed?"; }
        label class="form-check my-3" { input class="form-check-input" type="checkbox" name="confirmed" value="true" required; span class="form-check-label" { "I confirm: " (label) } }
        button type="submit" class="btn btn-danger" { (label) }
    } }
}
fn compact_action(action: &str, label: &str, fields: Markup) -> Markup {
    html! { details class="action-disclosure" { summary class="text-danger" { (label) } (reason_form(action,label,fields,html!{})) } }
}
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
struct RunQuery {
    definition: Option<String>,
    #[serde(default, deserialize_with = "empty_state")]
    state: Option<RunState>,
    parent: Option<String>,
    since_ms: Option<i64>,
    before_ms: Option<i64>,
    age_minutes: Option<i64>,
    #[serde(default)]
    history: bool,
    cursor: Option<Cursor>,
    limit: Option<usize>,
}
fn empty_state<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<Option<RunState>, D::Error> {
    let value = Option::<String>::deserialize(deserializer)?;
    match value.as_deref() {
        None | Some("") => Ok(None),
        Some(v) => serde_json::from_value(Value::String(v.into()))
            .map(Some)
            .map_err(serde::de::Error::custom),
    }
}
impl RunQuery {
    fn normalize(&mut self) -> Result<()> {
        if self.definition.as_ref().is_some_and(String::is_empty) {
            self.definition = None;
        }
        if let Some(minutes) = self.age_minutes {
            if !(1..=525600).contains(&minutes) {
                return Err(ControlError::Invalid(
                    "age_minutes must be 1..=525600".into(),
                ));
            }
            self.since_ms.get_or_insert(now_ms() - minutes * 60_000);
        }
        if self.history {
            self.since_ms.get_or_insert(now_ms() - 86_400_000);
            self.before_ms.get_or_insert(now_ms());
        }
        Ok(())
    }
    fn filter(&self) -> Runs {
        Runs {
            definition: self.definition.clone(),
            state: self.state,
            parent: self.parent.clone(),
            since_ms: self.since_ms,
            before_ms: self.before_ms,
        }
    }
    fn url(&self) -> String {
        format!(
            "/runs?{}",
            serde_urlencoded::to_string(self).expect("query serialization")
        )
    }
}
async fn runs(
    State(app): State<App>,
    Query(mut query): Query<RunQuery>,
    headers: HeaderMap,
) -> Result<Markup> {
    query.normalize()?;
    let filter = query.filter();
    let limit = query.limit.unwrap_or(5);
    let page = if query.history {
        app.session
            .client
            .history(&filter, query.cursor.as_ref(), limit)
            .await?
    } else {
        app.session
            .client
            .list(&filter, query.cursor.as_ref(), limit)
            .await?
    };
    let poll_url = query.url();
    let panel = html! { section id="runs-panel" class="card" hx-get=(poll_url) hx-trigger="every 2s" hx-swap="outerHTML" {
        div class="card-header" { h2 class="card-title" { (if query.history { "Terminal history" } else { "Run activity" }) } span class="ms-auto text-secondary small" { (icon("refresh")) " Live · 2s" } }
        div class="table-responsive" { table class="table table-vcenter card-table" { thead { tr { th { "Run / definition" } th { "State" } th { "Version" } th { "Admitted" } th { "Tokens" } th { "Parent" } @if !app.session.read_only { th { "Action" } } } }
            tbody { @for run in &page.items { tr {
                td { a class="fw-semibold" href=(format!("/runs/{}",run.id)) { (run.id) } div class="text-secondary small" { (run.definition) @if matches!(run.host, Some(HostData::Hidden)) { " · host data (hidden)" } } }
                td { (badge(run.state)) (held_badge(run.held.as_ref())) } td { code { (run.version) } } td title=(time(run.admitted_ms)) { (age(run.admitted_ms)) }
                td { (run_tokens(&run.annotations)) }
                td { @if let Some(parent) = &run.parent { a href=(format!("/runs/{parent}")) { (parent) } } @else { "—" } }
                @if !app.session.read_only { td { @if !run.state.terminal() { div class="btn-list flex-nowrap" { a class="btn btn-sm btn-outline-warning" href=(format!("/runs/{}#actions",run.id)) { (if run.held.is_some() { "Release" } else { "Hold" }) } a class="btn btn-sm btn-outline-danger" href=(format!("/runs/{}#actions",run.id)) { "Cancel" } } } } }
            } }
            @if page.items.is_empty() { tr { td colspan=(if app.session.read_only { 6 } else { 7 }) class="text-secondary p-4" { "No matching runs in this page." @if page.next.is_some() { " More candidates are available; continue to the next page." } } } }
        } } }
        div class="card-footer d-flex align-items-center" { span class="text-secondary small" { (page.items.len()) " matches · bounded page" @if let Some(sum) = page_tokens(&page.items) { " · " (sum) " on this page" } }
            div class="ms-auto btn-list" { @if query.cursor.is_some() { @let mut first = query.clone(); @let _ = first.cursor.take(); a class="btn btn-sm" href=(first.url()) { "First page" } }
                @if let Some(next) = page.next { @let mut q = query.clone(); @let _ = q.cursor.replace(next); a class="btn btn-sm btn-primary" href=(q.url()) { "Next page " (icon("arrow-right")) } }
            }
        }
    } };
    if headers.get("hx-request").is_some_and(|h| h == "true") {
        return Ok(panel);
    }
    // One bounded page feeds the selector; a larger catalog gets a text field.
    let defs = app.session.client.definitions(None, 200).await?;
    layout(&app,"Runs","Inspect execution, follow owned trees, and stop work when it needs attention.",html! {
        form method="get" action="/runs" class="card card-body mb-4" { div class="row g-3 align-items-end" {
            div class="col-md" { label class="form-label" for="definition" { "Definition" } @if defs.next.is_some() { input id="definition" class="form-control" name="definition" placeholder="All definitions" value=(query.definition.as_deref().unwrap_or_default()); } @else { select id="definition" class="form-select" name="definition" { option value="" { "All definitions" } @for d in &defs.items { option value=(d.name) selected[query.definition.as_ref() == Some(&d.name)] { (d.name) } } } } }
            div class="col-md" { label class="form-label" for="state" { "State" } select id="state" class="form-select" name="state" { option value="" { "All states" } @for s in RunState::ALL { option value=(s.label()) selected[query.state == Some(s)] { (s.label()) } } } }
            div class="col-md" { label class="form-label" for="age" { "Age" } select id="age" class="form-select" name="age_minutes" { @for (n,label) in [(60,"Last hour"),(1440,"Last day"),(10080,"Last week")] { option value=(n) selected[query.age_minutes.unwrap_or(1440) == n] { (label) } } } }
            div class="col-md" { label class="form-label" for="history" { "Time index" } select id="history" class="form-select" name="history" { option value="false" selected[!query.history] { "Admitted runs" } option value="true" selected[query.history] { "Ended runs" } } }
            div class="col-auto" { button class="btn btn-primary" type="submit" { (icon("filter")) "Apply filters" } }
        } } (panel)
    }).await
}
async fn children_tree(app: &App, root: &Inspection) -> Result<Markup> {
    let mut rows = vec![];
    let mut stack: Vec<_> = root
        .children
        .iter()
        .rev()
        .cloned()
        .map(|c| (c, 0usize))
        .collect();
    let mut seen = BTreeSet::new();
    while let Some((child, depth)) = stack.pop() {
        if !seen.insert(child.id.clone()) || seen.len() > 1000 {
            break;
        }
        if let Some(i) = app.session.client.inspect(&child.id).await? {
            for c in i.children.into_iter().rev() {
                stack.push((c, depth + 1));
            }
        }
        rows.push((child, depth));
    }
    Ok(
        html! { ul class="list-group list-group-flush" { @for (child,depth) in &rows { li class="list-group-item" { span style=(format!("padding-left:{}px",depth*20)) { (icon("corner-down-right")) " " a href=(format!("/runs/{}",child.id)) { (child.id) } " " (badge(child.state)) (held_badge(child.held.as_ref())) } } } @if rows.is_empty() { li class="list-group-item text-secondary" { "No owned children" } } } },
    )
}
async fn detail(State(app): State<App>, Path(id): Path<String>) -> Result<Markup> {
    let i = app
        .session
        .client
        .inspect(&id)
        .await?
        .ok_or_else(|| ControlError::NotFound("run not found".into()))?;
    let tree = children_tree(&app, &i).await?;
    let trace = app
        .config
        .trace_url
        .as_ref()
        .zip(i.trace_id.as_ref())
        .map(|(template, id)| {
            template.replace(
                "{trace_id}",
                &url::form_urlencoded::byte_serialize(id.as_bytes()).collect::<String>(),
            )
        });
    layout(&app,&format!("Run {id}"),"Pinned execution evidence and operator controls.",html! {
        div class="row g-4" { div class="col-lg-8" {
            section class="card mb-4" { div class="card-header" { h2 class="card-title" { "Execution" } span class="ms-auto" { (badge(i.run.state)) (held_badge(i.run.held.as_ref())) @if i.run.interrupted { " " span class="badge bg-purple-lt" { "interrupted" } } } }
                div class="card-body" { dl class="row mb-0" {
                    dt class="col-sm-3" { "Definition / version" } dd class="col-sm-9" { (i.run.definition) " / " code { (i.run.version) } }
                    dt class="col-sm-3" { "Admitted" } dd class="col-sm-9" { (time(i.run.admitted_ms)) }
                    dt class="col-sm-3" { "Version digest" } dd class="col-sm-9" { (digest_view(&i.digest)) " " @if i.pinned { span class="badge bg-azure-lt" title="Started at a version the host named" { "pinned version" } } @else { span class="text-secondary small" { "started at the active version" } } }
                    @if let Some(h) = &i.run.held { dt class="col-sm-3 text-orange" { "Held" } dd class="col-sm-9" { "By " strong { (h.actor) } " · " span title=(time(h.at_ms)) { (age(h.at_ms)) } p class="mb-0" { (h.reason) } @if !h.note.is_empty() { p class="text-secondary mb-0" { (h.note) } } div class="text-secondary small" { "Frozen with its live owned descendants; nothing is claimed or due until released." } } }
                    dt class="col-sm-3" { "Host data" } dd class="col-sm-9" { (host_view(&i.run.host)) }
                    dt class="col-sm-3" { "Lease holder / expiry" } dd class="col-sm-9" { @if let Some(lease) = &i.lease { (lease.holder) " · " (time(lease.expires_ms)) } @else { "No active lease" } }
                    dt class="col-sm-3" { "Jump budget" } dd class="col-sm-9" { (i.budget_spent) " spent / " (i.budget_limit.map(|n|n.to_string()).unwrap_or_else(||"unbounded".into())) }
                    dt class="col-sm-3" { "Trace" } dd class="col-sm-9" { @match &i.trace_id {
                        None => { span class="text-secondary" { "No span context was pinned at admission" } },
                        Some(trace_id) => {
                            code { (trace_id) } @if let Some(span) = &i.span_id { span class="text-secondary small" { " · span " code { (span) } } }
                            div { @if let Some(link) = trace { a href=(link) target="_blank" rel="noopener noreferrer" { "Open trace " (icon("external-link")) } } @else { span class="text-secondary small" { "Set --trace-url to link your trace backend" } } }
                        },
                    } }
                    @if let Some(error) = &i.run.error { dt class="col-sm-3 text-danger" { (if i.run.state.terminal() { "Terminal error" } else { "Last interruption" }) } dd class="col-sm-9" { pre { (error) } } }
                } }
            }
            section id="usage" class="card mb-4" { div class="card-header" { h2 class="card-title" { "Usage" } span class="ms-auto text-secondary small" { "Call annotations summed per name" } }
                div class="card-body" { @let totals = shown(i.annotations.iter().map(|(k, v)| (k, v.to_string())));
                    @if totals.is_empty() { p class="text-secondary mb-0" { "No numeric annotations recorded" } } @else { (annotations(&totals, true)) }
                }
            }
            section class="card mb-4" { div class="card-header" { h2 class="card-title" { "Call journal" } } div class="table-responsive" { table class="table card-table" {
                thead { tr { th { "Function / key" } th { "Delivery" } th { "State" } th { "Annotations" } th { "Evidence" } } }
                tbody { @for call in &i.calls { tr { td { (call.function) div class="text-secondary small" { (call.key) } } td { (call.delivery) } td { (call_state(call, i.run.interrupted)) }
                    td { @let a = shown(call.annotations.iter().map(|(k, v)| (k, v.as_str().map_or_else(|| v.to_string(), str::to_owned)))); @if a.is_empty() { span class="text-secondary" { "—" } } @else { (annotations(&a, false)) } }
                    td { details { summary { "Arguments / result" } pre { (call.arguments) } pre { (json!(call.result)) } } } } } }
            } } }
            section class="card mb-4" { div class="card-header" { h2 class="card-title" { "Pending waits" } } div class="card-body" { @for (key,w) in &i.waits { h3 { (key) } (wait_info(w)) } @if i.waits.is_empty() { p class="text-secondary mb-0" { "No pending waits" } } a href="/pending" { "Open decision queue" } } }
            section class="card" { div class="card-header" { h2 class="card-title" { "Mailbox copies" } } div class="card-body" { pre { (json!(i.mailbox)) } } }
        } div class="col-lg-4" {
            section class="card mb-4" { div class="card-header" { h2 class="card-title" { "Owned children tree" } } (tree) }
            @if !app.session.read_only { section id="actions" class="card mb-4" { div class="card-header" { h2 class="card-title" { "Run controls" } } div class="card-body" {
                @if !i.run.state.terminal() {
                    p class="text-secondary" { "Hold freezes this run and its live owned descendants without ending them; a call already dispatched still records its answer. Release lets them continue from the same place." }
                    @if i.run.held.is_some() { (reason_form("release","Release run",field("run",&id),html!{})) }
                    @else { (reason_form("hold","Hold run",field("run",&id),html! { label class="form-label" { "Note" input class="form-control" name="note" placeholder="Optional note"; } })) }
                    hr;
                }
                p class="text-secondary" { "Both commands cancel this run and its owned descendants atomically (a hold ends). Dispatched effects cannot be retracted." }
                (compact_action("cancel","Cancel run",field("run",&id))) hr; (compact_action("cancel-tree","Cancel tree",field("run",&id)))
            } } }
            a href=(format!("/journal?run={id}")) { "Operator journal for this run " (icon("arrow-right")) }
        } }
    }).await
}
/// The GenAI semantic-convention names ouro's README recommends for model
/// steps. Shown first; any other annotation is a plain key/value.
const INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
const OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
fn shown<'a>(items: impl Iterator<Item = (&'a String, String)>) -> Vec<(&'a str, String)> {
    items.map(|(k, v)| (k.as_str(), v)).collect()
}
fn annotations(items: &[(&str, String)], large: bool) -> Markup {
    let find = |name: &str| items.iter().find(|(k, _)| *k == name).map(|(_, v)| v);
    let tokens = [
        ("Input tokens", INPUT_TOKENS),
        ("Output tokens", OUTPUT_TOKENS),
    ]
    .into_iter()
    .filter_map(|(label, name)| find(name).map(|v| (label, name, v)))
    .collect::<Vec<_>>();
    let rest = items
        .iter()
        .filter(|(k, _)| *k != INPUT_TOKENS && *k != OUTPUT_TOKENS);
    html! {
        @if !tokens.is_empty() { div class=(if large { "token-usage d-flex gap-4 mb-2" } else { "token-usage" }) {
            @for (label, name, value) in &tokens { div class="token-count" title=(name) {
                @if large { div class="h2 mb-0" { (value) } div class="text-secondary small" { (label) " · " code { (name) } } }
                @else { span class="badge bg-azure-lt me-1" { (label) ": " (value) } }
            } }
        } }
        dl class="annotation-list row mb-0 small" { @for (k, v) in rest { dt class="col-sm-5 fw-normal" { code { (k) } } dd class="col-sm-7 mb-1" { (v) } } }
    }
}
/// A token count recorded under a GenAI name, when it is an integer.
fn count(totals: &BTreeMap<String, serde_json::Number>, name: &str) -> Option<i64> {
    totals.get(name).and_then(serde_json::Number::as_i64)
}
/// "input in / output out" for a run's totals, or a dash when it recorded none.
fn run_tokens(totals: &BTreeMap<String, serde_json::Number>) -> Markup {
    let (input, output) = (count(totals, INPUT_TOKENS), count(totals, OUTPUT_TOKENS));
    html! { @if input.is_none() && output.is_none() { span class="text-secondary" { "—" } }
    @else { span title=(format!("{INPUT_TOKENS} / {OUTPUT_TOKENS}")) { (input.unwrap_or(0)) " in / " (output.unwrap_or(0)) " out" } } }
}
/// The page's tokens summed from the runs' own totals, when any run has them.
fn page_tokens(runs: &[SavedRun]) -> Option<String> {
    let sum = |name: &str| {
        runs.iter()
            .filter_map(|r| count(&r.annotations, name))
            .sum::<i64>()
    };
    runs.iter()
        .any(|r| {
            count(&r.annotations, INPUT_TOKENS)
                .or(count(&r.annotations, OUTPUT_TOKENS))
                .is_some()
        })
        .then(|| {
            format!(
                "{} in / {} out tokens",
                sum(INPUT_TOKENS),
                sum(OUTPUT_TOKENS)
            )
        })
}
fn wait_info(w: &PendingWait) -> Markup {
    html! {
        p { strong { (w.types.join(", ")) } " · " (w.strategy) " · since " span title=(time(w.since_ms)) { (age(w.since_ms)) } }
        p class="text-secondary small" { "Timeout: " (w.timeout_ms.map(time).unwrap_or_else(||"none".into())) }
        details { summary { "Matcher and progress" } pre { "Filters: " (w.filters) "\nExpectations: " (w.expectations) "\nProgress: " (w.progress) "\nPosition: " (w.position) "\nRedacted: " (w.redacted) } }
    }
}
#[derive(Default, Deserialize, Serialize)]
struct PendingQuery {
    definition: Option<String>,
    run: Option<String>,
    event_type: Option<String>,
    since_ms: Option<i64>,
    before_ms: Option<i64>,
    cursor: Option<Cursor>,
    limit: Option<usize>,
}
async fn pending(State(app): State<App>, Query(q): Query<PendingQuery>) -> Result<Markup> {
    let page = app
        .session
        .client
        .waits(
            &Waits {
                definition: q.definition.clone(),
                run: q.run.clone(),
                event_type: q.event_type.clone(),
                since_ms: q.since_ms,
                before_ms: q.before_ms,
            },
            q.cursor.as_ref(),
            q.limit.unwrap_or(50),
        )
        .await?;
    layout(&app,"Decisions","Waiting for a decision. Answers target the exact persisted listen position.",html! {
        div class="row g-4" { @for w in &page.items { div class="col-lg-6" { section class="card h-100" { div class="card-header" { h2 class="card-title" { a href=(format!("/runs/{}",w.run)) { (w.run) } " / " (w.definition) } span class="badge bg-yellow-lt ms-auto" { "waiting" } }
            div class="card-body" { p class="text-secondary small" { (w.key) } (wait_info(&w.waiting))
                @if !app.session.read_only { hr; (reason_form("respond","Send answer",html! { (field("run",&w.run)) (field("wait",&w.key)) },html! {
                    label class="form-label" { "Answer" select class="form-select" name="decision" { option value="approve" { "Approve" } option value="reject" { "Reject" } option value="json" { "JSON payload" } } }
                    label class="form-label mt-2" { "JSON payload (when selected)" textarea class="form-control font-monospace" name="payload" rows="3" { "{}" } }
                    p class="form-hint" { "The console creates the CloudEvent type and subject from this wait. JSON is sent as data." }
                })) }
            }
        } } } }
        @if page.items.is_empty() { div class="card card-body text-secondary" { "No pending waits in this page." } }
        @if let Some(next) = page.next { @let mut next_q = q; @let _ = next_q.cursor.replace(next); a class="btn mt-3" href=(format!("/pending?{}",serde_urlencoded::to_string(next_q).expect("query"))) { "Next page" } }
    }).await
}
#[derive(Deserialize, Serialize)]
struct PageQuery {
    cursor: Option<Cursor>,
    limit: Option<usize>,
}
fn next_link(path: &str, q: PageQuery, next: Option<Cursor>) -> Markup {
    html! { @if let Some(next) = next { @let mut q = q; @let _ = q.cursor.replace(next); a class="btn mt-3" href=(format!("{path}?{}",serde_urlencoded::to_string(q).expect("query"))) { "Next page" } } }
}
fn breaker_view(breaker: &BreakerTrip, digest: &str) -> Markup {
    html! { span class="badge bg-red-lt" { "automatically tripped" } " " span class="text-secondary small" title=(time(breaker.at_ms)) { (age(breaker.at_ms)) }
    p class="mt-2" { (breaker.faults) " / " (breaker.threshold) " faults · " a href=(format!("/runs/{}", breaker.run)) { (breaker.run) } } p { (breaker.error) }
    p class="text-secondary small" { "Refuses admissions with content digest " (digest_view(digest)) " (of any version number) and versions the catalog does not hold; other content is admitted. Activate a version in the host to clear this trip." } }
}
fn schedule_info(s: &Schedule, paused: bool) -> Markup {
    html! { div {
        strong { (s.kind) } " · "
        @match s.next_due_ms { Some(due) => { "next " span title=(time(due)) { (time(due)) } }, None => { span class="text-secondary" { "no next due time" } } }
        @if paused { " · " span class="badge bg-yellow-lt" { "suspended" } }
        div class="text-secondary small" { "Overlap " (s.overlap) " · catch-up " (s.catchup)
            @if let Some(q) = &s.queued { " · queued slot " a href=(format!("/runs/{}",q.run)) { (q.run) } " (version " (q.version) ")" } }
    } }
}
/// One version with its latest activation: when, and by whom and why if the host said.
fn version_info(v: &Version) -> Markup {
    html! { div { code { (v.version) } " · "
        @match v.activated_ms { Some(at) => { "activated " span title=(time(at)) { (time(at)) } }, None => { span class="text-secondary" { "never activated" } } }
        @if let Some(by) = &v.activated_by { " by " (by) }
        @if let Some(note) = &v.activation_note { span class="text-secondary" { " · " (note) } }
        div class="text-secondary small" { "digest " (digest_view(&v.digest)) " · breaker policy " code { (v.breaker.label()) } }
    } }
}
#[derive(Deserialize)]
struct TotalsQuery {
    hours: Option<i64>,
}
async fn definition_page(
    State(app): State<App>,
    Path(name): Path<String>,
    Query(q): Query<TotalsQuery>,
) -> Result<Markup> {
    let hours = q.hours.unwrap_or(24);
    if !(1..=720).contains(&hours) {
        return Err(ControlError::Invalid("hours must be 1..=720".into()));
    }
    let d = app
        .session
        .client
        .definition(&name)
        .await?
        .ok_or_else(|| ControlError::NotFound("definition not found".into()))?;
    let (since, before) = totals_window(hours.checked_mul(3_600_000).map(|ms| now_ms() - ms), None);
    let totals = app.session.client.totals(&name, since, before).await?;
    layout(&app,&format!("Definition {name}"),"Versions and the usage of runs that ended in a window.",html! {
        section class="card mb-4" { div class="card-header" { h2 class="card-title" { "Versions" } span class="badge bg-secondary-lt ms-auto" { "active " (d.active.as_deref().unwrap_or("none")) } }
            div class="card-body" { @for v in &d.versions { (version_info(v)) } } }
        section id="totals" class="card" { div class="card-header" { h2 class="card-title" { "Totals" }
            form method="get" class="ms-auto" { select class="form-select" name="hours" aria-label="Window" { @for (n, label) in [(1, "Last hour"), (24, "Last 24 hours"), (168, "Last 7 days"), (720, "Last 30 days")] { option value=(n) selected[hours == n] { (label) } } } button class="btn mt-2" type="submit" { "Show" } } }
            div class="card-body" {
                p class="text-secondary" { (totals.runs) " runs ended with annotation totals, " span title=(time(totals.since_ms)) { (time(totals.since_ms)) } " to " span title=(time(totals.before_ms)) { (time(totals.before_ms)) } " (ouro rounds the window out to whole hours)." }
                @if totals.annotations.is_empty() { p class="text-secondary mb-0" { "No numeric annotations recorded in this window" } }
                @else { table class="table card-table" { thead { tr { th { "Annotation" } th { "Total" } } }
                    tbody { @for (k, v) in &totals.annotations { tr { td { code { (k) } } td { (v) } } } } } }
            } }
    }).await
}
async fn definitions(State(app): State<App>, Query(q): Query<PageQuery>) -> Result<Markup> {
    let page = app
        .session
        .client
        .definitions(q.cursor.as_ref(), q.limit.unwrap_or(50))
        .await?;
    layout(&app,"Definitions","Versions, admission breakers, and schedules. Existing work finishes during a manual pause.",html! {
        div class="row g-4" { @for d in &page.items { div class="col-lg-6" { section class="card h-100" { div class="card-header" { h2 class="card-title" { a href=(format!("/definitions/{}", d.name)) { (d.name) } } span class="badge bg-secondary-lt ms-auto" { "epoch " (d.epoch) } }
            div class="card-body" {
                dl class="row" { dt class="col-sm-4" { "Active version" } dd class="col-sm-8" { code { (d.active.as_deref().unwrap_or("none")) } }
                    dt class="col-sm-4" { "Versions" } dd class="col-sm-8" { @for v in &d.versions { (version_info(v)) } }
                    dt class="col-sm-4" { "Schedule" } dd class="col-sm-8" { @if d.schedules.is_empty() { "No schedule" } @for s in &d.schedules { (schedule_info(s, matches!(d.tripped, Some(Trip::Manual { .. })))) } }
                    dt class="col-sm-4" { "Breaker" } dd class="col-sm-8" { @match &d.tripped {
                        None => { span class="badge bg-green-lt" { "healthy" } " · " (d.faults) " consecutive faults" },
                        Some(Trip::Automatic { breaker, digest }) => { (breaker_view(breaker, digest)) },
                        Some(Trip::Manual { actor, reason, note, at_ms, digest, breaker }) => { span class="badge bg-yellow-lt" { "manually paused" } " " span class="text-secondary small" title=(time(*at_ms)) { (age(*at_ms)) } p class="mt-2" { "By " (actor) " · " (reason) } p class="text-secondary" { (note) }
                            p class="text-secondary small" { "A manual pause refuses every admission of every version, whatever its content." }
                            @if let Some(breaker) = breaker { hr; (breaker_view(breaker, digest)) } },
                    } }
                }
                @if !app.session.read_only { @match &d.tripped {
                    None => { (reason_form("pause","Pause definition",field("definition",&d.name),html! { label class="form-label" { "Note" input class="form-control" name="note" placeholder="Optional maintenance note"; } })) },
                    Some(Trip::Manual {..}) => { (reason_form("resume","Resume definition",field("definition",&d.name),html!{})) },
                    Some(Trip::Automatic { .. }) => { (reason_form("pause","Pause definition",field("definition",&d.name),html! { label class="form-label" { "Note" input class="form-control" name="note" placeholder="Optional maintenance note"; } })) },
                } }
            }
        } } } }
        @if page.items.is_empty() { div class="card card-body text-secondary" { "No definitions in this page." } }
        (next_link("/definitions", q, page.next))
    }).await
}
async fn workers(State(app): State<App>, Query(q): Query<PageQuery>) -> Result<Markup> {
    let page = app
        .session
        .client
        .workers(q.cursor.as_ref(), q.limit.unwrap_or(50))
        .await?;
    layout(&app,"Workers","Heartbeat rows the workers write. A worker that does not beat again before its expiry is stale.",html! {
        section class="card" { div class="table-responsive" { table class="table card-table table-vcenter" {
            thead { tr { th { "Worker" } th { "Liveness" } th { "Last beat" } th { "Runs held" } th { "Application version" } th { "Host" } th { "Started" } } }
            tbody { @for w in &page.items { tr {
                td { strong class="font-monospace" { (w.id) } }
                td { @if w.stale { span class="badge bg-red-lt" { "stale" } } @else { span class="badge bg-green-lt" { "live" } } }
                td title=(time(w.beat_ms)) { (age(w.beat_ms)) div class="text-secondary small" title=(time(w.expires_ms)) { @if w.stale { "expired " } @else { "expires " } (time(w.expires_ms)) } }
                td { (w.held) }
                td { @if let Some(v) = &w.application { code { (v) } } @else { span class="text-secondary" { "undeclared" } } }
                td { (w.host.as_deref().unwrap_or("—")) }
                td title=(time(w.started_ms)) { (age(w.started_ms)) }
            } }
            @if page.items.is_empty() { tr { td colspan="7" class="text-secondary p-4" { "No worker heartbeat rows in this page." } } }
            }
        } } }
        (next_link("/workers", q, page.next))
    }).await
}
#[derive(Deserialize, Serialize)]
struct JournalQuery {
    run: Option<String>,
    cursor: Option<Cursor>,
    limit: Option<usize>,
}
async fn journal(State(app): State<App>, Query(q): Query<JournalQuery>) -> Result<Markup> {
    let page = app
        .session
        .client
        .journal(q.run.as_deref(), q.cursor.as_ref(), q.limit.unwrap_or(50))
        .await?;
    layout(&app,"Operator journal","Who did what, why, and the reply committed with each request.",html! {
        section class="card" { div class="table-responsive" { table class="table card-table" { thead { tr { th { "When / request ID" } th { "Actor / reason" } th { "Command" } th { "Reply" } } }
            tbody { @for e in &page.items { tr { td { (time(e.at_ms)) div class="text-secondary small font-monospace" { (e.request.id) } } td { strong { (e.request.actor) } div { (e.request.reason) } } td { pre { (json!(e.command)) } } td { code { (json!(e.reply)) } } } } }
        } } }
        @if let Some(next) = page.next { @let mut next_q = q; @let _ = next_q.cursor.replace(next); a class="btn mt-3" href=(format!("/journal?{}",serde_urlencoded::to_string(next_q).expect("query"))) { "Next page" } }
    }).await
}
async fn emergency(State(app): State<App>) -> Result<Markup> {
    let stopped = app.session.client.stopped().await?;
    layout(&app,"Emergency stop","Store-wide execution control. The flag is checked at dispatch boundaries and worker heartbeats.",html! {
        section class="card border-danger" { div class="card-header" { h2 class="card-title text-danger" { (icon("hand-stop")) " " (if stopped { "All execution is stopped" } else { "Emergency stop is clear" }) } }
            div class="card-body" { p { "Stopping blocks admissions and further execution. In-flight work releases for recovery at a boundary or heartbeat. Resuming continues from the same evidence." }
                @if !app.session.read_only { (reason_form(if stopped {"resume-all"} else {"stop-all"},if stopped {"Resume all execution"} else {"Stop all execution"},html!{},html!{})) }
            }
        }
    }).await
}
#[derive(Deserialize)]
struct ActionForm {
    #[serde(default)]
    reason: String,
    request_id: Option<String>,
    #[serde(default)]
    confirmed: bool,
    run: Option<String>,
    wait: Option<String>,
    definition: Option<String>,
    note: Option<String>,
    decision: Option<String>,
    payload: Option<String>,
}
fn required(value: Option<String>, name: &str) -> Result<String> {
    value
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ControlError::Invalid(format!("{name} is required")))
}
async fn action(
    State(app): State<App>,
    Path(action): Path<String>,
    Form(f): Form<ActionForm>,
) -> Result<Markup> {
    if app.session.read_only {
        return Err(ControlError::ReadOnly);
    }
    if !f.confirmed {
        return Err(ControlError::Invalid("confirmation is required".into()));
    }
    let id = required(f.request_id, "request_id")?;
    let command = match action.as_str() {
        "cancel" | "cancel-tree" => Command::Cancel {
            run: required(f.run, "run")?,
        },
        "hold" => Command::Hold {
            run: required(f.run, "run")?,
            note: f.note.unwrap_or_default(),
        },
        "release" => Command::Release {
            run: required(f.run, "run")?,
        },
        "pause" => Command::Pause {
            definition: required(f.definition, "definition")?,
            note: f.note.unwrap_or_default(),
        },
        "resume" => Command::Resume {
            definition: required(f.definition, "definition")?,
        },
        "stop-all" => Command::Stop { enabled: true },
        "resume-all" => Command::Stop { enabled: false },
        "respond" => {
            let run = required(f.run, "run")?;
            let wait = required(f.wait, "wait")?;
            // Rebuild the identical event on a retry even if the wait has since closed.
            if let Some(entry) = app.session.client.entry(&id).await?
                && let Command::Respond {
                    run: old_run,
                    wait: old_wait,
                    event,
                } = entry.command
                && old_run == run
                && old_wait == wait
            {
                let data = answer(f.decision.as_deref(), f.payload.as_deref())?;
                if event.get("data") != Some(&data) {
                    return Err(ControlError::Conflict(
                        "request id reused with different answer".into(),
                    ));
                }
                let entry = app
                    .session
                    .execute(Some(id), f.reason, Command::Respond { run, wait, event })
                    .await?;
                return receipt(&app, entry).await;
            }
            let inspect = app
                .session
                .client
                .inspect(&run)
                .await?
                .ok_or_else(|| ControlError::NotFound("run not found".into()))?;
            let w = inspect
                .waits
                .get(&wait)
                .ok_or_else(|| ControlError::Conflict("wait is closed or unknown".into()))?;
            let event_type = w
                .types
                .first()
                .filter(|t| t.as_str() != "*")
                .ok_or_else(|| {
                    ControlError::Invalid(
                        "wildcard waits require a complete CloudEvent via MCP or CLI".into(),
                    )
                })?;
            let mut event = json!({"specversion":"1.0","source":"ouro-console","id":id,"type":event_type,"data":answer(f.decision.as_deref(),f.payload.as_deref())?});
            if let Some(subject) = w.expectations.get("subject") {
                event["subject"] = subject.clone();
            }
            Command::Respond { run, wait, event }
        }
        _ => return Err(ControlError::NotFound("unknown action".into())),
    };
    let entry = app.session.execute(Some(id), f.reason, command).await?;
    receipt(&app, entry).await
}
fn answer(decision: Option<&str>, payload: Option<&str>) -> Result<Value> {
    match decision {
        Some("approve") => Ok(json!({"decision":"approve"})),
        Some("reject") => Ok(json!({"decision":"reject"})),
        Some("json") => serde_json::from_str(payload.unwrap_or(""))
            .map_err(|_| ControlError::Invalid("payload must be valid JSON".into())),
        _ => Err(ControlError::Invalid(
            "choose approve, reject, or json".into(),
        )),
    }
}
async fn receipt(app: &App, entry: Entry) -> Result<Markup> {
    layout(app,"Command reply","The request and its reply are recorded in the operator journal.",html! { section class="card" { div class="card-body" {
        div class="alert alert-success" role="status" { (icon("check")) " Command accepted · " code { (json!(entry.reply)) } }
        dl { dt { "Request ID" } dd { code { (entry.request.id) } } dt { "Actor" } dd { (entry.request.actor) } dt { "Reason" } dd { (entry.request.reason) } }
        pre { (json!(entry.command)) } div class="btn-list" { a class="btn btn-primary" href="/runs" { "Back to runs" } a class="btn" href="/journal" { "View operator journal" } }
    } } }).await
}
