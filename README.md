# ouro-console

An operations console for [ouro](https://github.com/Ch4mpl00/ouro): inspect runs,
answer pending decisions, and stop work through a web UI, MCP tools, or scripts.
Rust 2024, one binary, no Node build and no CDN requests at runtime.

`OuroControl` connects directly to ouro's async control API over a SQLite or
Postgres store. Web, MCP, and separate CLI processes address the same persistent
state when configured with the same store. The console observes and controls
runs; the embedding ouro host still admits and drives workflows.

`ouro-runtime` and `ouro-durable` are pinned to the Git tag `v0.1.0`. `ows` types come through
`ouro-runtime`'s reexport. `--fake` remains available for process-local demos.

## Run

Requires Rust 1.90 or newer. With the binary on your PATH:

```sh
ouro-console serve --store sqlite:/var/lib/ouro/runs.sqlite
# http://127.0.0.1:8090

export OURO_STORE='sqlite:/var/lib/ouro/runs.sqlite'
ouro-console runs --limit 50
ouro-console serve --read-only

# The prefix selects the adapter; the remainder is the database URL.
ouro-console serve --store 'postgres:postgresql://operator@localhost/ouro'

# Demos use fresh in-memory data for each process.
unset OURO_STORE
ouro-console serve --fake
ouro-console serve --fake --read-only
ouro-console serve --fake --actor operator \
  --trace-url 'https://traces.example/trace/{trace_id}'
```

Choose `--store` (or `OURO_STORE`) or `--fake`; there is no implicit backend.
`--store` overrides `OURO_STORE`; `--fake` conflicts with a selected store
(including the environment). Backend status messages do not print connection URLs.
For a checkout, `cargo run -- serve --store sqlite:/path/to/runs.sqlite` runs
the same binary. There are no asset generation steps. A theme button switches
between light and dark, starts from the system preference, and remembers the
selection locally.

Writable connections use ouro's own SQLite/Postgres constructors, including
schema initialization/migration. Back up a store before opening it with a new
ouro version. The pinned Postgres adapter uses `NoTls`; use a local database or
secure tunnel. Database connections are separate from HTTP bearer protection.

### Read-only deployment

`--read-only` hides web actions, rejects HTTP/CLI commands, and exposes only MCP
reads. `OuroControl` also refuses writes at its boundary. Read-only connections
use ouro's own constructors, `sqlite::open_read_only` and
`postgres::connect_read_only`: they never create, migrate or write, SQLite
opens with `query_only` and deferred transactions, and Postgres uses serializable
`READ ONLY` transactions. A database without the schema, or with another schema
version, is refused with ouro's typed `SchemaError`; the console reports it as
`ControlError::Unavailable` (HTTP 503 on pages, a message on the CLI and MCP).
Open the store writable once, with the host's role, to migrate it.

Use a **read-only database user** for real protection. For Postgres, grant only
CONNECT, schema USAGE, and SELECT on `ouro_rows` and `ouro_migrations`, with no
CREATE or data modification privileges; `default_transaction_read_only=on` is
an additional safeguard. Provision the schema using the host's writable role
first. For SQLite, restrict the console account's filesystem access and keep
WAL/SHM files accessible for live reads. Console mode flags are not a substitute
for database permissions or deployment authentication.

The demo includes `review`, `transform`, `archive`, and `cleanup` definitions,
every run state, a three-level owned tree, open decision/signal waits, an
automatic breaker trip, a manual pause, annotated calls with token counts, and
live/stale worker heartbeat rows. Demo actions last until that process exits;
a separate demo CLI process does not affect a demo server. The fake executes no
workflows or heartbeat updates.

## Web UI

| Route | Contents |
| --- | --- |
| `/runs` | Definition/state/age filters, admission or terminal history, cursor paging, 2-second htmx refresh, a tokens column from the runs' own totals with the page's sum, cancel links |
| `/runs/{id}` | State/version, lease holder and expiry, exact waits, usage (the run's annotation totals, `gen_ai.usage.input_tokens`/`output_tokens` first), call journal with delivery/states/annotations/arguments/results, jump budget, owned descendants including ended children, mailbox, pinned trace/span ids with the configured trace link, cancel/cancel tree |
| `/pending` | What runs wait for, matcher/progress/correlation expectations, since time, absolute timeout, approve/reject or JSON data answers |
| `/definitions` | Paged: versions with their latest activation time, actor and note, active version, epoch, fault count, automatic breaker/manual pause attribution and time, each schedule kind with next due time, overlap, catch-up and queued slot, pause/resume |
| `/definitions/{name}` | Versions with activation attribution, and the summed annotations (tokens, cost) of the runs that ended in a window: last hour, 24 hours (default), 7 or 30 days |
| `/workers` | Paged heartbeat rows: live/stale, last beat and expiry, runs held, application version, host, start |
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
`--store`, `--read-only`, `--fake`, `--token-file`, `--actor`, and `--trace-url`
are global flags and may appear after the subcommand. `OURO_CONSOLE_TRACE_URL` is also
supported. Trace templates must be HTTP(S) and contain `{trace_id}`; identifiers
are encoded before substitution.

## MCP

Streamable HTTP runs on the same listener at `/mcp`, using the official Rust
SDK, `rmcp`. It is stateless at the transport level and shares the application's
client. Set the bearer authorization header in your MCP client's transport
configuration when a token is configured.

Read tools: `list_runs`, `inspect_run`, `run_log`, `list_pending`,
`list_definitions`, `get_definition`, `get_definition_totals`, `list_workers`.
`list_definitions` and `list_workers` take `cursor` and `limit`; `get_definition`
takes `definition`. `get_definition_totals` takes `definition` and an optional
window, `since_ms` and `before_ms` (default: the 24 hours before now), and returns
the summed annotations (tokens, cost) of the runs that ended in it.

Mutating tools: `respond`, `cancel_run`, `cancel_tree`, `pause_definition`,
`resume_definition`, `stop_all`, `resume_all`. All require a nonempty `reason`;
provide `request_id` for reliable retries. They return the request, command,
timestamp, and reply as JSON. Read-only mode registers only the eight reads and
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
multi-filter consumption. The real adapter delegates matching and response
identity handling to ouro, including incremental multi-event consumption. The
web form creates a CloudEvent using the wait's
type, subject and request ID, with approve/reject or custom JSON as `data`.
Wildcard/complex events can be sent through MCP or CLI.

## CLI

Script commands emit JSON to stdout, diagnostics to stderr, and a nonzero exit
status on error. They dispatch through the same async `ControlClient` and
read-only policy as the web and MCP surfaces. Set `OURO_STORE` or pass `--store`
to every invocation. Use run IDs and exact wait keys returned by `pending`;
the sample IDs below illustrate the demo data.

```sh
export OURO_STORE='sqlite:/var/lib/ouro/runs.sqlite'
ouro-console runs --state running --definition review --limit 50
ouro-console history --since-ms 0 --before-ms 1800000000000
ouro-console inspect run-002
ouro-console run-log run-002
ouro-console pending --event-type review.decision
ouro-console definitions --limit 50
ouro-console definition review
ouro-console definition-totals review --since-ms 1800000000000 --before-ms 1800086400000
ouro-console workers --limit 50
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
- `src/ouro.rs`: adapter from ouro's `Control` to the console's types, opaque cursor transport.
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

## Runtime mapping and remaining gaps

The console keeps its own view types. Numeric ouro version IDs are displayed as
strings (`"none"` for an inactive definition). All six run states, lease
ownership/expiry, call arguments/results and delivery states, mailbox metadata,
owned descendants, budget evidence, per-call annotations and the run's
annotation totals, and the trace/span IDs pinned at admission are projected
from `Control::inspect`. No trace IDs or worker records are invented.
Annotations are shown as ouro stores them: the GenAI token counts
(`gen_ai.usage.input_tokens`, `gen_ai.usage.output_tokens`) are displayed
first, every other name as a key/value. `list` and `history` return each run's
totals (ouro keeps them on the run row), so the runs list has a tokens column
and its footer sums the page; the run page shows the same totals. Wait
positions remain strings; strategies, matcher filters and incremental progress
are displayed without evaluating expressions. Complex or wildcard responses
can be submitted as complete CloudEvents through MCP/CLI. A response becomes
workflow progress when an ouro host drives the run again.

Cursors are URL-safe base64 wrappers around ouro's serialized opaque cursor.
They are passed back unchanged to ouro, which enforces query binding. The
console delegates transactional cancellation, pause/resume, emergency stop,
response matching and identity, and attributed idempotent receipts to `Control`.
It does not retry ambiguous writes. Failures are ouro's typed `ControlError`,
mapped by variant and never by message: not found, a reused request or response
identity, an invalid state, a closed wait and the emergency stop become
`NotFound` / `Conflict` (404 / 409); a malformed request or a response that
fails the wait's matcher is `Invalid` (400); read-only is `ReadOnly` (403); a
`SchemaError` inside a storage failure is `Unavailable` (503); any other
storage failure stays `Internal` (500) with its cause chain, including
Postgres's database diagnosis.

Definitions come from `Control::definitions` (bounded pages in binary name
order, each page one transaction) and `Control::definition`. The console reads
no store rows and knows no key layout. Each version shows its latest activation
time and, when the host attributed it, who activated it and the note. A
definition's page (`/definitions/<name>`) shows the versions and
`Control::totals`: the summed annotations of the runs that ended in a window
(last hour, 24 hours by default, 7 or 30 days), in a plain table. The same
totals are in MCP (`get_definition_totals`) and the CLI
(`definition-totals <name> [--since-ms N] [--before-ms N]`).
Workers come from `Control::workers`: one heartbeat row per worker, stale when
it did not beat again before its expiry by ouro's clock. Process memory and
CPU are not in the rows; ouro leaves them to an outside process exporter.
Version activation, workflow editing, metrics dashboards and host-specific views
remain outside v1. The console does not run a workflow worker. Store-only
cancellation changes durable state; the host observes it at its execution gates.

### Developing against an ouro checkout (no `[patch]`)

For local experiments, temporarily replace just these two dependency lines in
`Cargo.toml` with paths to a sibling checkout:

```toml
ouro-runtime = { path = "../ouro/crates/runtime" }
ouro-durable = { path = "../ouro/crates/durable" }
```

`ows` follows the runtime's own local dependency. Keep both declarations from
the same checkout. Cargo updates `Cargo.lock` for those sources when running
checks; restore the Git declarations and lockfile before committing console
changes. No `[patch]` section or modification to the ouro checkout is needed.

## Checks

```sh
export CARGO_HOME=/private/tmp/ouro-cargo
# If libgit2 cannot negotiate HTTPS in your environment:
export CARGO_NET_GIT_FETCH_WITH_CLI=true
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```

The existing page/action, MCP transport and CLI dispatch suites run against
both `FakeControl` and `OuroControl` on temporary SQLite files. Real fixtures
use ouro's `Runtime` to save/activate definitions, admit and drive workflows
(one function answers annotated output with GenAI token counts; two runs are
admitted inside a known span context), create owned children, pause a
definition, cancel a run, and write heartbeat rows from two runtimes, one of
them stale. A persisted lease provides a running view. Tests also cover independent-process CLI selection,
opaque paging, typed error mapping, read-only refusal of an unmigrated or newer
store, run and definition totals, activation attribution, and HTTP actions
observed through ouro's API, including driving a responded wait to completion.

Postgres variants are ignored by default. With a disposable test database whose
role can create/drop schemas, run the same cases plus the real-store contract:

```sh
TEST_DATABASE_URL='postgresql://tester@localhost/ouro_test' \
  cargo test postgres -- --ignored
```

Each fixture creates a random schema and successful tests remove their schemas;
a failing test may leave its `console_test_…` schema for inspection. Existing
schemas and runtime data are never truncated.

No console license has been selected yet. Third-party vendored license notices
are included under `assets/vendor/`; see its inventory for versions and sources.
