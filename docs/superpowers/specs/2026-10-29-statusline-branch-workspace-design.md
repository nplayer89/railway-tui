# Statusline: branch, scrolling workspace, minimal header — design

Short design for a small change. It follows `2026-10-29-statusline-design.md`.
Mockup (Option B boards): https://claude.ai/artifact/EZgBeyuVYHtguCddRQT7Jt

## Intent

Neil wants the top header and the NvChad-style bar to stop repeating each other. The bar's
branch segment should show the git branch of the deployment under the cursor, and the red
folder block should hold the workspace name, in a wider box that scrolls when the name doesn't
fit. He picked Option B: the header shrinks to one line, and the environment moves into the
bar.

## What gets built

### Header: one line (was two)

- **Left:** today's second line, in its existing styles: freshness or `STALE: …`, `N pending`,
  `live updates off`, and `rate limited, retrying in Ns`.
- **Right-aligned:** the key hints for the view.
- If the line is too narrow, the hints are dropped first. The status text is never dropped.
- `Workspace:`, `Project:` and `Env:` leave the header.
- The body gains one row.

### Statusline, left to right

1. **Mode block:** unchanged.
2. **View label:** it now always starts with the project, so the project stays on screen
   without a header (my call):
   - Projects: `projects`
   - Services: `shop › services`
   - Deployments / Instances / Variables / Logs: `shop › api › deployments|instances|variables|logs`
   - Pending: `shop › pending`
3. **Environment (new, replaces the old env-after-branch):**
   - A globe icon (`\u{f0ac}`), then the env name.
   - On production (`is_production`), it is bold and in `bad`. Otherwise it is in `text`.
   - It is left out when no environment is chosen.
4. **Branch:** the branch icon (`\u{e725}`), then the branch of the deployment under the
   cursor, in `text`. Which deployment that is depends on the view:
   - Services: the selected service's latest deployment (`ServiceInstance.latest`)
   - Deployments: the selected row's deployment
   - Instances: the selected instance's deployment
   - Variables: the service's latest deployment
   - Logs: the logged deployment (`find_deployment(&LogsView.deployment_id)`)
   - Projects and Pending: none

   The segment is hidden when there is no deployment, or when it has no branch (an image
   deploy).
5. *(fill)*
6. **Selection count:** unchanged (VISUAL only).
7. **Workspace block (replaces the project block):**
   - The folder icon in `bg` on `bad`, then the workspace name in a fixed 20-cell box on
     `select_bg`.
   - A name that fits is left-aligned and padded to 20.
   - A longer name scrolls left one character every 250 ms, as a loop of `name + "   "`.
   - The offset comes from the wall clock already passed to `draw`: `(unix ms / 250)`. There is no counter state, and
     the app's existing 250 ms tick redraws it.
   - The block is left out when no workspace is known.
8. **Position block:** unchanged.

**Narrow terminals, new drop order:** selection count, then workspace, then branch, then
position. After that the label is truncated with `…`. The mode block and the environment are
never dropped, since the environment is the production warning.

## Non-goals

- Making other overlong segments scroll (only the workspace scrolls).
- A setting for the box width.
- Changing the command line.

## Approaches considered

1. **Scroll offset from the clock at draw time** (**chosen**). This has no new state and
   matches how the mode is already worked out. Time is passed to `draw` already.
2. **A scroll counter bumped on every tick.** That is more state to keep in step, and it would
   also scroll while the name is hidden.
3. **Truncating the name with `…` instead of scrolling.** It's simpler, but Neil asked for
   scrolling.

## Left to the builder

- How the wall-clock step reaches the statusline (tests pass a fixed time).
- The exact globe icon within Nerd Fonts v3.
- Updating tests that look for `Workspace:` / `Project:` / `Env:` in the header or for the old
  project block.
