# Web dashboard — Phase 1: embedded HTTP API design

## Status

**Phase 1 implemented and merged to `main` (2026-08-31).** Design approved by user
(verbal walkthrough, 2026-08-31), then built incrementally across several commits, one
per tab. Two follow-on phases are out of scope for this document and get their own
spec/plan cycle later:

- **Phase 2** — the actual web frontend (static assets + JS) consuming this API, covering
  all 8 TUI tabs.
- **Phase 3** — Windows autostart (Task Scheduler entry or a real Windows Service) so
  `clauth daemon` (with the web server embedded) comes up at boot with no terminal window.

### Deviations from this design, found during implementation

- **Login jobs are NOT `mcp/jobs.rs`.** This document originally said the OAuth/Alibaba
  async login jobs would reuse the MCP `delegate` tool's disk-persisted job store. On
  closer reading of `mcp/jobs.rs` while implementing, that store turned out to be shaped
  entirely around a spawned `claude` subprocess (pid, heartbeat, streamed output, session
  id) — none of which a login flow has. Built a small in-memory job store instead
  (`web::jobs`: `start`/`finish`/`poll` over a `HashMap<String, JobStatus>`), scoped to
  the daemon process's lifetime. Ranked as a new standalone leaf in `lockorder.rs`
  (`WebJobs = 1900`).
- **Config tab has no shared setter with the TUI.** The Fallback tab extraction (below)
  worked because the TUI's fallback keystrokes and the new endpoints both needed "set this
  field and persist." The Config tab's TUI keystrokes each compute a cycle/step "next
  value" first (`cycle_theme`, `step_refresh_interval`, …) — presentation logic with
  nothing for the web API (which receives an exact end state from a form) to share. Wrote
  `actions::apply_config_patch` fresh instead of extracting from `tui/app.rs`.
- **Setup's `PATCH /api/profiles/{name}` doesn't cover model routing or rename.** Model
  settings (`edit_profile_model`) and `rename_profile` (which needs a `RotationGuard`
  acquired before the config lock, like `delete_profile`) were left out of the first cut
  to keep the endpoint's scope manageable — straightforward to add the same way the
  existing fields were, when a frontend actually needs them.
- **`plugin_host::install`/`self_heal` have no HTTP-level test.** Both shell out to the
  real `claude` binary via `agentgear`; the sandboxed harness for that
  (`testutil::FakeClaude`) is Unix-only (a PATH-shimmed shell script), so a cross-platform
  test here would either skip on Windows or risk touching a real Claude Code install.
  `GET /api/plugin/status` (pure reads) is tested; the two write endpoints are one-line
  wrappers over functions `plugin_host`'s own test suite already covers.

### Superseded by Phase 2

The "Auth model" section below (bearer token gating write endpoints) was **removed
during Phase 2 design** — see `2026-08-31-web-dashboard-frontend-design.md`'s "Auth
model: removed" section for the rationale and the resulting code changes
(`src/web/auth.rs` deleted, `clauth web url` command deleted, etc.). Left in place here
as a historical record of Phase 1's original design.

## Motivation

The TUI (`clauth`) is keyboard-only — no mouse support anywhere in `src/tui/` today (no
`crossterm::event::EnableMouseCapture`, no `MouseEvent` handling). The user wants a
browser-based dashboard purely for local use on the same machine (not remote/cross-device
access), to get mouse-driven tab switching and clicks instead of memorizing keybindings,
and to eventually let this run unattended in the background from boot instead of keeping a
terminal window open.

Given `clauth daemon` already exists as a headless background process (single-instance
flock, watchdog, `status.json` publishing every tick), the natural home for the web server
is inside `daemon`, not a separate process — one background process at boot does both the
existing refresh/auto-switch loop and now also serves the dashboard.

## Architecture

- The HTTP server runs embedded inside `clauth daemon` (not a new top-level subcommand
  running standalone). Boot → one process → both jobs.
