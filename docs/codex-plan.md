# Codex harness support: implementation spec (PR #51)

The plan for adding OpenAI `codex` as a second harness alongside Claude Code. This is the spec for
whoever implements it. It is self-contained: the verified codex behavior, the pinned decisions, the
integration anchors, and the open questions are all here.

Ownership: the contributor's agent implements the whole thing (harness groundwork + codex engine) as
a reviewable series on a branch cut from the v0.14 tag. The maintainer designs and reviews. The
decisions under "Pinned decisions" are settled; do not re-open or freelance them. The "Open
questions" are the parts still to decide, most of them product calls.

## Pinned decisions (settled, do not freelance)

1. Harness is implied by WHICH STATE FILE a profile lives in, not by a field parsed out of shared
   state. Claude profiles live in `~/.clauth/profiles.toml` (`AppState`, unchanged). Codex profiles
   live in a NEW `~/.clauth/codex-profiles.toml`. A profile's harness is known from the file its name
   came from; there is no in-`AppState` harness field, no dir-name parsing, no load-order
   chicken-and-egg (the file is at a fixed path, read before any profile dir is located). A codex
   profile's `-cx` dir is derived from that known harness. `ProfileConfig.harness` may still be
   written into the per-profile `config.toml` as a self-describing marker, but FILE MEMBERSHIP is
   authoritative. `Harness { Claude, Codex }` remains the in-memory type; cross-harness conversion is
   delete + recreate.
2. Per-harness independent state, realized by the file split. `profiles.toml`/`AppState` keeps its
   exact current meaning: the CLAUDE active slot (`active_profile`), claude `fallback_chain`, claude
   `wrap_off`, claude profile list. `codex-profiles.toml` holds the codex active slot, codex chain,
   codex `wrap_off`, codex profile list. In-memory pending-switch is keyed per harness. A codex switch
   writes only `codex-profiles.toml`; a claude switch only `profiles.toml`. A codex switch never
   blocks a claude switch. Fallback chains are strictly per-harness.
