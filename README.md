# railway-tui

A k9s-style terminal UI for looking at [Railway](https://railway.com) from the keyboard: see every
service's deployment health at a glance, and get from a failed or crashed deployment to the log
line that explains it in a couple of keystrokes.

Everything is read-only except two things: you can edit, add and delete a service's variables and
deploy those changes (see [Variables](#variables)), and you can deploy the latest commit of a
service's connected branch (`D`), or switch the service to another branch and deploy that (`d`),
both on Services or Deployments and confirmed with `y`. It never restarts, scales or otherwise
changes anything else. It uses only Railway's public GraphQL API.

Views: Projects, Services (one row per service with a coloured box per replica), Deployments
(history of one service), Instances (the replicas of a deployment) and Logs (live build or
runtime logs with a filter), Variables (one service's variables) and Pending (review and deploy
your unsent variable changes).

## Install

Requires a Rust toolchain.

```sh
cargo install --path .
```

## Log in

railway-tui reuses the [`railway` CLI](https://docs.railway.com/guides/cli) login, so log in with
the CLI first:

```sh
railway login
```

railway-tui reads the CLI's `~/.railway/config.json` but never writes it. When the token has
expired it runs `railway whoami`, which makes the CLI refresh its own login. `RAILWAY_ENV`
(`staging`, `dev`) picks the CLI's matching config file, as it does for the CLI.

Alternatively, set `RAILWAY_API_TOKEN` to an account API token; it takes precedence
over the CLI login.

Then run:

```sh
railway-tui
```

On launch it reopens the last-used project and environment.

## Keys

| key | action |
| --- | --- |
| `j` / `k`, arrows | move / scroll |
| `enter` | open the selected item |
| `esc` | back, or clear the filter |
| `/` | filter the current table or log |
| `:` | command line: `:projects`, `:services`, `:deploys`, `:logs`, `:vars`, `:pending`, `:q` |
| `e` | environment picker (Services) |
| `l` | logs of the service's latest deployment (Services) |
| `i` | instances of the service's deployment (Services) |
| `v` | variables of the selected service (Services; also `:vars`) |
| `p` | review pending variable changes (Services and Variables; also `:pending`) |
| `D` | deploy the latest commit of the service's branch (Services and Deployments): a confirmation shows the environment, service, repo, branch and the commit running now; `y` deploys, any other key cancels. Variable changes already written to Railway ship with it. Refused for a service that runs an image |
| `d` | pick a branch to deploy (Services and Deployments): type to filter the repo's branches, arrows to move, `enter` to pick, `esc` to close. Picking a branch other than the connected one switches the service's deploy trigger to it before deploying, and the service **stays on that branch** (pushes to it deploy if auto-deploy is on); pick the old branch again to go back. The confirm shows `old → new`. Branches are listed with the GitHub CLI (`gh api`, your GitHub login), falling back to Railway, which refuses `railway login` tokens; if neither can list them, type the exact branch name |
| `b` | switch between build and runtime logs (Logs) |
| `f`, `G` | resume following the log (Logs) |
| `PageUp` / `PageDown` | scroll the log by a page (Logs) |
| `r`, `ctrl-r` | reload now instead of waiting for the next refresh (in the log view: reconnect the stream; in Variables and the review `r` reveals, so use `ctrl-r`) |
| `t` | theme picker (also `:theme`): moving previews, `enter` keeps and remembers, `esc` reverts |
| `?` | toggle the help overlay |
| `q`, `ctrl-c` | quit (refused while a deploy is in flight; `ctrl-c` twice forces it) |

## Variables

From Services press `v` (or `:vars`) to open the selected service's variables in the current
environment. Values are hidden until revealed and are hidden again when you leave the view.

| key | action |
| --- | --- |
| `/` | live search by variable name only (never values) |
| `e` / `enter` | edit the selected value inline |
| `E` | edit in `$VISUAL`, then `$EDITOR`, then `vi` |
| `a` | add a variable |
| `d` | mark the selected variable for deletion (toggle) |
| `u` | undo the pending change to the selected variable |
| `r` / `R` | reveal the selected value / all values |
| `y` | copy the selected value to the clipboard (OSC 52) |
| `Y` | copy the selected variable as a `NAME=VALUE` line |
| `v` | start (or end) a visual selection of rows; move with `j` / `k` |
| `p` | open the review |

While a visual selection is active (the title shows `-- VISUAL --`):

| key | action |
| --- | --- |
| `y` / `Y` | copy the selected values / `NAME=VALUE` lines, in table order (sealed rows are skipped) |
| `d` | mark every selected variable for deletion (never unmarks) |
| `r` | reveal every selected value, or hide them all if all are already revealed |
| `v` / `esc` | end the selection (`esc` does not leave the view) |

`a`, `e`, `E`, `enter` and `u` do nothing while a selection is active. Opening the filter (`/`) ends
it. Sealed variables show `<Opaque>` and are never copied; empty ones show `<Empty>`.

Edits are not sent to Railway immediately. They are kept as pending changes and shown in the view.

### Review and deploy

`p` (or `:pending`) lists the pending changes in the open project, grouped by environment, then service, with the
current environment first. `D` deploys the selected group: a confirmation shows the diff, and any variable
that "changed on Railway since your edit". `y` deploys; any other key cancels. `x` discards a group
(also confirmed), and `r` / `R` both toggle reveal-all. A group cannot be discarded while it is deploying.

Deploy order for one service: upsert the changed variables without triggering a deploy, then the
deletes, then a redeploy of that service only. If something fails, the group stays pending and a
retry re-sends only the changes not yet applied. If the variables were written but the redeploy
failed ("written, not deployed"), a retry only redeploys. If the outcome of a call is unknown, the
variables are re-read from Railway first. Nothing is ever resent automatically.

### Pending storage

Pending changes are saved to `$XDG_STATE_HOME/railway-tui/pending.toml` (default
`~/.local/state/railway-tui/pending.toml`) with mode 0600, so they survive a restart. **The file
holds secret values in plain text.** A corrupt file is moved to `pending.toml.corrupt-<unix seconds>`
(never over an earlier one; the message names the path) and you start empty.

### First live run checklist

Do the first run on a non-production environment. Things not yet confirmed against the real API:

- Does deleting a variable trigger a deploy by itself? (`variableDelete` has no `skipDeploys`; the
  worst case is an extra deploy that already carries the new values.)
- Sealed variables (which cannot be read back) are now fetched via `isSealed` and shown as
  `<Opaque>`; confirm that matches Railway. How do `RAILWAY_*` variables appear in the view?
- What is Railway's exact "not found" wording when deleting a variable that is already gone?
- Does `$EDITOR` (`E`) receive every keystroke cleanly, with no characters lost to the TUI?

## Config

Optional, TOML, at `$XDG_CONFIG_HOME/railway-tui/railway-tui.conf` (default
`~/.config/railway-tui/railway-tui.conf`). Every key is optional; unknown keys are an error.

| key | meaning |
| --- | --- |
| `rate_limit_per_hour` | Railway API requests per hour to budget for. By default it is read from Railway's `RateLimit-Policy` header (100 until the first response). |
| `refresh_interval_secs` | Polling backstop in seconds (minimum 1). By default it spends about 10% of the hourly quota, between 15s and 600s. |
| `default_workspace` | Workspace to open, by id or name (case-insensitive). |
| `default_project` | Project to open, by id or name (case-insensitive). |
| `default_environment` | Environment to open, by id or name (case-insensitive). |
| `theme` | Colour theme by id or name: `tokyo-night`, `tokyo-night-storm`, `catppuccin-mocha`, `catppuccin-macchiato`, `catppuccin-frappe`, `catppuccin-latte`, `dracula`, `nord`, `gruvbox-dark`, `gruvbox-light`, `solarized-dark`, `solarized-light`, `one-dark`, `rose-pine`, `kanagawa`, `everforest`. Default `tokyo-night`. A theme picked with `t` takes precedence. Themes use 24-bit colour. |

Example:

```toml
rate_limit_per_hour = 1000
refresh_interval_secs = 60
default_workspace = "My Team"
default_project = "my-api"
default_environment = "production"
theme = "catppuccin-mocha"
```

## Development

```sh
cargo test
cargo clippy --all-targets -- -D warnings
```

The tests never call Railway. A separate, ignored suite checks the real API read-only using your
CLI login:

```sh
RAILWAY_TUI_LIVE=1 cargo test --test live_readonly -- --ignored --nocapture --test-threads=1
```
