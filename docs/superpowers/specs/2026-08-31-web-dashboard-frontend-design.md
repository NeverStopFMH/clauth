# Web dashboard — Phase 2: frontend design

## Status

**Approved by user (brainstorming walkthrough, including a visual-companion mockup
session, 2026-08-31).** Implementation starts immediately after this document, tab by
tab, starting with Overview + Fallback (see Rollout order below).

This phase also **revises Phase 1's auth model** — see "Auth model: removed" below. The
Phase 1 spec (`2026-08-31-web-dashboard-backend-api-design.md`) documents a bearer-token
gate on write endpoints; this document supersedes that section.

## Motivation

Phase 1 built the read/write HTTP API embedded in `clauth daemon`. This phase builds the
actual browser UI that consumes it, covering all 8 TUI tabs (Overview, Usage, Tokens,
Setup, Fallback, Config, Status, Plugin) with mouse-driven controls in place of the TUI's
keyboard-only interaction.

## Tech stack

- **Alpine.js** (latest 3.x stable) for reactivity/state — no build step, no virtual DOM,
  just `x-data`/`x-show`/`x-for` attributes directly in HTML.
- **Pico.css v2** (classless) for styling — semantic HTML (`<nav>`, `<article>`, `<table>`,
  `<button>`) is styled automatically with no class soup, and it ships built-in
  light/dark theming via `data-theme`, matching a dashboard that might get glanced at any
  time of day.
- Both are vendored as single minified files, downloaded once and committed into the repo
  — never fetched from a CDN at runtime (this is a fully offline, embedded-in-the-binary
  tool).
- No npm, no webpack/vite, no `package.json`. Matches the project's existing
  `cargo build --release`-only philosophy.

## File structure and asset embedding

New directory `assets/web/`:

```
assets/web/
  index.html          — the SPA shell: nav bar, per-tab <template>/x-show sections
  app.js              — Alpine data/methods: polling, fetch wrappers, toast/error state
  app.css             — small overrides on top of Pico (global max-width wrapper, chain
                         timeline, gauge bars — the handful of things Pico has no
                         classless element for)
  vendor/
    alpine.min.js
    pico.min.css
```

Each file is pulled into the binary with `include_str!` inside `src/web/assets.rs` (new
module) and served by new `GET` routes added to `web/mod.rs`'s `route()` match:

| Route | Serves | Content-Type |
|---|---|---|
| `GET /` | `index.html` | `text/html` |
| `GET /app.js` | `app.js` | `application/javascript` |
| `GET /app.css` | `app.css` | `text/css` |
| `GET /vendor/alpine.min.js` | vendored Alpine | `application/javascript` |
| `GET /vendor/pico.min.css` | vendored Pico | `text/css` |

All five are static, open (no auth — see below), and never change at runtime, so each
handler is a one-line `(StatusCode(200), ASSET_CONST.to_string())`-shaped function; no
templating engine is introduced.

## Auth model: removed

Confirmed with user: **drop the bearer-token gate entirely.** Both read and write
endpoints are open. Rationale discussed and accepted:

- All write endpoints only accept `application/json` bodies over non-`GET` methods
  (`POST`/`PATCH`/`DELETE`). Browsers issue a CORS preflight (`OPTIONS`) before such
  "non-simple" requests, and this server implements no CORS response headers — so a
  malicious webpage open in another tab cannot successfully call these endpoints via
  `fetch`, and a plain HTML form (which skips preflight) can't either, since form
  submissions can't produce a `application/json` body that the routes' `read_json_body`
  will accept. The realistic remaining threat is a process already running as the same
  local user, which already has direct filesystem access to `~/.claude`/`~/.clauth` and
  gains comparatively little from also being able to POST to this API (account
  reordering/disabling/config edits — no credential exfiltration, since credentials were
  never exposed over this API in the first place).
- Removes the one piece of setup friction (getting a token from the daemon into the
  browser) for a tool whose whole point is frictionless local access.

Changes required to Phase 1's implementation:

- Delete `src/web/auth.rs` entirely (`load_or_create_token`, `generate_token`,
  `check_bearer`).
