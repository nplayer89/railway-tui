# railway-tui — design

Date: 2026-10-28
Status: draft, awaiting Neil's approval

## Intent

A k9s-style terminal UI, written in Rust, for looking at Railway (railway.com) without using
Railway's website or canvas. Neil finds the dashboard and canvas slow and unpleasant for everyday
work and wants to answer "what's running, what broke, and why" from the keyboard.

The core job of v1 is: **see deployment health at a glance and get from a failing deployment to
the log line that explains it as fast as possible.**

## Goals (v1)

- Show the status of every service in a project/environment, including the health of each
  running replica.
- Get from a failed or crashed deployment to its build or runtime logs in a couple of keystrokes.
- Stream logs live.
- Filter logs to lines matching a string (e.g. `error`) to pinpoint a failure.
- Reuse the existing `railway` CLI login — zero setup for Neil.

## Non-goals (v1)

- **Any action that changes Railway state.** No restart, redeploy, rollback, scale, delete. v1 is
  look-only. Actions come later.
- Managing environment variables (later).
- Creating services / deploying new repos or images (later).
- A cross-project overview. Neil has a handful of projects and works in one at a time.
- Its own login flow (later — see "Auth" below).

## Anti-goals — failure even if it technically works

Neil's words: the thing that sends him back to the website is "it doesn't work", specifically
**stale statuses or logs that stop streaming.** Therefore:

- The UI must never silently show stale data. Every view shows when its data was last
  refreshed. If a refresh fails, the view says so visibly (and keeps showing the old data, marked
  stale) rather than failing quietly.
- A log stream always shows its connection state (connected / reconnecting / disconnected). If the
  stream drops, the UI says so prominently and reconnects automatically, without losing or
  duplicating lines the user has already seen.
- It must not break Neil's `railway` CLI login (see the token-refresh hazard below).

## Constraints

- Rust.
- Railway public GraphQL API only: `https://backboard.railway.com/graphql/v2` and
  `wss://backboard.railway.com/graphql/v2`. Do not use `/graphql/internal`.
- **Rate limits** (docs.railway.com/reference/public-api): Hobby 1,000 req/hour and 10 req/s;
  Pro 10,000/hour and 50/s; Free 100/hour. Fast polling is ruled out: refreshing every 5s alone
  would use ~720 req/hour. 429 responses carry `Retry-After` and `X-RateLimit-*` headers and must
  be honoured and surfaced in the UI. It is unknown whether websocket subscriptions count against
  the limit.
