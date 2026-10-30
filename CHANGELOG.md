# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project uses
[Semantic Versioning](https://semver.org/spec/v2.0.0.html). While the version is `0.y.z`, a minor
bump may contain breaking changes to keys, config or the on-disk files.

## [Unreleased]

## [0.1.0] - 2026-10-29

First release.

### Added

- Views: Projects, Services (one row per service with a coloured box per replica), Deployments,
  Instances and Logs, switched with keys or the `:` command line.
- Live build and runtime logs over Railway's websocket API, with a filter, follow mode, paging
  and automatic reconnect with backoff.
- Variables view: reveal, search by name, inline and `$EDITOR` editing, add, delete, copy over
  OSC 52, and a vim-style visual selection. Sealed variables show as `<Opaque>`.
- Pending variable changes kept in a 0600 file, reviewed per environment and service, and
  deployed only after confirmation, with safe retry of partly applied changes.
- Deploy the latest commit of a service's branch (`D`), or switch a service to another branch and
  deploy it (`d`), both confirmed.
- Request budgeting against Railway's hourly rate limit, with a configurable refresh interval.
- Login reused from the `railway` CLI, or `RAILWAY_API_TOKEN`.
- 16 colour themes with a live-preview picker, and an NvChad-style statusline.
- Optional TOML config for defaults, theme and rate limits; the last project and environment are
  reopened on launch.

[Unreleased]: https://github.com/nplayer89/railway-tui/compare/v0.1.0...HEAD
[0.1.0]: https://github.com/nplayer89/railway-tui/releases/tag/v0.1.0
