# Variables: value states, copy, visual selection — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** On the Variables screen, show `<Opaque>` / `<Empty>` instead of dots where they apply,
copy values with `y` / `Y` (`.env` lines) over OSC 52, and add a vim-style `v` range that `y`,
`Y`, `d` and `r` act on.

**Architecture:** Two pure helpers (`.env` line quoting, OSC 52 encoding) live in a new
`src/clipboard.rs`. The View-purpose variables load also fetches Railway's `isSealed` flags and
hands them to the open `VarsView`. The app emits a new `Command::Copy`, which `main.rs` writes to
the terminal, like `EditExternal`. Visual mode is an anchor name on `VarsView`, and the range is
computed from the anchor and the cursor each time it is needed.

**Tech Stack:** Rust 2021, ratatui 0.30, crossterm 0.29, graphql_client 0.16, wiremock for worker tests.

**Spec:** `docs/superpowers/specs/2026-10-29-vars-copy-visual-design.md`

## Global Constraints

- No new crates: write base64 by hand.
- Display strings, exact: `<Opaque>`, `<Empty>`, `••••••` (existing `HIDDEN`), `-- VISUAL --`.
- Display order: sealed → `<Opaque>`; else the value is `""` → `<Empty>`; else hidden → `••••••`; else the value.
- OSC 52 sequence, exact: `ESC ] 52 ; c ; <base64 of UTF-8 text> BEL` (`"\x1b]52;c;{b64}\x07"`).
- `.env` quoting: bare only when the value is non-empty and has none of: whitespace, `#`, `"`, `'`, `\`, `=`, `$`, backtick. Otherwise wrap it in `"…"` and escape `\`→`\\`, `"`→`\"`, LF→`\n`, CR→`\r`. An empty value is written `NAME=`.
- Values never reach a log, panic, or `Debug` output: clipboard payloads travel as `Secret<String>`.
- Copy messages, exact (the message names what was sent, never the value):
  - `y` on one row: `copied value of NAME`
  - `Y` on one row: `copied NAME`
  - a visual range: `copied N variables`, with ` (K sealed skipped)` appended when K > 0
  - a sealed single row: `NAME is sealed — nothing to copy`
- If the sealed lookup fails, show the message `sealed status unknown: <error>`. The view still loads.

## Review Focus

1. **A sealed variable missing from the `variables` map.** Railway may omit sealed variables from the map. It must still get a row showing `<Opaque>`, which can be deleted. Covered by a test in Task 2.
2. **A stale anchor.** If the anchor row disappears (it was deleted and its pending entry undone, or it was reloaded away) or a filter hides it, visual mode must quietly end, with no panic and no wrong range. Covered by a test in Task 5.
3. **`y` over a range that is entirely sealed.** Nothing is copied: no `Command::Copy`, and the message is `copied 0 variables (2 sealed skipped)`. Covered by a test in Task 5.
4. **Multi-line or unicode values over OSC 52.** The base64 must be of the UTF-8 bytes, and `y` copies multi-line values raw. Covered by a test in Task 1 (`é`) and Task 4.
5. **Visual-mode `d` on rows already pending delete.** They stay deleted (never toggled back). Covered by a test in Task 5.

---

### Task 1: `.env` line and OSC 52 helpers

**Files:**
- Create: `src/clipboard.rs` (unit tests in a `#[cfg(test)] mod tests` at the bottom, as `src/worker.rs` does)
- Modify: `src/lib.rs` (add `pub mod clipboard;`)

**Interfaces:**
- Produces: `pub fn env_line(name: &str, value: &str) -> String` (no trailing newline) and `pub fn osc52(text: &str) -> String`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn env_line_leaves_plain_values_bare() {
    assert_eq!(env_line("PORT", "8080"), "PORT=8080");
    assert_eq!(env_line("URL", "postgres://u@h:5432/db"), "URL=postgres://u@h:5432/db");
}

#[test]
fn env_line_quotes_and_escapes_where_needed() {
    assert_eq!(env_line("E", ""), "E=");
    assert_eq!(env_line("A", "has space"), "A=\"has space\"");
    assert_eq!(env_line("B", "a#b"), "B=\"a#b\"");
    assert_eq!(env_line("C", "x=y"), "C=\"x=y\"");
    assert_eq!(env_line("D", "${{X.Y}}"), "D=\"${{X.Y}}\"");
    assert_eq!(env_line("F", "say \"hi\""), "F=\"say \\\"hi\\\"\"");
    assert_eq!(env_line("G", "a\\b"), "G=\"a\\\\b\"");
    assert_eq!(env_line("H", "l1\nl2\r"), "H=\"l1\\nl2\\r\"");
    assert_eq!(env_line("I", "it's"), "I=\"it's\"");
    assert_eq!(env_line("J", "`x`"), "J=\"`x`\"");
}

