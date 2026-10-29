# NvChad-style statusline — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the footer with an NvChad-style statusline (mode block, view, env, project,
position) over a vim-style command line, and drop the `-- VISUAL --` title marker.

**Architecture:** `App` gains three read-only accessors: the mode (worked out from `modal` and
the visual anchor), the cursor position, and the status label for the current view. A new
`src/ui/statusline.rs` draws the statusline and the command line from those accessors. The old
footer code in `src/ui/mod.rs` moves into it.

**Tech Stack:** Rust 2021, ratatui 0.30.

**Spec:** `docs/superpowers/specs/2026-10-29-statusline-design.md`

## Global Constraints

- Mode precedence and colours, exact:
  - COMMAND (`Modal::Command`) uses `busy`
  - FILTER (`Modal::Filter`) uses `busy`
  - INSERT (`Modal::VarInput`) uses `ok`
  - VISUAL (`visual_range().is_some()`) uses `hint`
  - NORMAL uses `accent`
- Mode names, exact: `NORMAL`, `VISUAL`, `COMMAND`, `INSERT`, `FILTER`.
- Separator `\u{e0bc}` (``), exact. The other icons are Nerd Font v3 glyphs: vim `\u{e62b}`, file `\u{f15b}`, branch `\u{e725}`, folder `\u{f07b}`, list `\u{f03a}`. Define them as named consts in one place.
- There is no plain-text fallback.
- The view label separator is ` › `. Names not loaded yet show as `-`.
- Env name colour: `bad` when `ui::is_production(env)`, else `muted`.
- VISUAL count, exact: `N selected`.
- Narrow terminals drop pieces in this order: selection count, project block, env, position block. After that the view text is truncated with `…`. The mode block is never dropped.
- Command-line priority:
  1. open input, as today's footer renders it
  2. `app.message` in `busy`
  3. `filter: <text>` in `hint`
  4. blank
- Remove the footer breadcrumb pill and the `-- VISUAL --` title marker.
- All text from Railway passes through `clean()` as it does today. Values never reach a log.

## Review Focus

1. **A terminal narrower than the mode block plus the view** (for example 30 columns). It must render without panicking or overflowing, keep the mode block, and truncate the view with `…`. Covered by a test in Task 2.
2. **The Logs view.** The position block is hidden, and the label comes from `LogsView.service_name`. Covered by a test in Task 2.
3. **Long project or service names, or names with control characters.** They are cleaned and truncated, never break the layout. Covered by a test in Task 2.
4. **Mode while an overlay is open** (help, theme picker, env picker, deploy confirm). These are not modes, so the bar shows NORMAL. Covered by a test in Task 1.
5. **Message and filter at once.** The message wins, then the filter shows again once the message is cleared. Covered by a test in Task 2.

---

### Task 1: `App` accessors for mode, position and view label

**Files:**
- Modify: `src/app/mod.rs` (new `Mode` enum near `Kind`; methods near `selected_index` ~808)
- Test: `src/app/tests.rs`

**Interfaces:**
- Produces:
  - `#[derive(Debug, Clone, Copy, PartialEq, Eq)] pub enum Mode { Normal, Visual, Command, Insert, Filter }` with `pub fn name(self) -> &'static str`, which returns the exact names in Global Constraints.
  - `pub fn mode(&self) -> Mode`, with precedence as in Global Constraints.
  - `pub fn position(&self) -> Option<(usize, usize)>`: 1-based `(row, total)` of the top table view. It is `None` for Logs and for empty tables. Build it on `row_count_and_selected`.
  - `pub fn status_label(&self) -> String`: `<context> › <view>` as follows:
    - Projects: `projects`
    - Services: `<project> › services`
    - Deployments / Instances / Variables: `<service> › deployments|instances|variables`. Use the service name via `view_service_id` + `service`, else `-`.
    - Pending: `<project> › pending`
    - Logs: `<LogsView.service_name> › logs`
    - The label is returned uncleaned; the UI cleans it.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn mode_follows_modal_and_visual_anchor() {
    // app_in_variables: mode() == Mode::Normal
    // ':' → Command; Esc; '/' → Filter; Esc; 'a' → Insert; Esc
    // select DB_URL; 'v' → Visual; Esc → Normal
    // Mode::Visual.name() == "VISUAL"
}

#[test]
fn overlays_are_not_modes() {
    // '?' (help) → Normal; close; 't' (theme picker) → Normal
}

#[test]
fn position_is_one_based_and_absent_on_empty_or_logs() {
    // app_in_variables, select PORT → position() == Some((2, 3))
    // a filter that matches nothing → None
}