- `web/mod.rs`: delete `is_write_method`, `request_is_authorized`, and the 401 branch in
  `handle_request`; `spawn()` no longer takes a `token: String` parameter.
- `cli.rs`/`main.rs`: delete `Command::Web`/`WebCommand::Url` and `cmd_web` — there is no
  token to print a URL with anymore; the dashboard is just `http://127.0.0.1:47893/`.
- `daemon/mod.rs`'s `spawn_web_server` stops generating/loading a token before calling
  `web::spawn`.
- Every `tests/inline/web_*.rs` case that asserts "401 without a token" is rewritten to
  assert the operation succeeds with no `Authorization` header at all; the handful of
  `start_with(...)` test helpers that take a `TEST_TOKEN` drop that parameter.

## Architecture: single-page app

One HTML document (`index.html`). All 8 tabs live in the same page as sibling sections
toggled by Alpine's `x-show="tab === 'overview'"` — no page reloads, no per-tab routing.
Chosen over separate HTML files per tab because the tabs share a lot of global state (the
polled status payload, the toast/error UI, the current tab) that would otherwise need
duplicating across documents, and because instant tab switching (no reload) is the
closest web equivalent to the TUI's keypress-to-switch feel that motivated this whole
project.

**Global page frame**, confirmed via mockup:

- The *entire* page — nav bar included, not just the content below it — sits inside one
  centered wrapper with a max-width (~1200px) and horizontal auto margins. Below that
  width the wrapper is just `width: 100%` with some side padding. This is one wrapper
  `<div>` around everything in `index.html`, not a per-tab concern.
- Top horizontal tab bar (chosen over a left sidebar in the mockup comparison): a `<nav>`
  with the `clauth` brand + a compact "N accounts, ● active" status snippet on one side,
  and the 8 tab labels on the other. The active tab gets an underline/accent color;
  clicking a label sets Alpine's `tab` state (`x-data="{ tab: 'overview' }"` at the root,
  no URL routing).

## Data flow

- One Alpine store polls `GET /api/status` every 3 seconds via `fetch` + `setInterval`,
  writing the parsed JSON into reactive state every tab reads from. Polling pauses when
  `document.visibilityState !== 'visible'` (the `visibilitychange` event flips a flag the
  interval callback checks) and resumes immediately on refocus rather than waiting for
  the next tick.
- `GET /api/status/incidents` (Status tab) polls independently on a longer interval (30s)
  — incident feed data changes far less often than account usage.
- Every write action (switch account, edit a threshold, delete a profile, ...) is a plain
  `fetch()` call with a JSON body, awaited inline in the Alpine method the button/control
  invokes. No token header. On success: push a toast. On failure (non-2xx): set an
  inline error string in that control's local state (see below) and do **not** show a
  toast for it.
- OAuth/Alibaba login jobs: `POST` kicks off the job and gets `{job_id}` back; the
  triggering Alpine component starts its own local `setInterval` polling
  `GET /api/jobs/{id}` (independent of the global 3s status poll) and stops on
  `succeeded`/`failed`. While pending, it shows a spinner plus an elapsed-seconds counter
  computed client-side from `Date.now()` at job start — no extra backend field needed for
  this, since the job store's `Pending`/`Succeeded`/`Failed` shape from Phase 1 is
  unchanged.

## Feedback: toast + inline errors

- **Success → toast.** A small fixed-position stack (top-right) of auto-dismissing
  banners (~3s), pushed by any write call that resolves 2xx. Purely additive UI state in
  Alpine (`toasts: []`, each `{id, text}`, removed by its own `setTimeout`).
  Non-blocking, never demands acknowledgment.
- **Failure → inline, persistent.** The control that triggered the failing call renders
  the server's `error` message right next to itself (e.g. under a threshold slider, next
  to a delete button) and it stays until the user retries the action (success clears it)
  or changes the input. No toast is shown for failures — the whole point is that the
  reason has to be visible long enough to read and act on, not flash by.

## Per-tab UI, first two tabs (built and validated before the rest)