#[test]
fn osc52_wraps_base64_of_utf8() {
    assert_eq!(osc52(""), "\x1b]52;c;\x07");
    assert_eq!(osc52("f"), "\x1b]52;c;Zg==\x07");
    assert_eq!(osc52("fo"), "\x1b]52;c;Zm8=\x07");
    assert_eq!(osc52("foo"), "\x1b]52;c;Zm9v\x07");
    assert_eq!(osc52("é\n"), "\x1b]52;c;w6kK\x07");
}
```

- [ ] **Step 2: Run them and check they fail**

Run: `cargo test --lib clipboard`
Expected: compile error (`env_line` / `osc52` not found).

- [ ] **Step 3: Implement `env_line` and `osc52` in `src/clipboard.rs`**

Write a standard base64 encoder with `=` padding, using the standard alphabet. Give the module a
one-line `//!` doc in the style of the others.

- [ ] **Step 4: Run them and check they pass**

Run: `cargo test --lib clipboard`
Expected: 3 passed.

- [ ] **Step 5: Commit**

```bash
git add src/clipboard.rs src/lib.rs
git commit -m "feat: .env line quoting and OSC 52 encoding helpers"
```

---

### Task 2: Fetch sealed flags with the Variables view load

**Files:**
- Create: `graphql/SealedVariables.graphql`
- Modify: `src/api/mod.rs` (new op + method, next to `variables` at ~line 261)
- Modify: `src/app/mod.rs` (`DataEvent::Variables` ~304, `VarsView` ~60, `open_variables` ~1195, the `DataEvent::Variables` arm ~861, `VarRow` ~1564, `variable_rows` ~706)
- Modify: `src/worker.rs` (`Command::LoadVariables` arm ~265; worker tests at bottom)
- Modify: every `DataEvent::Variables { … }` and `VarsView { … }` literal in tests (`src/app/tests.rs` ~919 and ~1675, `src/ui/tests.rs` ~693, and others `cargo build --tests` reports)
- Test: `src/worker.rs`, `src/app/tests.rs`

**Interfaces:**
- Produces:
  - `Api::sealed_variables(&self, env_id: &str, service_id: &str) -> Result<BTreeSet<String>, ApiError>`: names with `isSealed == true` and `serviceId == service_id`.
  - `DataEvent::Variables { …, sealed: Option<Result<BTreeSet<String>, String>> }`: `Some` only for `VarsPurpose::View`.
  - `VarsView { service_id, revealed, reveal_all, sealed: BTreeSet<String>, anchor: Option<String> }` (`anchor` is used in Task 5; add it now, initialised to `None`).
  - `VarRow { name, current, pending, sealed: bool }`.

The query, paged:

```graphql
query SealedVariables($environmentId: String!, $after: String) {
  environment(id: $environmentId) {
    variables(first: 100, after: $after) {
      edges { node { name serviceId isSealed } }
      pageInfo { hasNextPage endCursor }
    }
  }
}
```

Loop while `hasNextPage`, passing `endCursor` as `after`.

- [ ] **Step 1: Write the failing tests**

In `src/worker.rs` tests (use the existing `mount` helper):

```rust
#[tokio::test]
async fn view_load_reports_sealed_names_for_the_service() {
    // mount "query Variables" → {"data":{"variables":{"A":"1","S":""}}}
    // mount "query SealedVariables" → two nodes: {name:"S",serviceId:"svc",isSealed:true},
    //   {name:"A",serviceId:"svc",isSealed:false}, plus {name:"S2",serviceId:"other",isSealed:true};
    //   pageInfo.hasNextPage false
    // send LoadVariables { purpose: View, service_id: "svc", .. }
    // assert the DataEvent::Variables has sealed == Some(Ok(["S"].into()))
}

#[tokio::test]
async fn deploy_load_skips_the_sealed_query() {
    // purpose: Deploy → event has sealed == None, and no request named SealedVariables was made
}
```

In `src/app/tests.rs` (extend `vars_event` to take `sealed: &[&str]`, or add `vars_event_sealed`):

