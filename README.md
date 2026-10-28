# railway-tui

A k9s-style terminal UI for looking at [Railway](https://railway.com) from the keyboard: see every
service's deployment health at a glance, and get from a failed or crashed deployment to the log
line that explains it in a couple of keystrokes.

It is **read-only**. It never restarts, redeploys, scales or changes anything on Railway. It uses
only Railway's public GraphQL API (queries and subscriptions, no mutations).

Views: Projects, Services (one row per service with a coloured box per replica), Deployments
(history of one service), Instances (the replicas of a deployment) and Logs (live build or
runtime logs with a filter).

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
| `:` | command line: `:projects`, `:services`, `:deploys`, `:logs`, `:q` |
| `e` | environment picker (Services) |
| `l` | logs of the service's latest deployment (Services) |
| `i` | instances of the service's deployment (Services) |
| `b` | switch between build and runtime logs (Logs) |
| `f`, `G` | resume following the log (Logs) |
| `PageUp` / `PageDown` | scroll the log by a page (Logs) |
| `ctrl-r` | reload |
| `?` | toggle the help overlay |
| `q`, `ctrl-c` | quit |

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

Example:

```toml
rate_limit_per_hour = 1000
refresh_interval_secs = 60
default_workspace = "My Team"
default_project = "my-api"
default_environment = "production"
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
