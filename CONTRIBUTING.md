# Contributing

Thanks for your interest in railway-tui. Bug reports, ideas and pull requests are all welcome.

## Reporting bugs and asking for features

Open an [issue](https://github.com/nplayer89/railway-tui/issues). For a bug, include your OS,
terminal, the railway-tui version (or commit), what you did, and what you expected. Never paste
tokens, variable values or the contents of `pending.toml`.

Security problems go through private reporting instead; see [SECURITY.md](SECURITY.md).

## Development

You need a Rust toolchain (CI uses 1.96). Before opening a pull request, run what CI runs:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

The tests never call Railway. A separate, ignored suite checks the real API read-only using your
CLI login:

```sh
RAILWAY_TUI_LIVE=1 cargo test --test live_readonly -- --ignored --nocapture --test-threads=1
```

## Writes to Railway

railway-tui is read-only by design, with a small, deliberate set of exceptions (variables,
redeploys and deploying a branch). `tests/no_mutations.rs` guards the list of GraphQL mutations
the code may send. A pull request that adds a new kind of write needs a clear reason, a
confirmation step in the UI, and an update to that test.

## Pull requests

- Keep each pull request to one change, and add or update tests for it.
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/)
  (`feat:`, `fix:`, `docs:`, `chore:` ...).
- Add a line under `[Unreleased]` in [CHANGELOG.md](CHANGELOG.md) for anything a user would notice.

## Releasing

1. Move the `[Unreleased]` entries in `CHANGELOG.md` under a new version heading, and bump
   `version` in `Cargo.toml` (`cargo check` updates `Cargo.lock`).
2. Merge that to `main`, then tag it: `git tag -a v0.2.0 -m v0.2.0 && git push origin v0.2.0`.
3. The release workflow checks the tag matches `Cargo.toml`, builds the binaries and publishes
   the GitHub release with that version's changelog section as its notes.

## License

Unless you say otherwise, any contribution you submit is dual-licensed as MIT OR Apache-2.0, as
described in the [README](README.md#license), with no additional terms or conditions.