- Binds to `127.0.0.1` only, on a fixed default port (`47893`, overridable via
  `~/.clauth/profiles.toml` or an env var — exact key TBD at implementation time), never
  `0.0.0.0`. Not intended for remote/cross-device access (explicitly out of scope per the
  user's stated goal).
- HTTP library: **`tiny_http`** (synchronous, minimal dependency footprint), not `axum`.
  Rationale: this codebase is almost entirely synchronous (`ureq` for the one HTTP client
  it already has); the only async runtime in the tree today is the sliver of `tokio` pulled
  in by `rmcp` for the MCP stdio server. Pulling in `axum` would mean a second, much larger
  async surface for a purely local, low-throughput API. `tiny_http` fits the existing
  style and keeps the dependency addition small and easy to justify (matching this
  project's existing practice of a documented rationale per new `Cargo.toml` entry).
- The read side reuses `daemon::status_json::build_status` as-is — no new state-snapshot
  logic; the JSON schema/shape already documented in `wiki/Daemon.md` is the contract for
  `GET /api/status` too.
- No browser auto-launch on daemon start (confirmed with user — daemon starts silently at
  boot; the user opens the dashboard themselves via a bookmarked URL when they want it).

## Auth model

Confirmed with user: **read endpoints are open (no auth), write endpoints require a
bearer token.** Rationale discussed and agreed: the API never carries a token/secret/key
(same invariant `status.json` already holds), so read access leaking account names/usage
percentages to another local process is low-value. But mutation endpoints (switch active
profile, delete a profile, change fallback thresholds, etc.) let any local process quietly
manipulate account state without needing to know a secret — that's a real risk distinct
from credential leakage, so those need a gate.

- On first start (or if missing), daemon generates a random token and persists it to
  `~/.clauth/web_token` (0600 permissions, same treatment as credential files). Stable
  across restarts — not regenerated every boot — so a bookmarked URL keeps working
  indefinitely unless the user explicitly rotates it.
- New CLI helper: `clauth web url` prints the full bookmarkable URL with the token
  embedded as a query param, e.g. `http://127.0.0.1:47893/?token=<token>`. The frontend
  JS reads the token from the query string on first load and persists it in
  `localStorage` so subsequent write calls attach it automatically (`Authorization:
  Bearer <token>` header) with no repeated prompt.
- Write endpoints reject requests without a valid `Authorization: Bearer <token>` header
  (401). Read endpoints ignore the header entirely.
- A token-rotation path (`clauth web rotate-token` or similar) is a nice-to-have, not
  required for Phase 1's first cut — note it here so it isn't forgotten, but it can land
  later without changing the API shape.

## Full endpoint inventory, by TUI tab

Backing logic: **Overview and Setup already have their mutations centralized in
`actions.rs`** — the API for these tabs is a thin HTTP wrapper over existing functions,
no refactor needed. **Fallback and Config currently mutate `AppState`/`Profile` fields
inline inside `tui/app.rs`**, with no reusable non-TUI entry point — seeded refactor below.

| Tab | Endpoint | Auth | Backing logic |
|---|---|---|---|
| Overview | `GET /api/status` | open | `daemon::status_json::build_status` |
| Overview | `POST /api/profiles/switch` `{name}` | token | `actions::switch_profile` |
| Overview | `POST /api/profiles/reorder` `{from, to}` | token | `actions::reorder_profile` |
| Usage | `GET /api/status` (same payload; usage windows are part of it) | open | — |
| Tokens | `GET /api/status` (token/cost fields folded into the same payload, or a small `GET /api/tokens` if the payload gets too large — decide at implementation time) | open | `tokens.rs` / `pricing.rs` read paths |
| Status | `GET /api/status/incidents` | open | `status.rs` cached incident feed |
| Setup | `POST /api/profiles` `{name, base_url?, api_key?, model?}` | token | `actions::create_blank_profile` |
| Setup | `DELETE /api/profiles/{name}` | token | `actions::delete_profile` |
| Setup | `PATCH /api/profiles/{name}` `{base_url?, api_key?, env?, model?, name?(rename), disabled?}` | token | `actions::edit_profile_endpoint` / `edit_profile_env` / `edit_profile_model` / `rename_profile` / `enable_profile` / `disable_profile` (dispatched per field present in the body) |
| Setup | `POST /api/profiles/{name}/login/oauth` | token | `oauth_login::login_with`, wrapped as an async job (see below) |
| Setup | `POST /api/profiles/{name}/login/alibaba` `{site, region}` | token | `alibaba_login::login_with`, async job |
| Setup | `GET /api/jobs/{job_id}` | token | job status poll, reusing `mcp/jobs.rs`'s existing persisted-job store |
| Fallback | `PATCH /api/fallback` `{chain: [name, ...]}` | token | **new**: `actions::reorder_fallback_chain` / add / remove (extracted from `app.rs`) |
| Fallback | `PATCH /api/profiles/{name}/fallback` `{threshold?, weekly_threshold?, max_auto_spend?, preferred?, last_resort?}` | token | **new**: `actions::set_fallback_threshold` / `set_weekly_threshold` / `set_max_auto_spend` / `toggle_preferred` / `toggle_last_resort` (extracted from `app.rs`) |
| Config | `PATCH /api/config` `{theme?, reset_display?, clock_format?, refresh_interval_ms?, burn_aware_switching?, ...}` (partial patch, one endpoint for all ~13 `GlobalConfigRow` fields) | token | **new**: `actions::apply_config_patch` (extracted from `app.rs`'s per-row cycle functions, split into "compute next value" (stays keyboard-only) vs "set + persist given value" (shared)) |
| Plugin | `GET /api/plugin/status` | open | `plugin_probe` (read-only) |
| Plugin | `POST /api/plugin/install` | token | `plugin_host::install()` |
| Plugin | `POST /api/plugin/self-heal` | token | `plugin_host` self-heal entry point |

## OAuth / browser-flow logins: async job pattern

`oauth_login::login_with` and `alibaba_login::login_with` open the system's default
browser and block on a local loopback callback (an ephemeral, separately-allocated port —
unrelated to and never conflicting with the dashboard's own fixed listening port) until
the user finishes signing in, which can take anywhere from seconds to minutes. An HTTP
request cannot block for that long, so these two operations are **fire-and-poll**, not
request/response:

1. `POST .../login/oauth` (or `/login/alibaba`) kicks off the flow on a background thread
   and returns `{job_id}` immediately.
2. The frontend polls `GET /api/jobs/{job_id}` until status is `succeeded` or `failed`.
3. Reuses the existing `mcp/jobs.rs` persisted job store (already built for the MCP
   `delegate`/`monitor` tools) rather than inventing a second job-tracking mechanism.

Every other write endpoint (API-key logins, setup-token logins, all of Setup/Fallback/
Config/Overview) is a plain synchronous request/response — no job wrapping needed, since
none of them block on external user interaction.

Login itself (adding/re-authenticating a Claude account into clauth) is unrelated to
dashboard access auth (the bearer token in the section above) — conflating the two was a
point of confusion during design and is called out here explicitly so the eventual
frontend copy doesn't reintroduce it: opening/browsing the dashboard never requires an
Anthropic sign-in; only the deliberate "add a new account" action does, exactly as today
in the TUI.

## Refactor: extracting Fallback/Config mutations out of `tui/app.rs`

Today each `GlobalConfigRow`/fallback-detail keypress has one function in `app.rs` that
both computes the next value (cycle to next enum variant, step a threshold by ±5, etc.)
*and* persists it. The web API needs "set to this exact value and persist," which is a
different shape (a dropdown or checkbox in a browser hands over a final value, not "give
me the next one in the cycle").

The refactor splits these in two:

- **Keyboard-specific "what's the next value" logic stays in `app.rs`** — unchanged
  behavior, just calls into the new shared setter below instead of writing the field and
  calling `save_app_state`/`save_profile` itself.
- **New shared "set + persist" functions move into `actions.rs`**, one per field/group
  (listed in the table above). Pure move-and-split, no behavior change for the TUI path —
  low risk, mechanically verifiable against existing TUI tests.

## Testing

- Existing TUI test suite (`tests/inline/*.rs`, the `demo_data_drives_all_actions`-style
  driver) must keep passing unchanged after the `app.rs` → `actions.rs` extraction — that
  suite is the regression guard for "the refactor didn't change TUI behavior."
- New tests for the HTTP layer: spin up the `tiny_http` server against a `HomeSandbox`
  temp dir (same pattern `showcase.rs`/`ShowcaseHome` already uses to redirect
  `~/.clauth`/`~/.claude`), issue real HTTP requests via `ureq` (already a dependency) from
  the test, assert status codes and JSON bodies. Auth tests: confirm read endpoints work
  with no header, write endpoints 401 without a valid bearer token and succeed with one.
- Job-polling tests: start an OAuth login job against a stubbed/mocked login backend (mirroring
  however `oauth_login`'s existing tests already fake the browser/callback round trip),
  poll `/api/jobs/{id}` and assert the state machine (`pending` → `succeeded`/`failed`).

## Explicitly out of scope for this phase

- The actual web frontend (HTML/CSS/JS) — Phase 2.
- Windows autostart / service packaging — Phase 3.
- Token rotation UX polish (a manual rotate command is enough for v1).
- Remote/cross-device access, TLS, or any bind address other than `127.0.0.1`.
- Config-file key name for the port override (left as a TBD to settle during
  implementation, not a design-level decision).