- **Plan detection (Neil's decision): don't hard-code a plan.** The tool works out its budget at
  runtime and adapts, with overrides in a config file. *(My call on mechanism:)* read the hourly
  limit from the `X-RateLimit-Limit` / `RateLimit-Policy` headers on the first API response, so no
  extra request is needed. Derive the backstop refresh interval from it so background refreshes
  use at most ~10% of the hourly budget. Until headers are seen, assume the Free limit (most
  conservative). Keep adjusting from `X-RateLimit-Remaining`.
- **Config file (Neil's decision: a `.conf` file with overrides).** *(My calls:)* location
  `~/.config/railway-tui/railway-tui.conf` (respecting `XDG_CONFIG_HOME`), TOML syntax, every key
  optional, a missing file is fine. Overrides at minimum: hourly request limit, backstop refresh
  interval, default workspace/project/environment. The tool's own remembered state (last project,
  etc.) is kept separately from this hand-edited file.
- **Railway access during development (Neil's decision): read-only.** The builder may make
  read-only calls (queries and subscriptions) against Neil's real Railway account to test and to
  measure the rate-limit unknowns. **No mutations, ever**, which matches the look-only v1. The
  app's own API layer should not contain any mutation operations. Token refresh still goes only
  through the CLI (see Auth).
- Should later become a standalone tool not dependent on the Railway CLI being installed — so the
  auth source must sit behind a seam that can be swapped (see Auth).

## The experience (decided with Neil)

### Navigation

Layout is k9s-style: **one full-screen table at a time**, drill down with Enter, back with Esc.
A header shows the current workspace, project and environment plus key hints; a breadcrumb at the
bottom shows where you are; `:` opens a command line for jumping straight to a view
(`:projects`, `:services`, `:deploys`, `:logs`).

Views:

1. **Projects** — Neil's projects grouped by workspace. Enter opens the project's services.
   *(My call: on launch, reopen the last-used project/environment; fall back to this view.)*
2. **Services** — the main screen. For the current environment, one row per service:

   | column | content |
   | --- | --- |
   | SERVICE | service name |
   | INSTANCES | one coloured box `■` per replica of the current deployment |
   | READY | running replicas / desired replicas, e.g. `1/2` |
   | STATUS | overall status |
   | DEPLOYED | relative time of the latest deployment |
   | COMMIT | short SHA, or `-` for image-based services |
   | MESSAGE | commit message, or image name |

   The **whole row is coloured** by overall status (like k9s). Instance box colours:
   green = RUNNING; red = CRASHED / EXITED; yellow = CREATED / INITIALIZING / RESTARTING;
   grey = STOPPED / REMOVING / REMOVED / SKIPPED. Row colour: green = healthy, red = FAILED /
   CRASHED / any replica down, yellow = BUILDING / DEPLOYING / QUEUED / WAITING / INITIALIZING /
   partially ready, grey = REMOVED / SKIPPED / SLEEPING. `e` switches environment.
   Mockup approved with Neil: `.superpowers/brainstorm/*/content/layout-v3.html`.
   *(My call: the STATUS column shows Railway's deployment status, except it shows DEGRADED when
   the deployment is SUCCESS but fewer replicas are running than desired.)*

   **Latest deploy failed while an older deploy is still serving (Neil's decision).** Railway keeps
   the previous deployment running when a new one fails. That row gets a **red-and-green striped
   status marker**: green because the service is still up, red because the newest deploy is broken.
   The STATUS text reads e.g. `FAILED (prev running)`. In a terminal the stripe can be drawn with
   alternating red/green cells or a half-block glyph with red foreground on green background. The
   exact glyphs are the builder's choice, but they must read as "both" at a glance and stay
   distinct from plain red and plain green.
   *(My calls for this case:)* the INSTANCES boxes and READY count describe the replicas that
   are actually running, i.e. the active deployment(s). `l` opens the **failed** deployment's logs,
   because that is the thing to investigate. The same applies while a new deploy is building or
   deploying alongside a running one: boxes show what's running, STATUS shows the in-progress
   deploy in yellow.
3. **Deployments** — deploy history for the selected service in this environment (status,
   time, commit, message), coloured the same way. Lets Neil open logs for an older failed deploy,
   not only the latest. *(My call; Neil agreed on playback.)*
   **Neil's decision:** like Railway's web UI, when drilling into a service whose latest deploy
   failed while an older one still runs, **both are shown together and clearly labelled**: the
   failed latest deployment and the one actually serving (marked ACTIVE / running). Neither
   is buried in the history list. History continues below them.
4. **Logs** — full-screen, live-streaming, follow mode on by default (scrolling up pauses follow;
   a key resumes it). Toggle between **build** and **runtime** logs; *(my call)* open on build logs
   when the deployment failed during build, otherwise runtime logs.
   **Filter**: `/` then a string; **only matching lines are shown** (Neil's choice — not
   highlight-in-place). Clearing the filter restores all lines.

5. **Instances** *(my call, following the approved mockup's `i` hint)* — `i` on a service lists
   the replicas of its deployment: instance id and status, coloured like the boxes. No region or
   uptime column (the API doesn't expose them per instance, despite the mockup). Enter opens
   runtime logs filtered to that replica if Railway's log attributes identify the replica;
   otherwise it opens the deployment's runtime logs unfiltered and says so.

### Keys

*(My call, Neil agreed on playback.)* k9s/vim style: `j`/`k` and arrows to move, Enter to drill
in, Esc to go back, `/` to filter (works in tables too), `:` command line, `e` environment,
`?` help overlay listing all keys, `q` / `:q` / Ctrl-C to quit. From Services, `l` jumps straight
to the logs of that service's latest deployment (shown in the approved mockup), so a failure is
two keystrokes from the main screen; `i` opens Instances.

### Freshness

Every view shows "updated Ns ago" and, on error, a visible stale/error marker. The log view shows
stream state. See anti-goals.

## Railway API facts this design relies on

Researched from the Railway CLI source (github.com/railwayapp/cli, commit b62467e) and the public
API docs — no live API calls were made. Line numbers are approximate pointers into that commit.

- **Per-replica status exists.** `Deployment.instances { id status }`, enum
  `DeploymentInstanceStatus`: CRASHED, CREATED, EXITED, INITIALIZING, REMOVED, REMOVING,
  RESTARTING, RUNNING, SKIPPED, STOPPED. Instances have no region field.
- **One query gets the services screen**: `environment(id, projectId) { serviceInstances {
  numReplicas region latestDeployment { status instances { id status } } activeDeployments { id
  status instances { id status } } } }` (CLI: `src/gql/queries/strings/EnvironmentInstances.graphql`).
  Desired replicas = `numReplicas` (multi-region totals: CLI `src/controllers/regions.rs`
  `total_replicas`).
- `DeploymentStatus`: BUILDING, CRASHED, DEPLOYING, FAILED, INITIALIZING, NEEDS_APPROVAL, QUEUED,
  REMOVED, REMOVING, SKIPPED, SLEEPING, SUCCESS, WAITING.
- Deploy history: `deployments(input: { projectId, environmentId, serviceId }, first, after)`.
- Live deployment status: `subscription deployment(id)`; also `deploymentEvents`.
- Hierarchy: `me { workspaces { id name projects ... } }` (+ `externalWorkspaces`),
  `projects(workspaceId)`, `environments(projectId)`. "Teams" are deprecated in favour of
  workspaces — use workspace queries only.
- **Runtime logs**: `query deploymentLogs(deploymentId, limit, filter, startDate, endDate)` and
  `subscription deploymentLogs(deploymentId, filter, limit)` → `Log { timestamp message
  attributes { key value } }`.
- **Build logs**: the CLI does *not* use `buildLogs`; it uses `environmentLogs` with filter
  `(@snapshot:<snapshotId> OR @replica:<snapshotId>)`, snapshotId from `deployment(id)` (CLI
  `src/controllers/deployment.rs`). Comment there says this matches the dashboard. Follow the CLI.
- Log streaming reference implementation: CLI `stream_deploy_logs` (limit 500, de-dupes by
  timestamp, retries) in `src/controllers/deployment.rs`.
- **Websocket**: subprotocol `graphql-transport-ws`; auth goes in **HTTP headers on the upgrade
  request** (`authorization: Bearer <token>`), not in `connection_init` (CLI `src/subscription.rs`).
- Schema: CLI ships `src/gql/schema.json` (introspection JSON) and generates types with
  `graphql_client`. The CLI also uses `reqwest`, `graphql-ws-client`, `reqwest-websocket`,
  `tokio`, `ratatui`, `crossterm`, `chrono`.

## Auth

v1 reads the Railway CLI's login. Facts (CLI `src/config.rs`, `src/client.rs`, `src/oauth.rs`):

- File `~/.railway/config.json` (`config-staging.json` / `config-dev.json` when `RAILWAY_ENV` is
  staging/dev). Shape: `{ user: { token, accessToken, refreshToken, tokenExpiresAt }, ... }`.
- Token precedence used by the CLI: `RAILWAY_API_TOKEN` env → `user.accessToken` → legacy
  `user.token`; all sent as `Authorization: Bearer`. (`RAILWAY_TOKEN` is a project token sent as
  `Project-Access-Token`; it can't list workspaces, so v1 ignores it.)
- **Hazard:** the access token is OAuth, expires, and is refreshed with a rotating refresh token.
  If two processes refresh at once Railway detects reuse and **revokes the whole login**. The CLI
  takes a lock while refreshing.

**Decision (my call): the TUI never refreshes the CLI's token itself.** When the token is expired
or about to expire (the CLI treats "within 60s" as expired), the TUI runs a cheap CLI command
(e.g. `railway whoami`) so the CLI refreshes under its own lock, then re-reads the config file. A
401 triggers the same path once. If the CLI isn't installed or refresh fails, show a clear
message ("run `railway login`") rather than a generic error. This avoids reimplementing the CLI's
lock protocol and makes breaking Neil's login impossible from our side.

**Seam:** all token access goes through one small interface (e.g. `TokenSource` with "get a valid
token" and "token was rejected"), with a `RailwayCliTokenSource` implementation and
`RAILWAY_API_TOKEN` honoured as an override. The future standalone tool adds its own
implementation (own OAuth login, own token store) without touching the rest of the app.

## Data freshness strategy (my call)

- **Services view**: one `EnvironmentInstances`-style query on entering the view, then a
  subscription per visible service's latest deployment for live status, plus a slow backstop
  re-query (~every 60s ≈ 60 req/hour) to catch new deployments and replica changes the
  subscriptions don't cover. The interval comes from the detected or configured budget (see
  Constraints), not a fixed 60s. Manual refresh key (`Ctrl-R`) for an immediate re-query.
- Only the current view fetches/subscribes; leaving a view drops its subscriptions.
- Track request spend from `X-RateLimit-Remaining`; on 429 back off per `Retry-After` and show it.
- **Logs**: on open, fetch recent history by query, then subscribe; de-dupe across the seam and
  across reconnects (by timestamp + message, as the CLI does). Reconnect with backoff, showing
  state throughout.
- Whether this budget holds up in practice is unknown until we see whether subscriptions are
  metered; the builder should log request counts during development.

## Left to the builder

- Crate choices and versions (the CLI's set above is a sensible starting point;
  `graphql_client` + vendored `schema.json` is the obvious route for types).
- Internal architecture (app state / event loop / async data layer split), testing approach.
- Exact layout details, colours within the scheme above, wording of messages, help overlay.
- Where the remembered state (last project etc.) lives, and its format.
- Whether the log filter is applied client-side, server-side (Railway's `filter` argument), or
  both. Client-side matching is required at minimum so filtering is instant on already-loaded
  lines.
- How deep the deployment history pages go.

## Open questions

None blocking. To be learned during the build: whether subscriptions count against the rate
limit, and whether `deployment(id)` subscriptions report replica-level changes.