```rust
#[test]
fn sealed_names_mark_rows_and_add_missing_ones() {
    // load DB_URL, PORT with sealed = ["PORT", "HIDDEN_KEY"]
    // names(&app) == ["DB_URL", "HIDDEN_KEY", "PORT"]
    // the rows for PORT and HIDDEN_KEY have sealed == true; HIDDEN_KEY has current == None
}

#[test]
fn sealed_lookup_failure_still_loads_with_a_message() {
    // sealed = Some(Err("boom")) → rows load, app.message == Some("sealed status unknown: boom")
}

#[test]
fn a_sealed_row_missing_from_the_map_can_be_deleted() {
    // HIDDEN_KEY sealed and absent from the map; select it, press 'd'
    // → the pending store has Change::Delete for HIDDEN_KEY
}
```

- [ ] **Step 2: Run them and check they fail**

Run: `cargo test sealed`
Expected: compile errors (no `sealed` field / no `SealedVariables` op).

- [ ] **Step 3: Implement**

- Add the op struct to `src/api/mod.rs` the way `Variables` is declared. Write `sealed_variables`.
- In the worker's `LoadVariables` arm, for `VarsPurpose::View` run `api.variables` and
  `api.sealed_variables` concurrently (`tokio::join!` or `futures::join!`). Send
  `sealed: Some(result.map_err(|e| e.to_string()))`. For other purposes send `sealed: None`.
- In the app's View arm, after `self.variables.finish(…)`, store `Ok` names in the top
  `VarsView.sealed`, and on `Err(e)` set `self.message` to the exact text in Global Constraints.
- `variable_rows`: for each sealed name absent from the rows, insert
  `VarRow { current: None, pending: None, sealed: true }`. Set `sealed` on rows present in both.
  Filtering still applies.
- `toggle_delete` must work for a row whose `current` is `None` but `sealed` is true. It should
  stage `Change::Delete`, not treat the row as nonexistent. Check `var_state` / `pending.delete`
  and adjust so that the test passes.

- [ ] **Step 4: Run all tests and check they pass**

Run: `cargo test`
Expected: all pass.

- [ ] **Step 5: Commit**

```bash
git add graphql/SealedVariables.graphql src/api/mod.rs src/worker.rs src/app/mod.rs src/app/tests.rs src/ui/tests.rs
git commit -m "feat: load which variables are sealed with the Variables view"
```

---

### Task 3: `<Opaque>` and `<Empty>` in the VALUE column

**Files:**
- Modify: `src/ui/vars.rs` (`shown`, `row`)
- Test: `src/ui/tests.rs` (extend `vars_app` so a test can set sealed names, for example `vars_app_sealed(env, vars, sealed)`)

**Interfaces:**
- Consumes: `VarRow.sealed` (Task 2).
- Produces: `pub(super) const OPAQUE: &str = "<Opaque>"`, `pub(super) const EMPTY: &str = "<Empty>"`. `shown(value, revealed)` now returns `EMPTY` for `""` regardless of `revealed`. That also applies on the Pending review screen, which calls `shown`. This is deliberate: an empty pending value reads `<Empty>` there too.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn sealed_shows_opaque_even_when_revealed() {
    // vars_app with ("TOKEN", "") sealed ["TOKEN"]; render → contains "<Opaque>", not "<Empty>", not "••••••"
    // press 'R'; render → still "<Opaque>"
}

#[test]
fn empty_value_shows_empty_even_while_hidden() {
    // vars_app with ("BLANK", ""), ("KEY", "s3cret"); render → contains "<Empty>" and "••••••"
}

