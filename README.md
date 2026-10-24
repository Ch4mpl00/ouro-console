# ouro-console

An operations console for [ouro](https://github.com/Ch4mpl00/ouro): inspect runs,
answer pending decisions, and stop work through a web UI, MCP tools, or scripts.
Rust 2024, one binary, no Node build and no CDN requests at runtime.

**This scaffold uses an in-memory fake.** It has no ouro crate dependencies and
never opens a runtime store. Each process starts with fresh demo data. Web and
MCP share that process's client; a separate CLI invocation gets a fresh fake,
so a script command does **not** affect a running demo server. The fake does not
execute workflows or simulate worker heartbeats. Actions change its state and
operator journal until that process exits.

## Run

Requires Rust 1.90 or newer. With the binary on your PATH:

```sh
ouro-console serve --fake
# http://127.0.0.1:8090

ouro-console serve --fake --read-only
ouro-console serve --fake --actor operator \
  --trace-url 'https://traces.example/trace/{trace_id}'
```

For a checkout, `cargo run -- serve --fake` runs the same binary. There are no
asset generation steps. The default backend is the fake; `--fake` makes that
choice explicit. A theme button switches between light and dark, starts from
the system preference, and remembers the selection locally.

The demo includes `review`, `transform`, `archive`, and `cleanup` definitions,
every run state, a three-level owned tree, open decision/signal waits, an
automatic breaker trip, a manual pause, and healthy/stale heartbeat snapshots.

## Web UI

| Route | Contents |
| --- | --- |
| `/runs` | Definition/state/age filters, admission or terminal history, cursor paging, 2-second htmx refresh, cancel links |
| `/runs/{id}` | State/version, lease holder and expiry, exact waits, call journal with delivery/states/arguments/results, jump budget, owned descendants including ended children, mailbox, configured trace link, cancel/cancel tree |
| `/pending` | What runs wait for, matcher/progress/correlation expectations, since time, absolute timeout, approve/reject or JSON data answers |
| `/definitions` | Versions, active version, epoch, fault count, automatic breaker/manual pause attribution, schedule, pause/resume |
| `/workers` | Heartbeat age, stale after 30 seconds, RSS, CPU, runs held |
| `/journal` | Request ID, actor, reason, command, committed reply; optional `?run=…` |
| `/emergency` | Always accessible from the red header control; current stop flag, confirmed stop/resume with required reason |

Every web mutation includes a request ID, a required reason and explicit
confirmation, and returns an attributed receipt. Repeating an unchanged request
returns its original reply; conflicting reuse is refused. Read-only mode hides
controls and rejects POST actions on the server. The emergency-stop flag stays
visible and refreshes every two seconds in all page headers.

Cancel and cancel-tree both cancel the run and transitive **owned** descendants,
including across terminal children, atomically. Independent runs stay separate.
Terminal evidence and pending-call delivery semantics are retained. A manual
pause stops new admissions/schedules while existing work finishes; resume
preserves epoch and fault count. An automatic breaker trip requires activation
of a version in the host, outside this console. Emergency stop is a store-wide
execution gate, not cancellation; clearing it resumes from retained evidence.
The fake models the flag, with no worker/executor to enforce dispatch boundaries.

Runs accept `definition`, `state`, `parent`, `since_ms` (inclusive), `before_ms`
(exclusive), `age_minutes`, `history`, `limit` (1–1000), and `cursor`. History
uses end time and a bounded interval; the UI defaults to the last day. Relative
age is resolved once and pinned across polls/pages. Pending waits accept
`definition`, `run`, `event_type`, the same absolute time bounds, `limit`, and
`cursor`. Journal accepts `run`, `limit`, and `cursor`. A page can be empty with
a continuation: keep following `next` until absent. Counts are per-page, never
an unbounded aggregate.

## Authentication and attribution

Loopback binding is the default. A non-loopback bind is refused without a token.
If a token is configured, **all** HTTP routes (including assets, POST actions and
MCP) require `Authorization: Bearer …`, even on loopback. Tokens are never placed
in HTML, URLs, or logs. Foreign origins are rejected; unauthenticated loopback
requests also require a loopback Host header. Serve behind TLS when deployed
outside the local machine. There is no user database or browser login in v1.
For browser access to a protected deployment, use a trusted authenticated proxy
that injects the bearer header on every request, including asset requests.

Use a token file with restrictive permissions (keep it outside this repo):

```json
{"token":"replace-with-a-long-random-secret","actor":"on-call"}
```

```sh
ouro-console serve --fake --bind 0.0.0.0:8090 --token-file /secure/console-token.json
# Alternatively set OURO_CONSOLE_TOKEN in the environment.
```

`--token-file` / `OURO_CONSOLE_TOKEN_FILE` takes precedence over
`OURO_CONSOLE_TOKEN`. `--actor` / `OURO_CONSOLE_ACTOR` takes precedence over the
file's actor; otherwise the actor is `local-operator`.
`--read-only`, `--fake`, `--token-file`, `--actor`, and `--trace-url` are global
flags and may appear after the subcommand. `OURO_CONSOLE_TRACE_URL` is also
supported. Trace templates must be HTTP(S) and contain `{trace_id}`; identifiers
are encoded before substitution.

## MCP

Streamable HTTP runs on the same listener at `/mcp`, using the official Rust
SDK, `rmcp`. It is stateless at the transport level and shares the application's
client. Set the bearer authorization header in your MCP client's transport
configuration when a token is configured.

Read tools: `list_runs`, `inspect_run`, `run_log`, `list_pending`,
`list_definitions`, `list_workers`.

Mutating tools: `respond`, `cancel_run`, `cancel_tree`, `pause_definition`,
`resume_definition`, `stop_all`, `resume_all`. All require a nonempty `reason`;
provide `request_id` for reliable retries. They return the request, command,
timestamp, and reply as JSON. Read-only mode registers only the six reads and
refuses attempts to call writes. Inputs have generated JSON Schemas.

`list_runs` accepts the run filters above (absolute milliseconds), `history`,
`cursor`, and `limit`; history requires both time bounds. `inspect_run` and
`run_log` take `run`. `list_pending` accepts the wait filters, cursor and limit.
Cancel tools take `run`; definition tools take `definition` and optional `note`.
`respond` takes `run`, `wait`, and a complete matching CloudEvent as `event`.
For example:

```json
{
  "run":"run-003", "wait":"/decision", "reason":"reviewed the evidence",
  "request_id":"review-42",
  "event":{
    "specversion":"1.0", "source":"operator", "id":"answer-42",
    "type":"review.decision", "data":{"decision":"approve"}
  }
}
```

The fake validates routing types, projected top-level correlation expectations,
timeouts, closed waits, redacted waits and event identity conflicts. It models
single-position demo waits, not ouro's expression matcher or incremental
multi-filter consumption. The real adapter must delegate matching and response
identity handling to ouro. The web form creates a CloudEvent using the wait's
type, subject and request ID, with approve/reject or custom JSON as `data`.
Wildcard/complex events can be sent through MCP or CLI.

## CLI

Script commands emit JSON to stdout, diagnostics to stderr, and a nonzero exit
status on error. They dispatch through the same async `ControlClient` and
read-only policy as the web and MCP surfaces. **Each invocation currently starts
a fresh fake**; store/remote connectivity is a later step.

```sh
ouro-console runs --state running --definition review --limit 50
ouro-console history --since-ms 0 --before-ms 1800000000000
ouro-console inspect run-002
ouro-console run-log run-002
ouro-console pending --event-type review.decision
ouro-console definitions
ouro-console workers
ouro-console journal --run run-002
ouro-console counts --definition review --limit 50
ouro-console status
ouro-console cancel-run run-002 --reason 'investigate' --request-id cancel-42
ouro-console cancel-tree run-002 --reason 'stop owned work'
ouro-console pause-definition review --reason 'maintenance' --note 'check upstream'
ouro-console resume-definition cleanup --reason 'maintenance complete'
ouro-console stop-all --reason 'contain unexpected execution'
ouro-console resume-all --reason 'incident resolved'
ouro-console respond run-003 /decision --reason 'reviewed' --event \
  '{"source":"operator","id":"answer-42","type":"review.decision","data":{"decision":"approve"}}'
```

Runs/history also support parent, absolute time bounds, and cursor. Pending
supports definition, run, event type, time bounds, and cursor. Counts supports
`--history`, `--since-ms`, `--before-ms`, and cursor. Mutations all accept
`--request-id` and require `--reason`. `--help` on each command lists its options.

## Structure and stack

- `src/client.rs`: console-owned DTOs, object-safe async `ControlClient`, shared
  authorization/attribution `Session`, errors.
- `src/fake.rs`: process-local fixtures, atomic state changes, query-bound
  continuations, audit and response identity deduplication.
- `src/web.rs`: axum routes/middleware, Maud pages/forms, embedded assets.
- `src/mcp.rs`: rmcp tool schemas/handlers and read-only tool registration.
- `src/cli.rs`: clap parsing, token configuration, composition and script dispatch.
- `assets/`: small theme/style helpers; `assets/vendor/` contains pinned Tabler,
  Tabler icons and htmx distributions, licenses, provenance and checksums.
- `tests/`: page/action/auth tests, HTTP MCP tests, client semantics and CLI tests.

Axum + Tokio serve HTTP and MCP. Maud provides typed Rust templates with default
HTML escaping and no external template compiler/build pipeline; this keeps the
server-rendered v1 small and refactorable. htmx handles only the live fragments.
Tabler provides the admin UI, light/dark variables, JavaScript and icons.
`rust-embed` includes all assets in the binary (including debug/test builds);
there are no runtime filesystem asset dependencies.

## Real client roadmap

ouro's store/runtime seams are moving to async (ouro#7). The console deliberately
has no dependency on that work's intermediate signatures.

Implement `ControlClient` in a future `OuroControl` adapter over the finalized
async `ouro_runtime::control::Control`, with injected store and clock. Map all
console-owned DTOs to/from ouro's types and keep real cursors opaque and
query-bound. Delegate cancellation, pause/resume, stop enforcement, command
idempotency/audit, and exact persisted wait matching to the control service.
Preserve error classification and original request IDs/replies across ambiguous
retries. Expand the wait projection for complex matchers without interpreting
ouro expressions in the console.

Definitions and worker heartbeat/resource catalogs need a host/store adapter;
workers/telemetry are adjacent to ouro#20, not methods currently supplied by
`Control`. Trace IDs are host-pinned and must be passed through. Add persistent
store or remote connection configuration so server and scripts address the same
backend, integration tests against ouro's adapters, and deployment auth/TLS
examples. Version activation, metrics dashboards, Grafana/Tempo compose examples,
workflow editing and application-specific views are outside v1.

## Checks

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

No console license has been selected yet. Third-party vendored license notices
are included under `assets/vendor/`; see its inventory for versions and sources.
