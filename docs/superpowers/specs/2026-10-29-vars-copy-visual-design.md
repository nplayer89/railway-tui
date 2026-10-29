# Variables screen: value states, copy, visual selection — design

Short design for a small change. Builds on `2026-10-28-env-vars-design.md`.

## Intent

Neil wants the Variables screen to say what a value *is* when it isn't shown, to copy variables
out to the clipboard (single values, or `.env` lines to paste into a file), and to act on several
rows at once the way vim's visual mode does.

## What gets built

### Value display

The VALUE column has three non-value states, checked in this order:

| state | shown | when |
| --- | --- | --- |
| sealed | `<Opaque>` | Railway marks the variable `isSealed` (shown even when "revealed": there is nothing to reveal) |
| empty | `<Empty>` | the value is the empty string (shown even while hidden: nothing to hide) |
| hidden | `••••••` | any other value not revealed with `r`/`R` |

Otherwise the revealed value is shown as today. A pending set shows the pending value's state, so
setting a variable to `""` shows `<Empty>`.

**Sealed detection.** The `variables` map cannot tell sealed from empty. The Variables load also
fetches `environment(id).variables` (the `Variable` connection, which carries `name`, `serviceId`,
`isSealed`) and keeps the names that are sealed for this service. Both arrive with the same load, so
the screen never shows a sealed variable as `<Empty>` once loaded. If the sealed query fails, the
view still loads and shows the values. A sealed variable may then wrongly show as `<Empty>`, and a
one-line message says sealed status is unknown.

### Copy (`y` / `Y`)

- `y`: copy the selected variable's value, raw (multi-line kept as-is).
- `Y`: copy `NAME=VALUE` in `.env` format.
- The value copied is the one the row shows: the pending value for a pending set, else Railway's.
  Hidden-ness doesn't matter: `y` copies the real value while it shows as dots.
- Sealed: nothing to copy. `y`/`Y` on a sealed row shows a message and copies nothing.
- `.env` quoting: a value is left bare if it is non-empty and has only characters safe unquoted;
  otherwise it is wrapped in double quotes, with `\` → `\\`, `"` → `\"`, newline → `\n`, CR → `\r`.
  Characters that force quoting: whitespace, `#`, `"`, `'`, `\`, `=`, `$`, backtick. An empty value
  is written `NAME=`.
- The status line confirms: `copied value of NAME` / `copied 3 variables (1 sealed skipped)`.

**Clipboard mechanism: OSC 52** (Neil's pick, over `pbcopy` and the `arboard` crate). The app
writes `ESC ] 52 ; c ; <base64> BEL` to the terminal. It works locally and over SSH with no extra
programs. The trade-off Neil accepted: some terminals disable it or cap its size, and the app cannot
detect that. The confirmation message therefore says what was *sent*, not that the clipboard took
it. The copy is a `Command` handled where the terminal is owned (`main.rs`), like `EditExternal`.
The payload is a value, so it is wrapped in `Secret` and never logged.

### Visual selection (`v`)

- `v` starts visual mode anchored at the selected row. `j`/`k`/arrows move the cursor, and every
  row between the anchor and the cursor is highlighted (a distinct style from the cursor row).
- `y` / `Y` copy every selected row (values joined by newlines for `y`; `.env` lines for `Y`) in
  table order, skipping sealed rows. Then visual mode ends.
- `d` marks every selected row for delete (the existing staged, undoable delete, with no
  confirm), then visual mode ends. Rows already marked for delete stay marked: in visual mode `d`
  only marks, it never toggles. Sealed variables can be deleted. It is refused while the service
  is deploying, as single `d` is today.
- `r` toggles reveal over the selected rows as a group. If any selected row is hidden, all of
  them are revealed; if all are already revealed, all are hidden. Then visual mode ends. Sealed
  and empty rows show the same either way. Like single `r` today, it changes the per-row set
  only, so while `R` (reveal all) is on every row stays shown.
- `Esc` or `v` again leaves visual mode without acting. Leaving the view, filtering, or the row
  list changing under the selection also ends it.
- While visual mode is on, the title line shows `-- VISUAL --` and other edit keys (`a`, `e`, `E`,
  `u`) are ignored.

## Non-goals

- `u` or edit over a visual range (only `y`, `Y`, `d`, `r` were asked for).
- Reading a sealed value by any route. Railway doesn't allow it.
- A fallback clipboard path (`pbcopy` etc.). OSC 52 only.
- Copying on the Pending review screen.

## Approaches considered

1. **`pbcopy` / `wl-copy` / `xclip` subprocess.** Reliable on the local machine, but copies to the
   host running the TUI, so useless over SSH.
2. **OSC 52 escape** (**chosen**). Works locally and over SSH with no dependency. Delivery can't be
   confirmed and some terminals cap or block it.
3. **`arboard` crate.** Native clipboard, no subprocess. Local-only like option 1, and adds a
   dependency (heavier on Linux).

## Left to the builder

- Base64: a small hand-written encoder or the `base64` crate.
- The exact highlight style for the visual range within the current theme.
- Where the sealed-name set lives (alongside the loaded `Variables` is the obvious place).
- Updating the help overlay and README key list.