#[test]
fn pending_set_to_empty_shows_empty() {
    // stage Set { value: "" } on KEY; render Variables → KEY row shows "<Empty>"
}
```

- [ ] **Step 2: Run them and check they fail**

Run: `cargo test --lib ui::tests::sealed_shows ui::tests::empty_value ui::tests::pending_set_to_empty`
Expected: FAIL (dots shown).

- [ ] **Step 3: Implement.** In `row`, a sealed row's value cell is `OPAQUE`, whatever its pending state or reveal state. Otherwise keep the current `shown` path.

- [ ] **Step 4: Run all tests and check they pass**

Run: `cargo test`
Expected: all pass. The existing `values_hidden_until_revealed` test is unchanged.

- [ ] **Step 5: Commit**

```bash
git add src/ui/vars.rs src/ui/tests.rs
git commit -m "feat: show <Opaque> for sealed and <Empty> for empty variables"
```

---

### Task 4: `y` / `Y` on the selected row, delivered over OSC 52

**Files:**
- Modify: `src/app/mod.rs` (`Command` enum ~353)
- Modify: `src/app/keys.rs` (`variables_key` ~390)
- Modify: `src/main.rs` (`dispatch` ~242)
- Modify: `src/ui/mod.rs` (footer hint ~108, help lines ~357)
- Test: `src/app/tests.rs`

**Interfaces:**
- Consumes: `clipboard::env_line`, `clipboard::osc52` (Task 1), `VarRow.sealed` (Task 2).
- Produces:
  - `Command::Copy { text: Secret<String> }`, with a doc comment saying it is handled by the main loop, not the worker.
  - `fn copy_rows(&mut self, names: &[String], as_env: bool, out: &mut Vec<Command>)` in `keys.rs`. It builds the text and the message for 1..n names, and Task 5 reuses it. The value for a name is the row's shown value: a pending `Set` value, else `current`. A pending `Delete` copies `current`. Sealed rows are skipped. With `y`, values are joined by `\n`. With `Y`, `env_line`s are joined by `\n`. No `Command` is pushed when nothing is copyable.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn y_copies_the_selected_value_raw() {
    // app_in_variables; select_var "PORT"; press 'y'
    // → vec![Command::Copy { text: Secret::new("has db inside".into()) }], message "copied value of PORT"
}

#[test]
fn shift_y_copies_an_env_line() {
    // select "PORT"; press 'Y' → Copy text "PORT=\"has db inside\"", message "copied PORT"
}

#[test]
fn y_copies_the_pending_value_and_ignores_hiddenness() {
    // stage Set REDIS_DB = "l1\nl2"; value is hidden; 'y' → Copy text "l1\nl2" (raw, multi-line)
}

#[test]
fn y_on_a_sealed_row_copies_nothing() {
    // sealed PORT; 'y' → no Copy command; message "PORT is sealed — nothing to copy"
}

#[test]
fn y_is_allowed_while_the_service_deploys() {
    // copying is not an edit: with the service deploying, 'y' still yields Copy
}
```

- [ ] **Step 2: Run them and check they fail**

Run: `cargo test --lib copies`
Expected: compile error (no `Command::Copy`).

- [ ] **Step 3: Implement**

- `variables_key`: `y` / `Y` call `copy_rows(&[selected], as_env, out)`. They are not in the `edits` set.
- `main.rs` `dispatch`: `Command::Copy { text }` writes `clipboard::osc52(text.expose())` to
  `stdout()` and flushes. A write error becomes `Error::Terminal`, as other terminal I/O does.
  It never reaches the worker.
- Footer hint for Variables: add `<y/Y> copy` and `<v> select`, keeping the line's existing
  style. Help: add a line `  y copy value  Y copy NAME=VALUE  v select rows`.

- [ ] **Step 4: Run all tests and check they pass**

Run: `cargo test`
Expected: all pass. Fix any footer/help snapshot assertions that change.

- [ ] **Step 5: Commit**

```bash
git add src/app/mod.rs src/app/keys.rs src/main.rs src/ui/mod.rs src/app/tests.rs src/ui/tests.rs
git commit -m "feat: copy a variable's value or NAME=VALUE with y / Y over OSC 52"
```

---

### Task 5: Visual mode (`v`) with `y`, `Y`, `d`, `r`

**Files:**
- Modify: `src/app/keys.rs` (`variables_key`, and wherever `/` opens the filter)
- Modify: `src/app/mod.rs` (new range accessor)
- Modify: `src/ui/vars.rs` (title line, range highlight)
- Modify: `src/ui/tables.rs` (a range style next to `row_style`, if one is needed)
- Modify: `README.md` (key list)
- Test: `src/app/tests.rs`, `src/ui/tests.rs`

**Interfaces:**
- Consumes: `VarsView.anchor` (Task 2), `copy_rows` (Task 4), the existing `toggle_delete` / `with_target` / `refuse_while_deploying`.
- Produces: `pub fn visual_range(&self) -> Option<RangeInclusive<usize>>` on `App`. It covers the
  indices into `variable_rows()` between the anchor and the cursor. It returns `None` when the
  view isn't Variables, when there is no anchor, or when the anchor's name is not in the current
  rows.

Behaviour (from the spec):
- `v`: with no anchor, it sets the anchor to the selected name (if there is one). With an
  anchor, it clears it. `Esc` with an anchor clears it, and only that: it does not pop the view.
- `j`/`k`/arrows: they are unchanged. The cursor moves and the range follows.
- `y`/`Y`: `copy_rows` over the range's names in table order, then clear the anchor.
- `d`: refused while deploying, as today. For each name in the range that is not already a
  pending `Delete`, stage a delete (the code path `toggle_delete` uses for the not-deleted
  case). Then clear the anchor.