#[test]
fn status_label_names_the_context_and_view() {
    // app_in_services → "<project name> › services"
    // 'v' into variables of svc-a → "<svc-a's name> › variables"
}
```

- [ ] **Step 2: Run them and check they fail**

Run: `cargo test --lib app::tests`
Expected: compile error (no `Mode`).

- [ ] **Step 3: Implement** the four items in Interfaces.

- [ ] **Step 4: Run all tests and check they pass**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add src/app/mod.rs src/app/tests.rs
git commit -m "feat: expose the current mode, cursor position and status label"
```

---

### Task 2: Draw the statusline and command line

**Files:**
- Create: `src/ui/statusline.rs` with `pub(super) fn draw_statusline(frame, area, app)` and `pub(super) fn draw_cmdline(frame, area, app)`
- Modify: `src/ui/mod.rs`:
  - `draw` layout gets `[head 2, body, status 1, cmd 1]`
  - remove `draw_footer`, moving its input-prompt rendering into `draw_cmdline`
  - add `mod statusline;`
- Modify: `src/ui/vars.rs` (remove the `-- VISUAL --` span)
- Modify: `README.md` (one line: needs a Nerd Font)
- Test: `src/ui/tests.rs` (update the existing `-- VISUAL --` test ~1465 and any footer-pill assertions)

**Interfaces:**
- Consumes: `App::mode`, `Mode::name`, `App::position`, `App::status_label`, `App::visual_range` (Task 1); `ui::is_production`, `clean`, `Theme` tokens.
- Produces: no new public API.

Segment rendering, left to right (the spec has the full detail):
1. The mode block: `" {vim} {NAME} "` in bold, with `bg` text on a background of the mode colour. Then the separator, in the mode colour on `select_bg`.
2. `" {file} {label} "`, with `text` on `select_bg`. Then the separator, in `select_bg` on `bg`.
3. `" {branch} {env} "`, with the env colour per Global Constraints on `bg`.
4. The fill.
5. `"N selected "` in `muted` (only in VISUAL mode).
6. `" {folder} "` with `bg` text on `bad`, then `" {project} "` with `text` on `select_bg`.
7. `" {list} "` with `bg` text on `ok`, then `" {row}/{total} "` with `text` on `select_bg`.

Fitting algorithm: build the left and right span groups, measure them with `Line::width()`, and drop the optional groups in the Global Constraints order until `left + right <= area.width`. If the line is still too wide, truncate the label text with a trailing `…` so it fits. If the area can't hold even the mode block, render it clipped (ratatui clips).

- [ ] **Step 1: Write the failing tests**

Use `render_sized` and `find`/`text` from `src/ui/tests.rs`. Read the second-to-last row and the last row of the buffer.

```rust
#[test]
fn statusline_shows_mode_label_env_project_and_position() {
    // vars_app("production", [("A","1"),("B","2")]); render at 120x20
    // status row contains "NORMAL", "api › variables", "production", "proj", "1/2", and '\u{e0bc}'
    // the "N" of NORMAL has bg == theme.accent; "production" fg == theme.bad
}

#[test]
fn visual_mode_shows_in_the_statusline_with_a_count() {
    // 'v', 'j' → status row contains "VISUAL" and "2 selected"; mode cell bg == theme.hint
    // title row no longer contains "-- VISUAL --"
}

#[test]
fn command_mode_marks_the_bar_and_types_on_the_last_line() {
    // ':' then type "dep" → status row contains "COMMAND" (bg == theme.busy); last row starts with ":dep"
}

#[test]
fn insert_and_filter_modes_are_marked() {
    // 'a' → "INSERT" (bg == theme.ok); Esc; '/' → "FILTER", last row starts with "/"
}

#[test]
fn message_wins_over_filter_on_the_last_line() {
    // apply filter "A" (type '/', 'A', Enter); set app.message = Some("hello")
    // last row contains "hello", not "filter: A"; clear message → last row contains "filter: A"
}

#[test]
fn narrow_statusline_drops_pieces_and_keeps_the_mode() {
    // render at 40 wide → status row contains "NORMAL", not "proj"; no panic
    // render at 24 wide → status row contains "NORMAL" and '…'
}

#[test]
fn logs_statusline_has_no_position_and_names_the_service() {
    // a Logs view (reuse an existing logs UI test setup) → status row contains "› logs", not "/"
}

#[test]
fn breadcrumb_pill_is_gone() {
    // vars_app → no row contains "projects > "
}
```

- [ ] **Step 2: Run them and check they fail**

Run: `cargo test --lib ui::tests`
Expected: the new tests fail.

- [ ] **Step 3: Implement** `src/ui/statusline.rs` and the `mod.rs` and `vars.rs` changes. Add a README line under requirements: "Icons need a Nerd Font (v3) in your terminal."

- [ ] **Step 4: Run all checks and confirm they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass, no warnings.

- [ ] **Step 5: Commit**

```bash
git add src/ui/statusline.rs src/ui/mod.rs src/ui/vars.rs src/ui/tests.rs README.md
git commit -m "feat: NvChad-style statusline with mode block over a vim command line"
```
