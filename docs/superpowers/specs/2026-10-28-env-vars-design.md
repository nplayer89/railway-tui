# railway-tui — editing environment variables

Status: draft for Neil's review. Builds on `2026-10-28-railway-tui-design.md` (v1, read-only).

## Intent

Railway's web UI makes it hard to find the variable you're looking for. Neil wants to open a
service's variables in railway-tui, type to find the one he wants, change it (or add or delete
variables), review everything pending, and deploy — one service at a time.

This is the first feature that writes to Railway. Until now the app has been strictly read-only.

## Goals

- A Variables view for one service in the current environment, with filter-as-you-type (the same `/`
  behaviour as the Services view).
- Change a value, add a variable, delete a variable.
- Edits are held as **pending changes** in railway-tui, saved on disk, so quitting loses nothing.
  Nothing reaches Railway until Neil deploys.
- A clean **review screen** listing pending changes grouped by service, showing old and new values.
- **Deploy per service**: send that service's pending changes to Railway, then redeploy that
  service only.
- Discard a service's pending changes.

## Non-goals

- Railway's own staged changes. Neil chose app-held pending changes so each service can be deployed
  on its own (option 2 in the brainstorm). Pending edits made here do not show in the web UI.
- Shared (project-level) variables, and copying variables between environments.
- Deploying several services in one action.
- Any other write: no restarts, rollbacks, deploy triggers, or service settings.

## Anti-goals — failure even if it technically works

- **Changing the wrong environment**, especially production by accident.
- A secret appearing on screen without Neil asking for it.
- A partly-applied deploy that looks finished, or pending edits lost silently.
- Any write to Railway that Neil did not confirm.

## The experience (decided with Neil)

### Variables view

- Opened from the Services view for the selected service (key: my call, `v`; also `:vars`).
- Each row is one variable: name, then the value hidden as `••••••`.
- **`/` searches the list live** (Neil): every keystroke narrows the list straight away, and
  variables that don't match disappear. It is the same behaviour as the Services view: matching
  ignores case and finds the text anywhere in the name, Enter keeps the search, Esc while typing
  cancels it, and Esc afterwards clears it. Pending additions are searched too. The search stays
  applied when the list reloads from Railway. It matches names only, never values, so a search
  cannot reveal what a hidden value contains.
- **Values stay hidden until Neil presses a key to reveal them.** My call: `r` reveals the selected
  value, `R` reveals all; everything is hidden again when the view is left.
- Pending edits show in the list with a marker: changed, added, or marked for deletion. A deleted
  variable stays in the list, struck through, until it is deployed.
- The environment name is always visible and prominent in this view (my call: in the view's title
  line, in the theme's `bad` colour when the environment looks like production — its name is
  `production` or contains `prod`, case-insensitively).

My calls on keys in the Variables view: `enter`/`e` edit value, `a` add, `d` mark/unmark for
deletion, `u` undo the pending change on the selected row, `p` open the review screen. Editing is
an inline one-line editor in the footer, like the filter. A value containing a newline (certificates,
keys) cannot be edited inline; `E` opens it in `$EDITOR` (any value can use `E`).

### Review screen

- Opened with `p` from the Variables or Services view, or `:pending`.
- Lists pending changes for **every environment of the open project**, the current environment
  first, grouped by environment and then by service (my call: edits made in staging must not
  disappear from view after switching to production). Deploying from a group always targets
  that group's environment.
- While anything is pending in the project, the header shows a count, e.g. `3 pending`.
- Each change is shown as:
  `+ NAME` added, `~ NAME` changed, `- NAME` deleted, each with old → new value.
- The review screen names the environment the changes will go to.
- Values are hidden here too until revealed with `r` / `R`.
- On a service group: `D` deploys that service, `x` discards its pending changes (with a yes/no).

### Confirm before every deploy

Neil asked for a simple "are you sure?" showing the diff. My call: it appears before **every**
deploy, not only production, since a deploy cannot be undone from the app. The prompt shows the
environment name (in `bad` colour for production), the service, and the full diff for that service,
with values hidden unless already revealed. `y` deploys, anything else cancels.

### Deploying a service

