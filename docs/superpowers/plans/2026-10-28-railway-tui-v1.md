# railway-tui v1 Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A look-only, k9s-style Rust TUI that shows Railway service and replica health and streams build/runtime logs, using the `railway` CLI's login.

**Architecture:** Elm-style core. `App::update(AppEvent) -> Vec<Command>` is pure and holds all UI state. A tokio worker runs each `Command` against Railway's public GraphQL API (HTTP plus `graphql-transport-ws`) and sends results back as `AppEvent::Data`. Rendering is a pure function of `App`. Health derivation, the rate budget, the log buffer and auth are separate pure-ish modules with their own tests.

**Tech Stack:** Rust 1.96, ratatui 0.30 (use its `ratatui::crossterm` re-export so the crossterm version cannot drift), tokio, reqwest 0.12 (rustls), graphql_client 0.14 with a vendored schema, graphql-ws-client 0.11 plus reqwest-websocket 0.5, serde/serde_json, toml, chrono, dirs, thiserror. Dev: wiremock, tempfile.

**Spec:** `docs/superpowers/specs/2026-10-28-railway-tui-design.md`. Read it first. Its "Railway API facts" section names the queries, and the Railway CLI source (github.com/railwayapp/cli @ b62467e) is the reference implementation for queries, log streaming (`src/controllers/deployment.rs`) and websockets (`src/subscription.rs`).

## Global Constraints

- Public API only: `https://backboard.railway.com/graphql/v2` and `wss://backboard.railway.com/graphql/v2`. Never `/graphql/internal`.
- **No GraphQL mutations anywhere in the codebase**, as a test enforces (Task 1). v1 is look-only.
- The TUI never writes `~/.railway/*` and never refreshes the OAuth token itself. Refresh only by running `railway whoami` (Task 4).
- Websocket subprotocol `graphql-transport-ws`, with auth in the HTTP upgrade headers as `authorization: Bearer <token>`.
- Never show stale data silently. Every data view shows "updated Ns ago", or a visible stale/error marker. The logs view always shows stream state.
- Rate budget: background refreshes use at most ~10% of the hourly limit. Assume 100/hour until a response says otherwise. Config overrides win.
- Config file `$XDG_CONFIG_HOME/railway-tui/railway-tui.conf` (default `~/.config/...`), TOML, every key optional, a missing file is fine.
- Development may make read-only live calls against Neil's Railway account. Never mutations.
- Commit messages end with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Railway adds a new status value.** An unrecognised deployment or instance status renders as a grey `UNKNOWN(<raw>)` and never panics or fails decoding. Tests: Task 2, Task 6.
2. **A log socket that stays open but goes dead.** Keepalive failure or a stream error must show `RECONNECTING`, never a quiet "connected". Test: Task 8.
3. **Late responses for something the user has left.** A services response for the previous environment, or log lines from a closed log session, must be dropped and not shown. Test: Task 9.
4. **A malformed `.conf`** (bad TOML, unknown key, wrong type). This is a clear error naming the file and the problem, printed before the TUI starts, with exit code 1. Test: Task 3.
5. **Not logged in, CLI missing, or token expired with a refresh that fails.** Print a clear "run `railway login`" style message, write no files and don't panic. Tests: Task 4 and Task 11.

---

## File Structure

```
Cargo.toml
graphql/schema.json            vendored from railwayapp/cli (MIT); SCHEMA_SOURCE.md records origin
graphql/*.graphql              one file per operation (queries + subscriptions only)
src/main.rs                    startup, terminal setup/teardown, event loop
src/model.rs                   domain types (statuses, deployments, logs)
src/status.rs                  row health derivation, box colours, deployment row ordering
src/config.rs                  .conf loading
src/remember.rs                remembered last project/environment
src/auth.rs                    TokenSource trait, CLI + env implementations
src/budget.rs                  rate-limit parsing and backstop interval
src/api/mod.rs                 Api (HTTP GraphQL), ApiError, mapping to model
src/api/subscribe.rs           websocket connection + subscription streams
src/logs/buffer.rs             LogBuffer (dedupe, order, cap, filter)
src/logs/follow.rs             reconnecting log follower + StreamState
src/app/mod.rs                 App, View, AppEvent, DataEvent, Command, update()
src/app/keys.rs                key handling per view
src/ui/mod.rs                  draw() + header/breadcrumb/freshness
src/ui/tables.rs               projects/services/deployments/instances tables
src/ui/logs.rs                 logs view
src/worker.rs                  executes Commands, emits DataEvents
tests/no_mutations.rs          guard test
tests/live_readonly.rs         #[ignore] live smoke checks (Task 12)
```

---

### Task 1: Crate scaffold, vendored schema, mutation guard

**Files:**
- Create: `Cargo.toml`, `src/main.rs` (stub), `graphql/schema.json`, `graphql/SCHEMA_SOURCE.md`, `tests/no_mutations.rs`

**Interfaces:**
- Produces: crate `railway_tui` with lib + bin (`src/lib.rs` declares modules; `src/main.rs` uses the lib), so integration tests can import modules.