- `r`: if any name in the range is not in `revealed`, insert all of them. Otherwise remove all of
  them. Then clear the anchor.
- `a`, `e`, `E`, `Enter`, `u`: ignored (they return `true` and do nothing) while an anchor is set.
- `/` (opening the filter) clears the anchor. A stale anchor (not in the rows) is treated as no
  anchor, and the next keypress clears it.
- Title line: append ` -- VISUAL --` while `visual_range()` is `Some`.
- Rows in the range get a style distinct from the plain cursor row, for example `t.select_bg`
  without BOLD for the non-cursor rows of the range. The cursor row keeps `row_style(…, true)`.

- [ ] **Step 1: Write the failing tests**

`src/app/tests.rs` (rows in `app_in_variables` are DB_URL, PORT, REDIS_DB):

```rust
#[test]
fn v_then_j_selects_a_range_and_esc_cancels_without_leaving() {
    // select DB_URL; 'v'; 'j' → visual_range() == Some(0..=1)
    // Esc → visual_range() == None and the top view is still Variables
}

#[test]
fn visual_y_copies_values_in_table_order_and_ends() {
    // select REDIS_DB; 'v'; 'k'; 'k'; 'y' → Copy text "postgres://x\nhas db inside\n1",
    //   message "copied 3 variables", visual_range() == None
}

#[test]
fn visual_shift_y_copies_env_lines_skipping_sealed() {
    // PORT sealed; range all three; 'Y' → Copy text "DB_URL=postgres://x\nREDIS_DB=1",
    //   message "copied 2 variables (1 sealed skipped)"
}

#[test]
fn visual_y_over_only_sealed_rows_copies_nothing() {
    // DB_URL and PORT sealed; range 0..=1; 'y' → no Copy; message "copied 0 variables (2 sealed skipped)"
}

#[test]
fn visual_d_marks_every_row_and_never_unmarks() {
    // 'd' on PORT first (pending Delete); then range all three; 'd'
    // → all three are pending Delete; visual_range() == None
}

#[test]
fn visual_d_is_refused_while_deploying() {
    // service deploying; range; 'd' → nothing staged; the deploy-refusal message is set
}

#[test]
fn visual_r_reveals_all_then_hides_all() {
    // reveal PORT alone with 'r'; range all three; 'r' → revealed == {DB_URL, PORT, REDIS_DB}
    // range again; 'r' → revealed is empty
}

#[test]
fn visual_ignores_edit_keys() {
    // range; 'a' → no VarInput modal; 'e' → no inline edit; visual still active
}

#[test]
fn stale_anchor_ends_visual_mode() {
    // add a pending NEW var, anchor on NEW, 'j'; undo NEW ('u' is ignored in visual, so
    //   remove it through the pending store directly) → visual_range() == None, no panic
    // filter: anchor DB_URL; '/' → anchor cleared
}
```

`src/ui/tests.rs`:

```rust
#[test]
fn visual_mode_shows_title_marker() {
    // vars_app with two vars; 'v' → text contains "-- VISUAL --"; Esc → it doesn't
}
```

- [ ] **Step 2: Run them and check they fail**

Run: `cargo test visual`
Expected: FAIL (`visual_range` not found).

- [ ] **Step 3: Implement** the behaviour list above.

- [ ] **Step 4: Run all tests, clippy and fmt, and check they pass**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all pass, no warnings.

- [ ] **Step 5: Update the README key list** (Variables section) with `y`, `Y`, `v` and the visual-mode keys. Also update the README's open question about sealed variables to say they are now fetched via `isSealed`.

- [ ] **Step 6: Commit**

```bash
git add src/app/keys.rs src/app/mod.rs src/ui/vars.rs src/ui/tables.rs src/app/tests.rs src/ui/tests.rs README.md
git commit -m "feat: vim-style visual selection on Variables with y, Y, d, r"
```

---

### Task 6: First live check (Neil runs it)

These are not automatable and need Neil's live Railway project. Per standing instructions, ask
before touching anything live.

- [ ] Does `environment.variables` include **service** variables, or only shared ones? If a known
  sealed service variable doesn't show `<Opaque>`, the query in Task 2 needs another source.
- [ ] How does a sealed variable come back from `variables(unrendered: true)`: missing, `""`, or
  something else? Either way it should now show `<Opaque>`.
- [ ] In Neil's terminal, `y` puts the value on the macOS clipboard. If it doesn't, check that
  the terminal allows OSC 52 (and, inside tmux, that `set-clipboard` is on).