**Overview** — accounts table + fallback chain visualization, mirroring
`src/tui/render/overview.rs`'s content (not its exact layout):

- A `<table>`: status marker, account name, type badge, 5h usage (inline progress bar +
  percentage + reset countdown), 7d usage (same shape), live-session count, and a
  `switch` button per row (disabled for a broken/canceled account) that calls
  `POST /api/profiles/switch`.
- Below the table, the fallback chain as a **vertical timeline** (confirmed over a
  horizontal step-flow alternative in the mockup session): a left-side connecting line,
  each member a numbered circular badge (color = active/next-up/blocked) with its name,
  a progress bar against its threshold, and a state pill (`当前使用中` / `↩ 约 40 分钟后
  切入` / `登录已失效`, etc. — text mirrors what `blocked_reason`/the switch projection
  already compute server-side in `status.json`). A caption line below the timeline
  mirrors the TUI's stop/stay-when-all-spent line. Read-only on this tab — editing chain
  membership/order happens on the Fallback tab.

**Fallback** — master-detail, mirroring `src/tui/render/chain.rs`'s information
architecture (left list picks a member, right pane edits it), translated to native form
controls:

- Left column: the ordered chain as a list of rows (drag handle, `#n`, name), plus a
  trailing "+ 添加账号到链路" row. Clicking it turns that row into an inline `<select>`
  listing every profile not currently in the chain (from the same `GET /api/status`
  payload's profile list minus the chain); picking one immediately calls
  `PATCH /api/fallback {chain: [...current, chosen]}` (append at the end) and the row
  reverts to the "+ add" prompt. Reordering existing members calls the same
  `PATCH /api/fallback {chain: [...]}` with the full new order on drop.
- Right pane, for whichever member is selected: a labeled 5h usage bar with a threshold
  tick mark, then a form grid — `切换阈值` (range input), `周额度阈值` (range input),
  `最大自动花费` (text input, blank = uncapped), `作为最后备用` and `优先账号` (both
  `<input type="checkbox" role="switch">`, Pico's native toggle style) — and a `从链路
  移除` danger button. Every field change calls
  `PATCH /api/profiles/{name}/fallback` with just that one field in the body (matching
  the Phase 1 endpoint's per-field dispatch), so one control's edit can't accidentally
  clobber a sibling field's in-flight edit.

The remaining 6 tabs (Usage, Tokens, Setup, Config, Status, Plugin) get the same level of
per-tab design treatment in follow-up iterations once these two are validated in the
browser — see Rollout order.

## Rollout order

1. Page shell (nav bar, max-width wrapper, tab switching, asset embedding/routes,
   polling store, toast/error primitives) + **Overview** + **Fallback** tabs, fully
   working end to end. Checked in the browser against a real `clauth daemon` before
   moving on.
2. Remaining 6 tabs, one at a time, each reusing the shell/primitives from step 1.
   Content/control choices for each are made when that tab is reached, following the
   same brainstorm-a-mockup-if-genuinely-visual approach used for Overview/Fallback —
   not pre-specified in this document, to avoid speculative design for tabs not yet
   started.

## Testing

- No JS test harness is introduced (matching the zero-build-step constraint); frontend
  correctness is verified manually in a browser against a running `clauth daemon`, one
  tab at a time, before moving to the next.
- Rust-side: a new `tests/inline/web_assets.rs` asserting each static route
  (`/`, `/app.js`, `/app.css`, `/vendor/alpine.min.js`, `/vendor/pico.min.css`) returns
  200 with the expected `Content-Type` header.
- Every existing `tests/inline/web_*.rs` case written against the Phase 1 token gate is
  updated per "Auth model: removed" above; the full suite must pass with zero remaining
  references to `TEST_TOKEN`/`Authorization` headers.

## Explicitly out of scope

- Windows autostart / service packaging — Phase 3.
- Design specifics for the 6 tabs beyond Overview/Fallback — decided when each is built.
- Any accessibility/i18n work beyond what Pico.css provides by default.
- Remote/cross-device access, TLS, CORS headers — unchanged from Phase 1 (still
  `127.0.0.1`-only).