- [ ] **Step 1:** Run `cargo init --name railway-tui`, add `src/lib.rs`, and add all the Tech Stack dependencies to `Cargo.toml` at their latest compatible versions.
- [ ] **Step 2:** Confirm the CLI repo licence is MIT (`gh api repos/railwayapp/cli/license --jq .license.spdx_id`, which should print `MIT`). Then download `https://raw.githubusercontent.com/railwayapp/cli/b62467e/src/gql/schema.json` to `graphql/schema.json`, and record the URL, commit and licence in `SCHEMA_SOURCE.md`.
- [ ] **Step 3: Write the guard test** `tests/no_mutations.rs::graphql_files_contain_no_mutations`. It reads every `graphql/*.graphql` file and asserts that none matches the regex `(?m)^\s*mutation\b`. A second test, `no_graphql_query_derive_outside_graphql_dir`, scans `src/**/*.rs` and asserts that every `query_path = "..."` starts with `graphql/`. Either use a small hand-written scan or add `regex` as a dev-dependency.
- [ ] **Step 4:** Run `cargo test --test no_mutations`. Expected: PASS. There are no `.graphql` files yet, so it passes trivially; the test is there to guard the files later tasks add.
- [ ] **Step 5:** Run `cargo build`. Expected: compiles.
- [ ] **Step 6: Commit** with `feat: scaffold crate with vendored Railway schema and mutation guard`.

### Task 2: Domain model and health derivation

**Files:**
- Create: `src/model.rs`, `src/status.rs`

**Interfaces:**
- Produces (`model.rs`):
  - `enum DeploymentStatus { Building, Crashed, Deploying, Failed, Initializing, NeedsApproval, Queued, Removed, Removing, Skipped, Sleeping, Success, Waiting, Unknown(String) }`, with `fn from_api(&str) -> Self` and `fn label(&self) -> String`. The label is the API spelling; Unknown shows as `UNKNOWN(<raw>)`.
  - `enum InstanceStatus { Crashed, Created, Exited, Initializing, Removed, Removing, Restarting, Running, Skipped, Stopped, Unknown(String) }`, with `from_api`.
  - `struct Instance { id: String, status: InstanceStatus }`
  - `struct DeploymentSummary { id, status: DeploymentStatus, created_at: DateTime<Utc>, commit_sha: Option<String>, commit_message: Option<String>, image: Option<String>, instances: Vec<Instance> }`
  - `struct ServiceInstance { service_id, service_name, num_replicas: u32, latest: Option<DeploymentSummary>, active: Vec<DeploymentSummary> }`
  - `struct Workspace { id, name, projects: Vec<ProjectRef> }`, `struct ProjectRef { id, name }`, `struct Environment { id, name }`
  - `struct LogLine { timestamp: DateTime<Utc>, message: String, attributes: Vec<(String, String)> }`
  - `enum LogKind { Build, Runtime }`
- Produces (`status.rs`):
  - `enum Health { Healthy, Degraded, Failing, FailedButServing, Progressing, Inactive }`
  - `struct RowStatus { health: Health, label: String, boxes: Vec<InstanceStatus>, ready: u32, desired: u32 }`
  - `fn derive_row(svc: &ServiceInstance) -> RowStatus`
  - `enum BoxColour { Green, Red, Yellow, Grey }`, with `fn box_colour(s: &InstanceStatus) -> BoxColour`
  - `fn default_log_kind(d: &DeploymentSummary) -> LogKind`
  - `enum RowTag { Latest, Active }`, `struct DeploymentRow { deployment: DeploymentSummary, tag: Option<RowTag> }`, and `fn deployment_rows(svc: &ServiceInstance, history: &[DeploymentSummary]) -> Vec<DeploymentRow>`

