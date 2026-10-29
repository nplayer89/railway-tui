# Statusline branch, scrolling workspace, one-line header — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Shrink the header to one line. In the statusline, show env, cursor branch and a
scrolling workspace block, and prefix the label with the project.

**Architecture:** `App` gains `cursor_branch()` and `workspace_name()`, and its existing
`status_label()` changes format. `src/ui/statusline.rs` gains a pure `marquee` helper and draws
the new segments. `draw_header` in `src/ui/mod.rs` becomes one line.

**Tech Stack:** Rust 2021, ratatui 0.30, chrono.

**Spec:** `docs/superpowers/specs/2026-10-29-statusline-branch-workspace-design.md`

## Global Constraints

- Header: one row. The status spans sit on the left, as today's second line (same text and
  styles). The hints are right-aligned in `hint`, and they are dropped first when the row is too
  narrow.
- `Workspace:`, `Project:` and `Env:` are removed from the header.
- Label format, exact:
  - Projects: `projects`
  - Services: `<project> › services`
  - Pending: `<project> › pending`
  - Other views: `<project> › <service> › deployments|instances|variables|logs`
  - Names not loaded yet show as `-`.
- Env segment: the globe icon `\u{f0ac}`, then the env name. On `is_production` it is bold and
  in `bad`, otherwise in `text`. It is omitted with no env, and never dropped for width.
- Branch segment: the branch icon `\u{e725}`, then the branch in `text`. It is omitted when
  `cursor_branch()` is `None`.
- Workspace block: the folder icon in `bg` on `bad`, then exactly 20 cells on `select_bg`. A name
  of ≤20 chars is left-aligned and padded. Longer names use the marquee: a loop of
  `name + "   "`, advancing one char per 250 ms of wall clock (`unix_ms / 250`). The block is
  omitted with no workspace.
- Drop order: selection count, then workspace, then branch, then position, then truncate the
  label with `…`. Mode and env are never dropped. Never panic.
- All Railway text goes through `clean()`.

## Review Focus

1. **Workspace names with multi-byte chars (`é`, emoji).** The marquee must slice by chars,
   never by bytes, so it can't panic. Covered by a test in Task 2.
2. **An image deploy (`branch: None`) or a service with no deployments.** The branch segment is
   hidden and nothing else shifts oddly. Covered by a test in Task 1.
3. **A name exactly 20 chars long.** It does not scroll. Covered by a test in Task 2.
4. **A very narrow header.** The status text stays and the hints go; no panic. Covered by a test
   in Task 2.
5. **Logs whose deployment isn't in the loaded lists.** `cursor_branch()` is `None`, with no
   panic. Covered by a test in Task 1.

---

### Task 1: `App` accessors: branch under the cursor, workspace name, new label

**Files:**
- Modify: `src/app/mod.rs` (`status_label` ~900; new methods beside it)
- Test: `src/app/tests.rs` (update the existing `status_label_names_the_context_and_view`)

**Interfaces:**
- Produces:
  - `pub fn status_label(&self) -> String`, in the new format (Global Constraints).
  - `pub fn cursor_branch(&self) -> Option<&str>`, per view:
    - Services: the selected service's `latest` deployment branch
    - Deployments: the selected row's `deployment.branch`
    - Instances: the branch of the selected instance's deployment
    - Variables: the branch of the service's `latest`
    - Logs: `find_deployment(&l.deployment_id)?.branch`
    - Projects and Pending: `None`
  - `pub fn workspace_name(&self) -> Option<&str>`: the name of the workspace whose id is
    `workspace_id`. It is the same lookup `draw_header` does today, moved here.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn status_label_starts_with_the_project() {
    // app_in_services → "<project> › services"
    // 'v' into svc-a variables → "<project> › <svc-a name> › variables"
}

#[test]
fn cursor_branch_follows_the_selected_deployment() {
    // services() fixtures: give svc-a's latest deploy branch Some("main") and svc-b's Some("feat/x")
    // app_in_services: cursor on svc-a → Some("main"); move down → Some("feat/x")
    // projects view → None
}

