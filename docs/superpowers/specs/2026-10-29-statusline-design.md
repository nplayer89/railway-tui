# NvChad-style statusline and command line — design

Short design for a small change. Builds on `2026-10-29-vars-copy-visual-design.md`, whose
`-- VISUAL --` title marker this replaces. Mockup:
https://claude.ai/artifact/EZgBeyuVYHtguCddRQT7Jt

## Intent

Neil wants the app to feel like his Neovim with NvChad: a mode marker at the bottom left, in a
statusline that looks like NvChad's, and a vim-style command line under it. The marker covers
visual and command modes (Neil's ask), plus the other modes the app already has.

## What gets built

### Layout

The screen becomes: header (2 rows, unchanged) / body / **statusline (1 row)** /
**command line (1 row)**. The statusline is the one new row. The command line replaces today's
footer.

### Mode

The mode is worked out from existing state on every draw. There is no new mode state.

| mode | when | colour (theme token) |
| --- | --- | --- |
| COMMAND | `Modal::Command` open | `busy` |
| FILTER | `Modal::Filter` open | `busy` (my call: themes have no cyan; it is the other "typing on the bottom line" mode) |
| INSERT | `Modal::VarInput` open (adding or editing a variable) | `ok` |
| VISUAL | `App::visual_range()` is `Some` | `hint` |
| NORMAL | anything else | `accent` |

The checks run top to bottom, and the first match wins.

### Statusline, left to right

1. **Mode block:** the vim icon and then the mode name, in bold, with the theme `bg` colour on a
   background of the mode colour. A `` separator follows, in the mode colour.
2. **View segment:** a file icon, then `<context> › <view>`, on `select_bg`, followed by a ``
   separator in `select_bg`. Context and view per screen:
   - Projects: `projects`
   - Services: `<project> › services`
   - Deployments: `<service> › deployments`
   - Instances: `<service> › instances`
   - Variables: `<service> › variables`
   - Pending: `<project> › pending`
   - Logs: `<service> › logs`

   A name that isn't loaded yet shows as `-`, as the header does today.
3. **Environment:** a branch icon, then the env name. The name is in `bad` when
   `is_production`, otherwise `muted`.
4. *(fill)*
5. **Selection count:** `N selected`, in `muted`, only in VISUAL mode.
6. **Project block:** a folder icon in the `bg` colour on `bad`, then the project name on
   `select_bg`. It is left out when no project is open.
7. **Position block:** a list icon in the `bg` colour on `ok`, then `row/total` on `select_bg`,
   counting the visible, filtered table rows. It is left out on Logs and on empty tables.

The rest of the bar uses the theme `bg`.

**Glyphs (Nerd Font, no fallback, by Neil's choice):**
- separator `` U+E0BC
- vim `` U+E62B
- file `` U+F15B
- branch `` U+E725
- folder `` U+F07B
- list `` U+F03A

Exact icon choice is the builder's within Nerd Fonts v3. The separator is fixed.

**Narrow terminals:** pieces drop out in this order until the bar fits: selection count, then
project block, then environment, then position block. After that the view text is truncated
with `…`. The mode block is never dropped.

### Command line (last row)

What it shows, in this order:
1. Open input, as today:
   - `:text▏` for a command
   - `/text▏` for a filter
   - `name: …▏` / `NAME = value▏` for a variable
   - the Discard confirm question
2. Otherwise `app.message`, in `busy`.
3. Otherwise `filter: <text>` in `hint`, when a table filter is applied.
4. Otherwise blank.

### Removed

- The footer's breadcrumb pill. The view segment replaces it.
- `-- VISUAL --` on the Variables title line.

## Non-goals

- Trimming the header's Workspace/Project/Env line (Neil may ask later).
- A plain-text fallback or a setting for icons.
- New theme colours.

## Approaches considered

1. **Derive the mode from existing state at draw time** (**chosen**). There is no duplicated
   state, so the bar can't disagree with what the app is doing.
2. **An explicit `Mode` that every key handler sets.** It duplicates `modal` and the visual
   anchor, and can drift (for example, still showing VISUAL after the anchor is cleared).
3. **A mode marker on the Variables screen only.** `:` and `/` work on every screen, so this
   doesn't fit.

## Left to the builder

- Exact icons within Nerd Fonts v3, and the padding inside segments.
- Where the module lives (`src/ui/statusline.rs` is the obvious place).
- Updating tests that look for the old footer pill or `-- VISUAL --`.