Rules for `derive_row`, in order (the spec's Services section):
- `boxes` are the instances of all `active` deployments. If `active` is empty, use `latest`'s instances.
- `ready` is the number of boxes that are `Running`.
- `desired` is `max(num_replicas, 1, ready)`.
- If `latest` is None: `Inactive`, with label `NO DEPLOYS`.
- If `latest` is Failed or Crashed, and some active deployment with a different id has at least one Running instance: `FailedButServing`, with label `FAILED (prev running)` (`CRASHED (prev running)` for Crashed).
- Otherwise, if `latest` is Failed or Crashed: `Failing`, with the status label.
- If `latest` is Building, Deploying, Queued, Waiting, Initializing or NeedsApproval: `Progressing`, with the status label.
- If `latest` is Removed, Removing, Skipped, Sleeping or Unknown: `Inactive`, with the status label.
- If `latest` is Success and `ready < desired`: `Degraded`, with label `DEGRADED`.
- If `latest` is Success otherwise: `Healthy`, with label `SUCCESS`.

`box_colour` follows the spec: Running is Green; Crashed and Exited are Red; Created, Initializing and Restarting are Yellow; everything else, including Unknown, is Grey.

`default_log_kind` *(plan's call, refining the spec's "build when it failed during build")*: Failed or Building gives Build; everything else gives Runtime. Railway uses CRASHED for runtime failures and FAILED for build or healthcheck failures.

`deployment_rows` ordering: put `latest` first, tagged `Latest`, if it isn't also active. Next come the active deployments, tagged `Active`. Then the rest of `history` in its original order, untagged, with no duplicates by id.

- [ ] **Step 1: Write failing tests** in `src/status.rs` `mod tests`, building fixtures with a small `deploy(id, status, &[instance statuses])` helper:
  - `latest_failed_prev_running_is_striped_case`: latest `d2` Failed with no instances, active `[d1 Success, [Running, Running]]`, `num_replicas` 2. Expect health `FailedButServing`, label `"FAILED (prev running)"`, boxes `[Running, Running]`, `ready == 2`, `desired == 2`.
  - `latest_failed_nothing_serving_is_failing`: health `Failing`, label `"FAILED"`.
  - `success_with_crashed_replica_is_degraded`: active is latest, with instances `[Running, Crashed]` and 2 replicas. Expect `Degraded`, `"DEGRADED"`, `ready == 1`, `desired == 2`.
  - `building_while_old_runs_is_progressing_and_boxes_show_running`: latest Building, active d1 `[Running]`. Expect `Progressing`, boxes `[Running]`.
  - `no_deployments_is_inactive`: label `"NO DEPLOYS"`, `desired == 1`.
  - `unknown_status_is_inactive_and_labelled`: `DeploymentStatus::from_api("HIBERNATING")` gives `Unknown("HIBERNATING")`, health `Inactive`, label `"UNKNOWN(HIBERNATING)"`. Also `InstanceStatus::from_api("WEIRD")` gives a Grey box. **(Review Focus 1)**
  - `deployment_rows_pin_latest_then_active_then_history`: latest d3 Failed, active d2, history `[d3, d2, d1]`. Ids come out `[d3, d2, d1]` with tags `[Latest, Active, None]`.
  - `default_log_kind_rules`: Failed and Building give Build; Crashed and Success give Runtime.
- [ ] **Step 2:** Run `cargo test status::`. Expected: FAIL (does not compile; the types are missing).
- [ ] **Step 3:** Implement the model types and the `status.rs` functions above.
- [ ] **Step 4:** Run `cargo test status::`. Expected: PASS.
- [ ] **Step 5: Commit** with `feat: domain model and service health derivation`.

### Task 3: Config file and remembered state

**Files:**
- Create: `src/config.rs`, `src/remember.rs`

**Interfaces:**
- Produces:
  - `#[serde(deny_unknown_fields)] struct Config { rate_limit_per_hour: Option<u32>, refresh_interval_secs: Option<u64>, default_workspace: Option<String>, default_project: Option<String>, default_environment: Option<String> }`, deriving `Default`. Defaults match a workspace, project or environment by id, or by name case-insensitively.
  - `fn config_path(xdg_config_home: Option<&Path>, home: &Path) -> PathBuf` returns `<xdg or home/.config>/railway-tui/railway-tui.conf`.
  - `fn load_config(path: &Path) -> Result<Config, ConfigError>`. A missing file gives `Ok(Config::default())`.
  - `ConfigError` with a `Display` of the form `"<path>: <toml error message>"`.
  - `struct Remembered { last_project_id: Option<String>, last_environment_id: Option<String> }` (serde, Default).
  - `fn remembered_path(xdg_state_home: Option<&Path>, home: &Path) -> PathBuf` returns `<xdg or home/.local/state>/railway-tui/state.toml`.
  - `fn load_remembered(path) -> Remembered` never fails: a missing or corrupt file gives Default.
  - `fn save_remembered(path, &Remembered) -> io::Result<()>` creates the directories, writes a temp file and renames it into place.

- [ ] **Step 1: Write failing tests** (with tempfile):
  - `missing_config_is_default`.
  - `full_config_parses`: `rate_limit_per_hour = 1000`, `refresh_interval_secs = 30`, `default_project = "my-api"`.
  - `unknown_key_errors_naming_file_and_key`: the file contains `rate_limit = 5`. The error string contains the file path and `rate_limit`. **(Review Focus 4)**
  - `wrong_type_errors`: `rate_limit_per_hour = "lots"` gives `Err`, and the message contains the path.
  - `config_path_respects_xdg` and `config_path_defaults_to_home_dot_config`.
  - `remembered_roundtrip` and `corrupt_remembered_is_default`.
- [ ] **Step 2:** Run `cargo test config:: remember::`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test config:: remember::`. Expected: PASS.
- [ ] **Step 5: Commit** with `feat: config file and remembered state`.

### Task 4: Auth — token sources

**Files:**
- Create: `src/auth.rs`

**Interfaces:**
- Produces:
  - `trait TokenSource: Send + Sync { fn token(&self) -> Result<String, AuthError>; fn refresh_after_rejection(&self) -> Result<String, AuthError>; }`
  - `struct EnvTokenSource(String)`, for `RAILWAY_API_TOKEN`. Its refresh returns `AuthError::Rejected`.
  - `trait CliRunner: Send + Sync { fn run_whoami(&self) -> Result<(), AuthError>; }`, plus `struct RealCliRunner`. That runs `railway whoami` with the inherited env and a 15s timeout, ignores stdout, and maps "not found" to `CliMissing`.
  - `struct CliTokenSource<R: CliRunner> { config_path: PathBuf, runner: R, now: Box<dyn Fn() -> i64 + Send + Sync>, refresh_lock: Mutex<()> }`
  - `fn cli_config_path(railway_env: Option<&str>, home: &Path) -> PathBuf`: `staging` gives `config-staging.json`; `dev` or `develop` gives `config-dev.json`; anything else gives `config.json`, all under `home/.railway/`.
  - `fn token_source_from_env() -> Result<Arc<dyn TokenSource>, AuthError>`: if `RAILWAY_API_TOKEN` is set and non-empty, use `EnvTokenSource`; otherwise `CliTokenSource<RealCliRunner>`.
  - `enum AuthError { NotLoggedIn { path: PathBuf }, CliMissing, RefreshFailed(String), Rejected, Io(String) }`, whose `Display` texts tell the user what to do (e.g. `"Not logged in to Railway (no token in <path>). Run \`railway login\`."`).

`CliTokenSource::token()` reads the JSON on every call and does not cache it. It uses `user.accessToken` if present. If `user.tokenExpiresAt` is ≤ `now() + 60`, it takes `refresh_lock`, calls `runner.run_whoami()`, re-reads the file, and returns `RefreshFailed` if the token is still expired. Without an `accessToken`, it uses the legacy `user.token`, which has no expiry. If neither is present it returns `NotLoggedIn`. `refresh_after_rejection()` takes the lock, runs whoami once, and re-reads. The lock means concurrent 401s cause a single CLI run.

- [ ] **Step 1: Write failing tests** with a fake `CliRunner` that counts calls and can rewrite the temp config file to simulate the CLI refreshing it:
  - `uses_access_token_when_fresh`: no runner calls.
  - `expired_token_runs_whoami_then_rereads`: the fake rewrites the expiry to the future and the new token is returned. One runner call.
  - `within_60s_counts_as_expired`.
  - `still_expired_after_whoami_is_refresh_failed`.
  - `legacy_token_used_when_no_access_token`.
  - `missing_file_or_user_is_not_logged_in`: the message contains `railway login`. **(Review Focus 5)**
  - `cli_missing_surfaces`: the fake returns `CliMissing` and `token()` returns `Err(CliMissing)`.
  - `never_writes_config`: the file's bytes are unchanged after `token()` and `refresh_after_rejection()` (with a runner that doesn't write).
  - `cli_config_path_variants`.
- [ ] **Step 2:** Run `cargo test auth::`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test auth::`. Expected: PASS.
- [ ] **Step 5: Commit** with `feat: token sources reusing the railway CLI login`.

### Task 5: Rate budget

**Files:**
- Create: `src/budget.rs`

**Interfaces:**
- Produces:
  - `const DEFAULT_HOURLY_LIMIT: u32 = 100;`
  - `enum LimitSource { Default, Detected, Configured }`
  - `struct RateBudget { .. }`, with:
    - `fn new(configured_limit: Option<u32>) -> Self`
    - `fn observe(&mut self, headers: &reqwest::header::HeaderMap, now: Instant)`
    - `fn hourly_limit(&self) -> (u32, LimitSource)`
    - `fn remaining(&self) -> Option<u32>`
    - `fn blocked_until(&self) -> Option<Instant>`
  - `fn parse_hourly_limit(headers: &HeaderMap) -> Option<u32>`
  - `fn backstop_interval(hourly_limit: u32, configured_secs: Option<u64>) -> Duration`

Rules:
- `parse_hourly_limit` reads `RateLimit-Policy` (entries like `1000;w=3600`, possibly comma-separated) and returns the quota of the entry whose `w=3600`. Without that entry it returns `None`. A bare `X-RateLimit-Limit` is ignored because its window is unknown; Task 12 checks the real headers and adjusts this.
- A configured limit always wins over a detected one.
- `observe` records `X-RateLimit-Remaining`. On `Retry-After` (seconds) it sets `blocked_until`.
- `backstop_interval`: a configured value wins (minimum 1s). Otherwise it is `36000 / limit` seconds, clamped to 15..=600. That gives 360s for 100/hour, 36s for 1000 and 15s for 10000.

- [ ] **Step 1: Write failing tests:**
  - `policy_header_hourly_entry`: `"10;w=1, 1000;w=3600"` gives `Some(1000)`.
  - `no_policy_is_none`.
  - `configured_beats_detected`.
  - `retry_after_sets_blocked_until`.
  - `backstop_intervals`: (100, None) gives 360s; (1000, None) gives 36s; (10000, None) gives 15s; (1000, Some(5)) gives 5s.
  - `default_until_detected`: `new(None).hourly_limit() == (100, Default)`.
- [ ] **Step 2:** Run `cargo test budget::`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test budget::`. Expected: PASS.
- [ ] **Step 5: Commit** with `feat: rate budget and backstop interval`.

### Task 6: HTTP API client

**Files:**
- Create: `src/api/mod.rs`, plus these files under `graphql/`: `Workspaces.graphql`, `Environments.graphql`, `EnvironmentInstances.graphql`, `Deployments.graphql`, `DeploymentSnapshot.graphql`, `DeploymentLogs.graphql`, `EnvironmentLogs.graphql`
- Test: `tests/api_http.rs` (wiremock)

**Interfaces:**
- Consumes: `TokenSource` (Task 4), `RateBudget` (Task 5), model types (Task 2).
- Produces:
  - `struct Api`, with `fn new(endpoint: Url, tokens: Arc<dyn TokenSource>, budget: Arc<Mutex<RateBudget>>) -> Self` and these `async fn`s:
    - `workspaces() -> Result<Vec<Workspace>, ApiError>` (via `me.workspaces`; also include `externalWorkspaces` if the schema has it)
    - `environments(project_id) -> Result<Vec<Environment>, ApiError>` (non-ephemeral only)
    - `service_instances(project_id, env_id) -> Result<Vec<ServiceInstance>, ApiError>`
    - `deployments(project_id, env_id, service_id, first: i64) -> Result<Vec<DeploymentSummary>, ApiError>`, newest first
    - `deployment_logs(deployment_id, since: Option<DateTime<Utc>>, limit: i64) -> Result<Vec<LogLine>, ApiError>`
    - `build_logs(env_id, deployment_id, since, limit) -> Result<Vec<LogLine>, ApiError>`, which looks up `snapshotId` with `DeploymentSnapshot` and then calls `environmentLogs` with the filter `(@snapshot:<id> OR @replica:<id>)`, as the CLI does
  - `fn budget(&self) -> Arc<Mutex<RateBudget>>`
  - `enum ApiError { Auth(AuthError), RateLimited { retry_after: Duration }, Http(u16), GraphQl(Vec<String>), Network(String), Decode(String) }`, implementing `Display`.

Notes:
- Write the queries against `graphql/schema.json`. graphql_client fails to compile if a field is wrong, and that is the check. The spec's API facts give the shapes.
- The commit SHA, message and image come from `Deployment.meta`, which is JSON: read the `commitHash`, `commitMessage` and `image` keys if present.
- Get service names from `serviceInstances { serviceName }` if the schema has that field; otherwise join on `project.services`.
- Map enum values through `from_api` on the raw string. That is why `Other(String)` from graphql_client must reach `Unknown`.
- Every response goes through `budget.observe` before its status is checked.
- On HTTP 401, or a GraphQL error whose message contains "Not Authorized", call `tokens.refresh_after_rejection()` and retry once. If the retry also fails, return `Auth(Rejected)`.
- HTTP 429 returns `RateLimited`, using `Retry-After` or 60s if it is missing.
- `token()` is blocking (it may run a subprocess), so call it via `tokio::task::spawn_blocking`.

- [ ] **Step 1: Write failing tests** in `tests/api_http.rs`. Each mounts a wiremock JSON response:
  - `service_instances_maps_latest_active_and_instances` checks the mapped `ServiceInstance` fields, including `commit_sha` taken from `meta`.
  - `unknown_enum_value_decodes_to_unknown`: the deployment status is `"HIBERNATING"`, and the result is `Ok` with `Unknown("HIBERNATING")`. **(Review Focus 1)**
  - `sends_bearer_token`: a header matcher on `authorization: Bearer tok`.
  - `401_triggers_one_refresh_and_retry`: the first response is 401 and the second 200. The fake token source records exactly one `refresh_after_rejection` call.
  - `persistent_401_is_auth_error`.
  - `429_is_rate_limited_with_retry_after`: `Retry-After: 30` gives 30s, and `budget.blocked_until()` is `Some`.
  - `policy_header_updates_budget`: `RateLimit-Policy: 1000;w=3600` gives `hourly_limit() == (1000, Detected)`.
  - `graphql_errors_surface`: `{"errors":[{"message":"boom"}]}` gives `GraphQl(["boom"])`.
  - `build_logs_uses_snapshot_filter`: a body matcher asserts the second request's variables contain `@snapshot:snap1`.
- [ ] **Step 2:** Run `cargo test --test api_http`. Expected: FAIL.
- [ ] **Step 3:** Write the `.graphql` operations and implement `Api`.
- [ ] **Step 4:** Run `cargo test --test api_http && cargo test --test no_mutations`. Expected: PASS for both.
- [ ] **Step 5: Commit** with `feat: Railway GraphQL HTTP client`.

### Task 7: Log buffer

**Files:**
- Create: `src/logs/mod.rs`, `src/logs/buffer.rs`

**Interfaces:**
- Produces:
  - `const LOG_CAP: usize = 10_000;`
  - `const REPLICA_ATTR_KEYS: &[&str] = &["deploymentInstanceId", "replica"];`. Task 12 confirms the real key.
  - `struct LogBuffer { .. }`, with:
    - `fn new(cap: usize) -> Self`
    - `fn push(&mut self, line: LogLine) -> bool`, which returns false for a duplicate
    - `fn extend(&mut self, lines: impl IntoIterator<Item = LogLine>) -> usize`, which returns how many were added
    - `fn last_timestamp(&self) -> Option<DateTime<Utc>>`
    - `fn len(&self) -> usize`
    - `fn visible(&self, filter: Option<&str>, replica: Option<&str>) -> Vec<&LogLine>`

Rules:
- Dedupe on the key `(timestamp, message)`.
- Lines are kept sorted by timestamp, so a history page that arrives after live lines slots in place.
- At capacity the oldest line is dropped and its key evicted.
- `filter` is a case-insensitive substring match on `message`. Only matching lines are returned, per Neil's decision.
- `replica` keeps only lines whose attributes have one of `REPLICA_ATTR_KEYS` equal to it.

- [ ] **Step 1: Write failing tests:**
  - `duplicates_dropped`
  - `out_of_order_history_inserted_sorted`
  - `cap_drops_oldest_and_allows_its_key_again`
  - `filter_is_case_insensitive_and_only_matching`: lines `"ERROR: x"`, `"ok"` and `"an error"` with filter `"error"` return 2 lines.
  - `replica_filter_uses_attributes`
  - `last_timestamp_is_newest`
- [ ] **Step 2:** Run `cargo test logs::buffer`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test logs::buffer`. Expected: PASS.
- [ ] **Step 5: Commit** with `feat: log buffer with dedupe, ordering and filtering`.

### Task 8: Subscriptions and the reconnecting log follower

**Files:**
- Create: `src/api/subscribe.rs`, `src/logs/follow.rs`, `graphql/DeploymentLogsSub.graphql`, `graphql/EnvironmentLogsSub.graphql`, `graphql/DeploymentStatusSub.graphql`

**Interfaces:**
- Consumes: `Api` (Task 6), `TokenSource`, `LogLine`, `LogKind`.
- Produces:
  - `enum StreamState { Connecting, Live, Reconnecting { attempt: u32, retry_in: Duration }, Failed(String) }`
  - `fn backoff(attempt: u32) -> Duration` gives 1, 2, 4, 8 and 16s, then 30s for every attempt after that.
  - `trait LogSource: Send + Sync`, with:
    - `fn history(&self, since: Option<DateTime<Utc>>) -> impl Future<Output = Result<Vec<LogLine>, ApiError>> + Send`
    - `fn subscribe(&self) -> impl Future<Output = Result<BoxStream<'static, Result<Vec<LogLine>, ApiError>>, ApiError>> + Send`

    These are spelled with `+ Send` futures, not plain `async fn`, so that `follow_logs` can be passed to `tokio::spawn`.
  - `struct RailwayLogSource { api: Arc<Api>, ws: Url, tokens: Arc<dyn TokenSource>, env_id: String, deployment_id: String, kind: LogKind }`, implementing `LogSource`.
  - `enum FollowEvent { Lines(Vec<LogLine>), State(StreamState) }`
  - `async fn follow_logs<S: LogSource>(source: S, out: mpsc::Sender<FollowEvent>)`. It runs until `out` is closed.
  - In `api/subscribe.rs`:
    - `async fn connect(ws: &Url, token: &str) -> Result<graphql_ws_client::Client, ApiError>`. This is a `graphql-transport-ws` connection with `authorization: Bearer` set on the upgrade request. Keepalive pings every 15s; if no pong arrives within 30s, the stream errors.
    - `fn deployment_status_stream(client, deployment_id) -> BoxStream<'static, Result<DeploymentStatus, ApiError>>`

`follow_logs` algorithm:
1. Emit `State(Connecting)`. Load `history(since: last_seen)` and emit the lines.
2. Call `subscribe()`. Emit `State(Live)` and forward each batch as `Lines`.
3. On a stream error, the stream ending, or a subscribe error, emit `State(Reconnecting { attempt, retry_in: backoff(attempt) })`, sleep, then go back to step 1 using the newest timestamp seen so the gap is backfilled. The receiving `LogBuffer` handles the overlap.
4. On `ApiError::Auth(_)`, emit `State(Failed(msg))` and stop.
5. A successful `Live` resets `attempt`.

- [ ] **Step 1: Write failing tests** in `src/logs/follow.rs` with a scripted fake `LogSource`, using `tokio::time::pause` so the backoff is instant:
  - `connect_then_live_forwards_lines`.
  - `stream_error_reconnects_and_backfills_from_last_timestamp`: the fake records the `since` argument of the second `history` call and it equals the last line's timestamp. The states are `[Connecting, Live, Reconnecting{attempt:1,..}, Connecting, Live]`. **(Review Focus 2)**
  - `stream_end_counts_as_drop`: a stream returning `None` gives `Reconnecting`.
  - `auth_error_fails_without_retry`.
  - `backoff_sequence`: 1, 2, 4, 8, 16, 30, 30.
  - `stops_when_receiver_dropped`.
- [ ] **Step 2:** Run `cargo test logs::follow`. Expected: FAIL.
- [ ] **Step 3:** Implement `follow_logs`, `backoff`, `connect`, `RailwayLogSource` and `deployment_status_stream`. Use the CLI's `src/subscription.rs` as the reference for `connect`.
- [ ] **Step 4:** Run `cargo test logs::follow && cargo test --test no_mutations`. Expected: PASS for both.
- [ ] **Step 5: Commit** with `feat: websocket subscriptions and reconnecting log follower`.

### Task 9: App state, events and key handling

**Files:**
- Create: `src/app/mod.rs`, `src/app/keys.rs`

**Interfaces:**
- Consumes: model and status (Task 2), `Config`/`Remembered` (Task 3), `LogBuffer` (Task 7), `StreamState`/`FollowEvent` (Task 8).
- Produces:
  - `struct Loaded<T> { value: Option<T>, fetched_at: Option<Instant>, error: Option<String>, loading: bool }`
  - `enum View { Projects, Services, Deployments { service_id: String }, Instances { service_id: String }, Logs(LogsView) }`
  - `struct LogsView { session: u64, deployment_id: String, service_name: String, kind: LogKind, replica: Option<String>, buffer: LogBuffer, follow: bool, scroll: usize, filter: Option<String>, state: StreamState }`
  - `enum AppEvent { Key(KeyEvent), Tick(Instant), Data(DataEvent) }`
  - `enum DataEvent`, with these variants:
    - `Workspaces(Result<Vec<Workspace>, String>)`
    - `Environments { project_id, result: Result<Vec<Environment>, String> }`
    - `Services { project_id, env_id, result: Result<Vec<ServiceInstance>, String> }`
    - `Deployments { service_id, env_id, result: Result<Vec<DeploymentSummary>, String> }`
    - `StatusChanged { deployment_id, status: DeploymentStatus }`
    - `Log { session: u64, event: FollowEvent }`
    - `Budget { interval: Duration, blocked_until: Option<Instant> }`
    - `LiveUpdates { ok: bool }`
  - `enum Command`, with these variants:
    - `LoadWorkspaces`
    - `LoadEnvironments { project_id }`
    - `LoadServices { project_id, env_id }`
    - `LoadDeployments { project_id, env_id, service_id }`
    - `WatchDeployments { ids: Vec<String> }`, which replaces the previous watch set; an empty set means stop
    - `OpenLogs { session, env_id, deployment_id, kind }`
    - `CloseLogs { session }`
    - `SaveRemembered(Remembered)`
    - `Quit`
  - `struct StartPrefs { workspace: Option<String>, project: Option<String>, environment: Option<String>, remembered: Remembered }`
  - `struct App`. Its fields are the view stack, the current workspace/project/environment, `Loaded` data per view, the per-view selection, the table filter, the command line, the env picker, help, `backstop: Duration` and `live_updates: bool`. It has:
    - `fn new(prefs: StartPrefs) -> (App, Vec<Command>)`, which returns `[LoadWorkspaces]`
    - `fn update(&mut self, ev: AppEvent, now: Instant) -> Vec<Command>`

Behaviour:
- **Start:** when workspaces arrive, resolve the project in this order: `prefs.project` (by id or name, case-insensitive), then `remembered.last_project_id`, then none. If one resolves, push Services and emit `LoadEnvironments`. If not, show Projects with `prefs.workspace` listed first.
- **Choosing the environment:** when environments arrive, pick `prefs.environment`, then remembered, then `production`, then the first. Emit `LoadServices` and `SaveRemembered`.
- **Services keys:** Enter pushes Deployments and emits LoadDeployments. `l` opens logs for the `latest` deployment, with kind `default_log_kind(latest)`. `i` pushes Instances. `e` opens the environment picker; choosing an environment clears services, emits LoadServices and SaveRemembered. `Ctrl-R` emits LoadServices.
- **Deployments:** the rows come from `deployment_rows`. Enter on a row opens its logs.
- **Instances:** Enter opens runtime logs for the deployment that owns the instance, with `replica` set to the instance id.
- **Opening logs:** increment the session counter, push Logs, and emit `OpenLogs`. Leaving the Logs view (Esc) emits `CloseLogs{session}`.
- **Logs view keys:**
  - `b` toggles build/runtime. It closes the old session and opens a new one.
  - Up, `k` or PgUp scroll and set `follow = false`.
  - `G` or `f` set `follow = true`.
  - `/` edits the filter; Enter applies it.
  - Esc clears the filter first, then goes back.
- **Global keys:** j/k and arrows move; Esc goes back; `/` filters the current table by name; `:` opens the command line (`projects`, `services`, `deploys`, `logs`, `q`, `quit`); `?` toggles help; `q` or Ctrl-C quits from any view where no input is active.
- **Watching deployments:** entering Services emits `WatchDeployments` with the latest deployment ids of the listed services. Leaving Services emits `WatchDeployments{ids: []}`.
- **A `StatusChanged` event** patches the matching `latest.status` in place. If the new status is Success, Failed, Crashed or Removed, it also emits `LoadServices` so the instances are picked up.
- **On Tick,** if the current view's data `fetched_at + backstop <= now` and it is not loading or blocked, emit its Load command. This is the backstop refresh.
- **Stale results:** drop `Services`, `Deployments` and `Environments` results whose ids don't match the current context, and `Log` events whose session isn't the open one. **(Review Focus 3)**
- **A load error** keeps the old value and sets `error`. The UI shows it as stale.

- [ ] **Step 1: Write failing tests** in `src/app/mod.rs` `mod tests`, driving `update` with synthetic events:
  - `start_resolves_configured_project_by_name_and_loads_envs`
  - `start_without_prefs_shows_projects`
  - `env_resolution_prefers_production`
  - `l_on_failed_service_opens_build_logs_of_latest`: the commands contain `OpenLogs{kind: Build, deployment_id: latest}`.
  - `esc_from_logs_closes_session`
  - `late_services_for_old_env_ignored`: after switching to env B, a `Services{env_id: A}` event leaves the state unchanged. **(Review Focus 3)**
  - `log_events_for_closed_session_ignored` **(Review Focus 3)**
  - `scroll_up_pauses_follow_and_G_resumes`
  - `backstop_tick_emits_reload_after_interval_only`
  - `load_error_keeps_old_value_and_sets_error`
  - `terminal_status_event_triggers_service_reload`
  - `colon_q_quits`
- [ ] **Step 2:** Run `cargo test app::`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test app::`. Expected: PASS.
- [ ] **Step 5: Commit** with `feat: app state machine and key handling`.

### Task 10: Rendering

**Files:**
- Create: `src/ui/mod.rs`, `src/ui/tables.rs`, `src/ui/logs.rs`

**Interfaces:**
- Consumes: `App` (Task 9), `derive_row` and `box_colour` (Task 2).
- Produces: `fn draw(frame: &mut Frame, app: &App, now: Instant)`.

Layout (approved mockup `layout-v3.html` and the spec's Services table):
- **Header:** workspace, project and environment, key hints, and freshness. Freshness reads `updated {n}s ago`; if there is an error it reads `STALE: {error}`, red; while loading it reads `loading…`. When live updates are off it also shows `live updates off`.
- **Rate limiting:** while `blocked_until` is in the future, the header also shows `rate limited, retrying in {n}s` in yellow.
- **Breadcrumb:** at the bottom.
- **Projects:** grouped by workspace. Each workspace is a non-selectable heading row, with its projects indented beneath it.
- **Services columns:** SERVICE, INSTANCES, READY (`{ready}/{desired}`), STATUS, DEPLOYED (relative), COMMIT (7 chars or `-`), MESSAGE (commit message or image).
- **Row colours:** Healthy is green. Degraded, Failing and FailedButServing are red. Progressing is yellow. Inactive is grey.
- **INSTANCES boxes:** one `■` per box, separated by spaces, each coloured by `box_colour`.
- **FailedButServing:** draw a 2-cell marker `▚▚` (red fg on green bg) before the STATUS text, so it reads differently from plain red. *(Plan's call within the spec's "builder's choice".)*
- **Deployments:** same colouring, with a TAG column (`LATEST`, `ACTIVE`).
- **Instances:** columns ID, STATUS and DEPLOYMENT.
- **Logs:**
  - A title line with the service, deployment, BUILD/RUNTIME and the active filter.
  - A stream-state badge: `● LIVE` green; `CONNECTING…` yellow; `RECONNECTING (attempt n, retry in Ns)` with a yellow background across the full width; `DISCONNECTED: msg` with a red background across the full width.
  - A `FOLLOW`/`PAUSED` badge.
  - Lines prefixed `HH:MM:SS`.
- **Overlays:** help, the environment picker and the command line.

- [ ] **Step 1: Write failing tests** with `ratatui::backend::TestBackend` (100×30), asserting on buffer cells:
  - `services_row_shows_coloured_boxes`: the cells under INSTANCES for `[Running, Crashed]` have fg Green then Red.
  - `failed_but_serving_has_striped_marker`: a `▚` cell with fg Red and bg Green.
  - `freshness_shows_seconds_ago`: `now - fetched_at = 5s` renders `updated 5s ago`.
  - `error_shows_stale_marker`
  - `reconnecting_banner_visible`: the text contains `RECONNECTING`. **(Review Focus 2)**
  - `logs_show_only_filtered_lines`
- [ ] **Step 2:** Run `cargo test ui::`. Expected: FAIL.
- [ ] **Step 3:** Implement.
- [ ] **Step 4:** Run `cargo test ui::`. Expected: PASS.
- [ ] **Step 5: Commit** with `feat: render views, freshness and stream state`.

### Task 11: Worker and main wiring

**Files:**
- Create: `src/worker.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Consumes: everything above.
- Produces:
  - `async fn run_worker(api: Arc<Api>, ws: Url, tokens: Arc<dyn TokenSource>, remembered_path: PathBuf, configured_interval: Option<u64>, mut cmds: mpsc::Receiver<Command>, events: mpsc::Sender<AppEvent>)`
  - `fn spawn_follower<S: LogSource + 'static>(session: u64, source: S, events: mpsc::Sender<AppEvent>) -> JoinHandle<()>` runs `follow_logs` and wraps each `FollowEvent` as `DataEvent::Log { session, .. }`. The worker keeps a `HashMap<u64, JoinHandle<()>>` and aborts the handle on `CloseLogs`.
  - `main`

Worker:
- Each Load command runs in its own task and sends its `DataEvent`, with errors mapped to strings via `Display`.
- After each request, send `DataEvent::Budget { interval: backstop_interval(limit, configured_interval), blocked_until }`.
- `OpenLogs` spawns `follow_logs` and stores its `JoinHandle` by session. Its `FollowEvent`s are forwarded as `DataEvent::Log`.
- `CloseLogs` aborts that session's task.
- `WatchDeployments` keeps one websocket client. It aborts the previous watch tasks and spawns one `deployment_status_stream` per id. If the connection fails, it sends `LiveUpdates{ok:false}` and the backstop keeps working.
- `SaveRemembered` writes the file through `spawn_blocking`.

Main:
1. Load the config. On error, print it to stderr and exit 1.
2. Build the token source and call `token()` once before touching the terminal. On error, print the AuthError and exit 1. **(Review Focus 5)**
3. Load the remembered state.
4. Set up the terminal: raw mode, alternate screen, and a panic hook that restores the terminal.
5. Run a `tokio::select!` loop over crossterm `EventStream`, a 250ms tick and worker events. It calls `app.update`, sends commands to the worker and draws.
6. Restore the terminal on quit.

- [ ] **Step 1: Write a failing test** `worker::tests::aborted_follower_sends_nothing_more`. Call `spawn_follower` with a fake `LogSource` that emits a line every 100ms, under paused tokio time. Receive one `Log` event, abort the handle, and advance time by 1s. Assert that no further events arrive.
- [ ] **Step 2:** Run `cargo test worker::`. Expected: FAIL.
- [ ] **Step 3:** Implement the worker and main.
- [ ] **Step 4:** Run `cargo test` (the whole suite). Expected: PASS. Then run `cargo clippy --all-targets -- -D warnings`. Expected: no warnings.
- [ ] **Step 5:** Run `HOME=/nonexistent cargo run`. Expected: it prints the not-logged-in message mentioning `railway login`, exits 1, and leaves the terminal intact.
- [ ] **Step 6: Commit** with `feat: worker and terminal main loop`.

### Task 12: Live read-only verification and README

**Files:**
- Create: `tests/live_readonly.rs`
- Modify: `src/budget.rs` and `src/logs/buffer.rs` (only if the findings require it), `README.md`, and the spec's "Open questions"

This task touches Neil's real Railway account, **read-only**, which the spec permits. It must not run any mutation, since the Task 1 guard covers the code.

- [ ] **Step 1:** Write `#[ignore]` tests that run only when `RAILWAY_TUI_LIVE=1`. Each prints its findings:
  - `live_rate_limit_headers`: call `workspaces()` once and print all `ratelimit`-related response headers. Assert that `parse_hourly_limit` returns `Some`.
  - `live_subscription_metering`:
    - Read `X-RateLimit-Remaining`.
    - Hold a `deploymentLogs` subscription open for 60s on the newest deployment of the first service in the first project.
    - Read `Remaining` again and print the difference, minus the number of HTTP requests made in between.
  - `live_log_attribute_keys`: fetch 50 runtime log lines and print the set of attribute keys.
- [ ] **Step 2:** Run `RAILWAY_TUI_LIVE=1 cargo test --test live_readonly -- --ignored --nocapture`. Record the output.
- [ ] **Step 3:** Apply the findings:
  - If the hourly limit arrives in a different header shape, fix `parse_hourly_limit` and add a unit test with the real header value.
  - Set `REPLICA_ATTR_KEYS` to the real key, with a test.
  - Write all three findings into the spec's "Open questions" as answered.
- [ ] **Step 4:** Run `cargo run`. Open a project, check the boxes match Railway, open the logs with `l`, filter with `/error`, and toggle with `b`. Note anything that differs from the spec.
- [ ] **Step 5:** Write a README: what the tool is, install (`cargo install --path .`), the requirement to be logged in with `railway login`, a keys table, and the config keys with an example `.conf`.
- [ ] **Step 6:** Run `cargo test && cargo clippy --all-targets -- -D warnings`. Expected: PASS.
- [ ] **Step 7: Commit** with `test: live read-only checks; docs: README`.