#[test]
fn cursor_branch_is_none_for_image_deploys_and_unknown_logs() {
    // latest.branch None → None; a Logs view whose deployment_id is not loaded → None
}

#[test]
fn workspace_name_resolves_the_current_workspace() {
    // app_in_services → Some(<fixture workspace name>)
}
```

- [ ] **Step 2: Run them and check they fail.** Run `cargo test --lib app::tests`. Expected:
  compile errors or failures.
- [ ] **Step 3: Implement** the three methods.
- [ ] **Step 4: Run all tests and check they pass.** Run `cargo test`. Expected: all pass. UI tests
  that assert the old label format may need updating; update them here.
- [ ] **Step 5: Commit:** `feat: expose the cursor's branch and workspace name; prefix labels with the project`

---

### Task 2: One-line header, env/branch/workspace segments, marquee

**Files:**
- Modify: `src/ui/statusline.rs` (new consts `ICON_GLOBE`; `pub(super) fn marquee(text: &str, width: usize, step: u64) -> String`; the new segments; drop order; `draw_statusline` takes the wall-clock step)
- Modify: `src/ui/mod.rs` (layout head `Length(1)`; `draw_header` becomes one line; pass `Utc::now()`-derived step to `draw_statusline`)
- Test: `src/ui/tests.rs`, plus unit tests for `marquee` in `statusline.rs` or `ui/tests.rs`

**Interfaces:**
- Consumes: `App::status_label`, `App::cursor_branch`, `App::workspace_name` (Task 1).
- Produces: `marquee(text, width, step)`:
  - When `text.chars().count() <= width`, it returns `text` padded with spaces to `width`.
  - Otherwise it returns `width` chars of the endless loop `text + "   "`, starting at
    `step % (len + 3)`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn marquee_pads_short_text_and_scrolls_long_text() {
    assert_eq!(marquee("shop", 6, 99), "shop  ");
    assert_eq!(marquee("abcdefghijklmnopqrst", 20, 5), "abcdefghijklmnopqrst"); // exactly 20: no scroll
    assert_eq!(marquee("abcdef", 4, 0), "abcd");
    assert_eq!(marquee("abcdef", 4, 3), "def ");
    assert_eq!(marquee("abcdef", 4, 7), "  ab");
    assert_eq!(marquee("abcdef", 4, 9), "abcd"); // wraps: loop length 9
    assert_eq!(marquee("éé😀abc", 3, 2), "😀ab");
}

#[test]
fn header_is_one_line_with_status_left_and_hints_right() {
    // render services app at 140 wide: row 0 contains "updated" and "<?> help"; no row contains "Workspace:" or "Project:"
}

#[test]
fn narrow_header_keeps_status_and_drops_hints() {
    // render at 30 wide: row 0 contains "updated", not "<?> help"; no panic
}

#[test]
fn statusline_shows_env_branch_and_scrolling_workspace() {
    // workspace name "Neil's Deployment Workspace", env "production", latest branch "feat/x"
    // status row contains "production" (fg bad, bold), "feat/x", and 20 workspace cells
    // two renders with steps differing by 1 show the workspace text shifted by one char
}

#[test]
fn non_production_env_is_plain_text() {
    // env "staging" → fg == theme.text, not bold
}

#[test]
fn statusline_drop_order_keeps_env_and_mode() {
    // VISUAL with a count; shrink width: count goes, then workspace, then branch, then position;
    // "NORMAL"/"VISUAL" and the env name are present at every tested width ≥ 30
}
```

Replace the existing drop-order test, the project-block tests and the header tests that the
new layout makes wrong. Keep their intent.

- [ ] **Step 2: Run them and check they fail.** Run `cargo test --lib ui::tests`.
- [ ] **Step 3: Implement.** In `draw`, pass `(wall.timestamp_millis().max(0) as u64) / 250` as
  the step. Tests call the draw path with a fixed time (`render` helpers may need a variant that
  takes the step or wall time).
- [ ] **Step 4: Run all checks and confirm they pass.** Run
  `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`.
- [ ] **Step 5: Commit:** `feat: one-line header; env, cursor branch and scrolling workspace in the statusline`