3. Back-compat is trivial by construction. `profiles.toml` is UNCHANGED (same claude-only shape), so
   old and new binaries read and write it identically: no dual-write, no migration, no old-format
   fixture gymnastics, no serde-drop hazard. `codex-profiles.toml` is new; an old binary never reads
   or writes it, so it CANNOT drop or corrupt codex state (this is why the separate file exists, it
   dissolves the mixed-version write-hole class rather than patching it). An existing install has an
   all-claude `profiles.toml` and no `codex-profiles.toml` (zero codex profiles), correct with no
   migration step. During a mixed-version window an old standby daemon manages claude profiles only
   and simply ignores codex profiles until a new daemon runs. ONE residual it does not cover,
   unfixable by construction: an OLD binary creating or renaming a claude profile literally named
   `<x>-cx` (its `validate_profile_name` predates decision 4's reservation) can mkdir/write or
   `remove_dir_all` inside codex `<x>`'s dir. New binaries reject that name; an old binary in a mixed
   window cannot be stopped. Accepted residual, not a data path the new code can close.
4. Dir naming: claude bare (`profiles/<name>`), codex `-cx` (`profiles/<name>-cx`). Cross-harness
   dir-collision guard (both required): `validate_profile_name` (`actions.rs:31`, allows `-` and
   checks name-uniqueness only today) rejects a claude name ending in `-cx`; and codex create rejects
   when `profiles/<name>-cx` already exists as a dir OR a claude profile literally named `<name>-cx`
   exists. `save_profile` has no dir-exists check today (`profile.rs:1324`), so the guard lands at
   create/rename.
5. Store mode: force `-c cli_auth_credentials_store="file"` on every clauth-controlled codex spawn.
   Not "refuse unless file" (that blocks users who run keyring for their own interactive codex).
6. Single-writer refresh. Codex refresh tokens are single-use rotating, and `refresh_token_reused`
   is a PERMANENT death (browser re-login only). Route every codex rotation through the existing
   per-profile `RotationGuard` + lease, same discipline as claude. Never let two carriers hold one
   chain: refuse an isolated start when the account is the live `~/.codex` login, and refuse
   capture/switch on a profile holding a live session lease.
7. Behavior-preserving for claude, trivially: `profiles.toml`, the credential/switch path, and the
   claude usage/fallback code are untouched by the file split. The existing suite gates it.
8. Published surface is ADDITIVE, no schema bump. `status.json` stays `SCHEMA_VERSION` 1; top-level
   `active_profile` + `wrap_off` remain the claude slot (`status_json.rs:235,237`); per-harness/codex
   fields are added alongside. `which --json` + MCP `list_profiles` gain codex fields additively. An
   old standby daemon publishes the claude-only schema-1 feed; a new daemon adds codex fields; every
   existing external reader keeps working through the mixed window (matches `wiki/daemon.md`'s
   additive evolution rule).
9. Ship as a reviewable series (see "Delivery"), never one entangled diff. That is what made #51
   hard to review.

## Verified codex-0.145 behavior (source: openai/codex + live probes; file:line are `codex-rs/`)

- **CODEX_HOME** (`utils/home-dir/src/lib.rs:13-63`): if set, the path must exist (fatal `NotFound`
  otherwise) and be a directory, then it is `canonicalize()`d, fully resolving symlinks. Empty is
  treated as unset (defaults to `~/.codex`). Create the dir before spawn, and expect a symlinked
  `~/.clauth` to resolve to its real target (canonicalize both sides of any lease/identity check).
- **auth.json** (`login/src/auth/storage.rs:40-61`): `{ auth_mode?, OPENAI_API_KEY?, tokens{
  id_token, access_token, refresh_token }, last_refresh? }`, written `0600` at `$CODEX_HOME/auth.json`.
- **Store modes** (`config/src/types.rs:105-117`, `storage.rs:498-540`, key `cli_auth_credentials_store`),
  default `File`: `File` reads/writes the file; `Keyring` uses the OS keyring keyed by
  `sha256(canonical CODEX_HOME)` and ignores + deletes the file; `Auto` is keyring-first with file
  fallback and deletes the file on a keyring save; `Ephemeral` is memory-only. The isolated
  `config.toml` is a copy of the operator's, so an `auto`/`keyring` setting there makes codex ignore
  the seeded `auth.json` and, on the first refresh, write the rotated token to the keyring and delete
  the file, so adopt-back reads a stale-or-gone file and the next standby refresh replays a spent
  token into a permanently dead chain. Forcing `-c cli_auth_credentials_store="file"` fixes it
  (verified live: keyring config + a seeded `auth.json` reported `no Codex credentials were found`;
  adding the override reported `auth is configured` / `auth storage mode: File`).
- **Refresh** (`login/src/auth/manager.rs:1308-1445`): `POST https://auth.openai.com/oauth/token`,
  body `{client_id: app_EMoamEEZ73f0CkXaXp7hrann, grant_type: "refresh_token", refresh_token}`. The
  response rotates the refresh_token and `persist_tokens` writes it back. `refresh_token_reused` ->
  `Exhausted` -> `Permanent`. codex self-refreshes only when `last_refresh` is older than a
  day-scale interval. Test hooks: env `CLIENT_ID_OVERRIDE_ENV_VAR`, `REFRESH_TOKEN_URL_OVERRIDE_ENV_VAR`.
- **Usage** (`backend-client/src/client/rate_limit_resets.rs:82-83`): `GET
  https://chatgpt.com/backend-api/wham/usage`, windows duration-keyed by `limit_window_seconds`
  (not named), plan tier rides the id_token JWT claim (stale on upgrade).
- **config.toml is written by codex** (`codex mcp add` mutates it, verified live), so it must be a
  copy in the isolated home, never a symlink onto the real `~/.codex/config.toml`.
- **The isolated home fills with more than auth/config/sessions**: codex writes `goals/logs/memory/
  state` sqlite, `history.jsonl`, `models_cache.json`, `installation_id`, `log/`, `cache/`, `tmp/`.
  Adopt-back must whitelist `auth.json`, never diff the tree.
- codex refuses to plant PATH-alias helper binaries when `CODEX_HOME` is under `/var/tmp` (non-fatal
  warning). Irrelevant for `~/.clauth`, bites only if a profile ever lands under a temp path.

## Architecture

### State model: two files, disjoint profile sets
`profiles.toml` (`AppState`, claude, unchanged) and `codex-profiles.toml` (codex, new). Both are
under the same global state flock (`~/.clauth/.lock`, `with_state_lock`), so cross-file operations
serialize. There are no shared fields, so there is no cross-file consistency problem. The daemon
iterates both files and dispatches per subsystem. A codex profile named `foo` and a claude profile
named `foo` may coexist (dirs `foo-cx` vs `foo`).

### The two shared seams (a `Harness` trait)
These are the shared paths a wrong codex impl would break claude on, so seam them:
- **credential-install:** claude = `.credentials.json` link/snapshot/detach + macOS keychain mirror
  behind the `ensure_installable` gate. codex = zero-network atomic rewrite of the isolated `auth.json`.
- **runtime-spawn:** claude = `CLAUDE_CONFIG_DIR` pin + `claude_command()` + `MANAGED_ENV_KEYS`
  scrub. codex = `CODEX_HOME` pin + `codex_command()` + a codex env scrub list.

### The path layer must go harness-aware in phase 1 (else the gates no-op)
`RotationGuard::acquire` mkdir-700s + locks under bare `profile_dir(name)` (`runtime.rs:252,287`),
and `has_live_session` reads bare-name session dirs (`runtime.rs:126-130`). For a codex profile whose
real dir is `-cx`, an unmodified path layer locks in a stray bare dir and the liveness check backing
"refuse capture/switch on a live session lease" always reads idle. `profile_dir`/`profile_subpath`
must resolve `-cx` from the known harness (file membership) in phase 1, and the same edit covers the
create/delete/rename dir moves.

### Inline `match harness` (not seamed into the trait; lower claude-regression risk)
- usage-fetch fan-out (the `NamedEntry` legs): codex gets a third leg over the codex profile set.
- fallback chain-member (`ChainMember`/`ChainSnapshot`): codex quota semantics feed the reused `walk_chain`.
- `which.rs` session->profile resolution: a codex arm off `CODEX_HOME`.
- sync-skip: `settings_sync`/`claude_json`/session-index/runtime-GC/`live_isolated_stores` operate on
  claude profiles only. They already walk `profiles.toml`/the profiles root; they skip codex profiles
  (resolved via `codex-profiles.toml` membership, with the `-cx` dir shape as a fast-path guard,
  never as the authoritative harness).

### The codex engine
- **Isolated home:** `CODEX_HOME` -> `profiles/<name>-cx/codex-home/`. Create + canonicalize before
  spawn. Contents: real `auth.json` (seeded from the store at acquire), a COPY of `config.toml`,
  codex-created `sessions/` + sqlite state. Scrub an inherited `CODEX_HOME`. Force
  `-c cli_auth_credentials_store="file"`.
- **Switch:** local atomic rewrite of the target `auth.json`. Session-boundary: a running codex holds
  its in-memory token until next start, so surface that in the TUI (a switch is not a no-op).
- **Refresh wire:** as verified above; standby-refresh parked profiles only, the live one belongs to
  the CLI.
- **Adopt-back:** while a session runs, the isolated home is the chain carrier. One-way
  `codex-home/auth.json` -> store on the watchdog + a final sync on drop. Whitelist `auth.json`.

## Integration anchors (current `mommy` tree, approximate line positions, re-verify before editing)
- `AppState` `src/profile.rs` ~326 stays the CLAUDE state (`active_profile` ~328, `fallback_chain`
  ~331, `profiles` ~329 bare claude names). It is written wholesale by `save_app_state` ~1170. The
  codex state gets a PARALLEL type + `save_codex_state` into `codex-profiles.toml`; the existing
  claude writers are untouched.
- `Profile` `src/profile.rs` ~145 is `derive(Debug, Clone)`, NOT serde; built by `load_profile` ~1189
  from the per-profile `config.toml` via `ProfileConfig` ~770 (`load_profile` NotFound ->
  `ProfileConfig::default()`). Codex profiles load the same way, from `<name>-cx/config.toml`.
- `profile_dir(name)` `src/profile.rs` ~910 = `profiles_root()?.join(name)`, no suffix. Must resolve
  `-cx` for codex. `profile_subpath` `src/runtime.rs` ~114.
- `active_profile` (CLAUDE slot) has ELEVEN write sites, all staying claude-scoped: `src/actions.rs`
  346/366/791/827/898/968, `src/profile.rs` 702/736, `src/tui/app.rs` 6552/6579/6952 (`tui/app.rs:1914`
  is a read binding). Codex switching writes `codex-profiles.toml` via parallel code, so these sites
  are largely unchanged; end any edit with a grep sweep.
- Credential install: `link_profile_credentials`/`force_link_profile_credentials` `src/claude.rs`
  ~445/~920. Pre-install gate: `oauth::ensure_installable` `src/oauth.rs` ~1260.
- runtime: `ProfileRuntime::acquire` `src/runtime.rs` ~334; env pin
  `command.env("CLAUDE_CONFIG_DIR", runtime.config_dir())` `src/start.rs` ~131; `claude_command()`
  `src/runtime.rs` ~574; `scrub_profile_env` + `MANAGED_ENV_KEYS` ~517-533. `RotationGuard` ~276 +
  `rotation_lock_path` ~252, `has_live_session` ~126-130, GC + `live_isolated_stores` ~177-230, all
  keyed off bare `profile_dir`.
- fallback: `walk_chain(idx,len,skip_pred,accept_pred)` `src/fallback.rs` ~962 is reusable as-is;
  `ChainMember` ~740 and `ChainSnapshot` ~766 carry claude usage-window fields.
- pending switch: `PendingSwitch = Arc<RankedMutex<HashSet<String>, rank::PendingSwitch>>`
  `src/usage/scheduler.rs` ~158; drained `src/daemon/tick.rs` ~83.
- usage fan-out: `TokenEntry` ~166 + `ThirdPartyEntry` ~183 via `NamedEntry` ~191 in
  `src/usage/scheduler.rs`; two-leg fan-out in `fn tick` ~2199-2211. `UsageInfo` `src/usage/fetch.rs` ~369.
- `RotationGuard` keyed by profile NAME; `with_state_lock` `src/lock.rs` ~208; lock ranks
  `src/lockorder.rs`.
- `settings_sync::sync_once` + `claude_json::sync_once` run in the per-session watchdog
  (`src/runtime.rs` ~405 + Drop ~476), NOT daemon-per-tick, and reconcile ALL profiles cross-profile.
  `known_paths()` defs `src/settings_sync.rs` ~146 / `src/claude_json.rs` ~64 (call sites ~222/~81).
- `validate_profile_name` `src/actions.rs` ~31 (single chokepoint: CLI create `main.rs:408`, TUI
  create/rename `tui/app.rs:5550,6334,6912,6242`). `save_profile` `src/profile.rs` ~1324 has no
  dir-exists check. `delete_profile` removes the dir before state (`actions.rs:553-561`).
- daemon feed: `status_json.rs` ~232-240 publishes `active_profile` + `wrap_off` + `pending_switch`
  under `SCHEMA_VERSION` (=1, `status_json.rs:26`); the only in-binary reader is `probe.rs:98-101`
  (untyped `Value`, reads `generated_at`, ignores `schema`). `which.rs`
  `session_profile_from_config_dir` assumes the `CLAUDE_CONFIG_DIR`/runtime shape.
- hot reload: `ReloadFingerprint`/`reload_fingerprint` (`src/profile.rs:858-900`) stats `profiles.toml`
  + each dir's `config.toml`/`session-token.json` mtimes; it does NOT include `codex-profiles.toml`,
  so a codex-only state edit (a codex switch or chain reorder) never shifts the fingerprint and the
  daemon acts on a stale codex slot. Add a `codex-profiles.toml` mtime trigger in phase 1.
- perms walk: `enforce_clauth_perms` (`src/profile.rs:1107`) recurses `~/.clauth` chmod-only to 0600
  on every entry point of BOTH binaries, which strips the exec bit off the PATH-alias helper binaries
  codex plants under `codex-home/`. Needs a codex-home exemption (phase 3), and an old binary
  re-breaks them mixed-window.
- Clean slate: zero `harness`/`codex` tokens in `src/` on `mommy` today.

## Parity map

| clauth feature | codex disposition |
|---|---|
| account switch | port (auth.json rewrite, session-boundary) |
| fallback chain + walk | port (`walk_chain` reused, codex members) |
| standby refresh | port, single-writer + permanent-death discipline |
| usage tracking | codex-specific (JSONL + `wham/usage`, duration windows) |
| isolated `start` | port (`CODEX_HOME` pin, forced file store) |
| `clauth which` | port (codex arm off `CODEX_HOME`) |
| settings sync / `.claude.json` sync | SKIP codex (meaningless against `CODEX_HOME`) |
| sessions index / rescue | SKIP or codex-specific (codex owns `sessions/`) |
| Tokens tab (`tokens.rs`/`token_ledger`) | open (Q9): codex sessions feed it or excluded |
| daemon `status.json` / `which --json` / `list_profiles` | ADDITIVE codex fields, schema stays 1 (decision 8) |
| MCP `delegate` | port via `codex exec` (open, Q1) |
| macOS keychain mirror | N/A for codex (forced file mode) |
| `clauth proxy` in-session fallback | out of scope, separate feature (Q6) |

## Gotchas
- The gate is `cargo.sh` (fmt -> clippy `-D warnings` -> nextest -> doctests -> deny/audit). Green
  predicts CI.
- The two state files are independent (disjoint profile sets, same flock). An old binary only ever
  touches `profiles.toml`; it cannot see or corrupt `codex-profiles.toml`. That is the whole point of
  the split, do not reintroduce codex fields into `AppState`.
- Lock order: the codex-state mutex reuses the existing `RankedMutex` ranks (`src/lockorder.rs`), no
  new rank, no inversion; both files sit under `with_state_lock`.
- Mechanical-sweep discipline: grep-verify every `active_profile`/`fallback_chain` occurrence before
  and after each edit; a `format!`-built name defeats symbol grep. The claude `active_profile` has 11
  write sites (unchanged); the codex active slot adds its own writers in the new file.

## Delivery (reviewable series)
1. harness axis: `Harness` enum; the `codex-profiles.toml` state type + its load/save + codex profile
   CRUD; `-cx` naming + the `validate_profile_name` reservation + the codex-create dir-collision
   guard; the path layer (`profile_dir`/`RotationGuard`/`has_live_session`/GC) resolving `-cx` from
   the known harness; `which.rs` dispatch; sync-skip guards; the `codex-profiles.toml` mtime added to
   `reload_fingerprint`. `profiles.toml` and every claude path untouched. No codex-engine code. Pin
   the codex CRUD/switch/delete/rename CLI grammar (Q8) before this phase's CLI surface, since dup
   names across harnesses are legal and only bare `switch` has a rule today.
2. the two `Harness` trait seams (credential-install + runtime-spawn) with claude behind them, a pure
   behavior-preserving refactor.
3. codex credential + isolated start: seed `auth.json`, `CODEX_HOME` pin, force file mode, copy
   config, refuse a live-account start; exempt `codex-home/` from `enforce_clauth_perms` so codex's
   helper binaries keep their exec bit. (gated on Q2)
4. codex refresh wire + adopt-back under the single-writer discipline.
5. codex usage leg (JSONL + `wham/usage`) + fallback member. (gated on Q3)
6. published-surface + parity: additive codex fields in `status.json`/`which --json`/`list_profiles`;
   the `c codex`/`c claude` view filter + header chip; `delegate` via `codex exec`. (gated on Q1)

## Open questions (to decide)
1. **`delegate` / `clauth start {codex}`:** interactive `codex` vs `codex exec` for the headless
   delegate path; the MCP `delegate` tool must branch on harness (prompt/format differ from `claude -p`).
2. **Shared config in the isolated home:** does a codex profile share the operator's global codex
   `skills/`/`rules/`/MCP servers (symlink the read-only bits, like claude runtime symlinks
   `~/.claude/skills`), or start bare? `config.toml` must be a copy; the rest is a product call.
3. **Usage model mapping:** codex duration-keyed windows vs clauth's named `five_hour`/`seven_day`/
   `weekly_scoped`. Reuse the model or a codex-specific shape, and how does the TUI render them?
4. **Auto-start / kick:** claude opens a 5h window via a kick before fetching. Codex usage is passive
   (no kick endpoint), so does auto-start apply to codex, or is it claude-only?
5. **Plan-tier staleness:** the id_token plan claim goes stale on a plan upgrade. How does clauth
   detect and refresh the tier?
6. **`clauth proxy`:** the fork's localhost injection proxy for in-flight fallback. In scope here or
   deferred as a standalone feature?
7. **codex login:** reuse `codex login` (shell out) vs reimplement browser PKCE. `codex login
   --with-access-token` reads a token from stdin but not a full chain.
8. **Cross-harness CLI ergonomics:** `clauth switch <name>` disambiguates by which file holds the
   name (and errors if a name exists in both harnesses); confirm the `c codex`/`c claude` filter +
   header chip grammar.
9. **Tokens tab:** `tokens.rs`/`token_ledger` read claude-shaped session JSONL. Do codex sessions
   feed the Tokens tab (cost lens over codex usage) or are they excluded?