1. Re-read the service's current variables from Railway.
2. If a variable Neil edited has changed on Railway since he edited it, the confirm lists it as
   "changed on Railway since your edit" (my call: warn, don't block).
3. On `y`, in this order (my call: the variables are never left with deletions applied but
   additions missing, whatever `variableDelete` turns out to do):
   - upsert all added and changed variables in one `variableCollectionUpsert`
     (`replace: false`, `skipDeploys: true`);
   - delete each deleted variable with `variableDelete`, one at a time;
   - then `serviceInstanceRedeploy(environmentId, serviceId)`.
4. Progress is shown step by step. On success the service's pending changes are cleared and the
   view reloads.

### Partial failure and retries (my calls)

- **Applied state is tracked per variable.** Each pending entry records whether its write has
  reached Railway. After each successful write step, the entries it covered are marked applied and
  the pending file is saved before the next step.
- **A failed write stops the deploy.** The redeploy never runs after a failed variable write. The
  review screen shows the service as "partly written": applied entries greyed with a tick, the rest
  still pending, plus the error.
- **Retrying a deploy** sends only the entries not yet applied, then redeploys. A retried delete
  that Railway answers with "not found" counts as success.
- **Variables written, redeploy failed**: every entry is applied, and the service is marked
  "written, not deployed". Retrying runs only the redeploy.
- **Writes are never resent automatically.** The read path's resend-once after a 401 does not apply
  to mutations. A 401 or "Not Authorized" means the write was refused, so it is safe to refresh the
  token and offer the retry to Neil.
- **Unknown outcome** (timeout or connection lost after sending): for a variable write, the app
  re-reads the service's variables and marks each entry applied or not from what is actually on
  Railway. For the redeploy, it does not resend. It shows "redeploy outcome unknown — check the
  deployments view", and Neil can retry it by hand.

### Pending changes on disk

- Stored under the XDG state dir next to the remembered state (my call:
  `railway-tui/pending.toml`), keyed by environment id then service id.
- Each entry records the project id, the action, the new value, the value it had when edited (for
  the "changed on Railway" check), and whether it has been applied.
- Written with the same temp-file-and-rename approach as the remembered state, file mode `0600`
  because it holds secret values.
- Pending changes for an environment or service that no longer exists are shown in the review
  screen as orphaned and can only be discarded.

## Railway API facts this design relies on

From the vendored schema (`graphql/schema.json`):

- Read: `variables(projectId, environmentId, serviceId, unrendered: true)` returns a name→value map.
  `unrendered: true` keeps references such as `${{Postgres.DATABASE_URL}}` as written, so Neil edits
  what he wrote, not the resolved value.
- `variableCollectionUpsert(input: {projectId, environmentId, serviceId, variables, replace,
  skipDeploys})`.
- `variableDelete(input: {projectId, environmentId, serviceId, name})` — has **no** `skipDeploys`.
- `serviceInstanceRedeploy(environmentId, serviceId)`.

## Constraints

- **Only these three mutations exist in the codebase**: `variableCollectionUpsert`,
  `variableDelete`, `serviceInstanceRedeploy`. `tests/no_mutations.rs` becomes an allowlist that
  fails on any other mutation.
- Mutations are sent from one module, and only after a `y` on the confirm prompt.
- No automated test ever writes to Railway: every write path is tested against a mock server
  (wiremock). The live tests stay read-only. **Neil runs the first real writes himself.**
- Values never appear in logs, error messages or panics; only names do.
- Writes count against the same hourly request budget as reads.

## Left to the builder

- Layout details of the Variables and review screens within the existing theme and table style.
- How the confirm prompt scrolls when a diff is longer than the screen.

## Open questions (to settle during build or Neil's first live run)

- Does `variableDelete` trigger a deploy by itself? It has no `skipDeploys`. Only Neil's first live
  run can answer this, and the build does not wait for it. With the order above, the worst case is
  an extra deploy that already carries the new values. If that happens, it is accepted for now and
  noted for a follow-up.
- Railway's "sealed" variables cannot be read back. How they appear in `variables` is unknown; until
  known, a variable whose value comes back empty is shown as-is.
- Whether Railway-provided variables (`RAILWAY_*`) come back from `variables(unrendered: true)`; if
  they do, they should be read-only in the view.
