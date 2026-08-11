# Configuration

Two files, both TOML, both safe to hand-edit while clauth runs (it reloads on external change):

- `~/.clauth/profiles.toml` for everything program-wide: profile order, the active marker, the fallback chain, appearance, the scheduler.
- `~/.clauth/profiles/<name>/config.toml` for one account: endpoint, key, env, model routing, its chain settings.

Every key below has a TUI equivalent on the Setup, Fallback, or Config tab ([Interface and keys](Interface-And-Keys#config-tab-rows)).

## Account types

**Claude Pro / Max / Team / Enterprise.** Leave `base_url` blank. clauth captures the OAuth token from your session or mints one through `clauth login`, then detects the plan tier from Anthropic's profile endpoint.

**API endpoint.** Set `base_url`, and `api_key` if the endpoint wants one. Works against the Anthropic API or any compatible proxy. The key is handed to Claude Code through `apiKeyHelper` rather than written into `settings.json`.

**Long-lived setup token.** `clauth login <name> --setup-token` stores a `claude setup-token` mint as `session-token.json`. Sessions run on that static login, which never races clauth's token refresher. The Setup tab then shows a `token` row counting down to the re-mint.

The token outranks the profile's OAuth pair at every switch for as long as it exists, so a later `clauth login <name>` updates only what clauth polls usage with. `clauth static-token <name> --clear` drops it and puts the OAuth login back in front of sessions.

A mint is a narrower credential than a `/login` session: it carries `user:inference` and `user:sessions:claude_code` and no refresh token, against the five scopes a browser login stores. Claude Code turns off anything gated on the wider set, Claude in Chrome by name. Clear the token if you want those features back.

### Third-party usage data

Two providers get typed usage panels:

| Provider | Base URL | Shows |
|----------|----------|-------|
| DeepSeek | `https://api.deepseek.com` | balance rows per currency: total, granted, topped up |
| Z.ai | `https://api.z.ai` | percentage bars per limit window (5h / 7d / 30d), per-tool rows, plan level, 7-day per-model token totals |

Any other endpoint is scanned best-effort: clauth probes a short list of usage paths on the origin your key already authorizes, and renders whatever percentage or balance shapes come back. Those panels carry a "looks wrong? report it" line, since the shape is guessed. An endpoint that returns nothing usable stops being polled until you press <kbd>r</kbd>.

## Model routing

Per account, on the Setup tab or in `config.toml`:

```toml
[models]
default  = "opusplan"                     # preset alias or a full model id
opus     = "claude-opus-4-5-20251101"     # ANTHROPIC_DEFAULT_OPUS_MODEL
sonnet   = "claude-sonnet-4-5-20250929"   # ANTHROPIC_DEFAULT_SONNET_MODEL
haiku    = "claude-haiku-4-5-20251001"    # ANTHROPIC_DEFAULT_HAIKU_MODEL
fable    = "claude-fable-5"               # ANTHROPIC_DEFAULT_FABLE_MODEL
subagent = "claude-sonnet-4-5-20250929"   # CLAUDE_CODE_SUBAGENT_MODEL
```

`default` lands as the top-level `model` key in `settings.json`; the rest ride in its `env` block. A switch or a `clauth start` applies whichever account you land on.

## Presets

A preset is a named `base_url` + `[models]` pair you can stamp onto any account from the Setup tab's <kbd>a</kbd> menu. Two ship built in, `DeepSeek` and `Z.ai`, each setting the endpoint and a base model; the tier rows stay yours to pin afterwards.

`save as preset` stores the focused account's own endpoint and models under a name you type, in `~/.clauth/presets/<name>.json`:

```json
{ "base_url": "https://api.example/anthropic", "models": { "default": "my-model" } }
```

`apply preset` opens the picker, built-ins first. Applying replaces the account's endpoint and its whole `[models]` block, so a tier the preset leaves unset is cleared rather than kept; the picker warns and names the fields first when the account already carries any. The account's own api key is never touched, and a preset never carries one. <kbd>d</kbd> in the picker deletes a saved preset; the built-ins have no file and stay.

## Auto-start the 5-hour window

The 5h window opens on a real inference call. clauth's own token refresh does not trip it, so an account can read 0% while the clock has yet to start.

```toml
auto_start = true    # per profile; older spelling kick_timer still reads
```

clauth then sends a 1-token Haiku ping on launch and on each refresh tick while no window is running. On a cold start it fetches usage before the first ping, so it never fires over a window that might already be live. That costs a fraction of a cent and it is a real billed `/v1/messages` call under your own token. Default off, OAuth accounts only.

If the messages limiter is blocking Claude Code, a live 5h window will not clear it. clauth re-tests with the same ping on the poll cadence and can rotate the chain around an account whose ping keeps getting rejected.

## `profiles.toml`

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `active_profile` | string | none | the account currently linked into `~/.claude` |
| `profiles` | list | `[]` | display order |
| `fallback_chain` | list | `[]` | ordered chain members ([Auto-switch](Auto-Switch)) |
| `refresh_interval_ms` | int | `90000` | usage poll cadence, 10 s to 1 h |
| `refresh_spent_accounts` | bool | `true` | keep polling accounts at 100% |
| `preemptive_rotation` | bool | `true` | rotate OAuth ahead of expiry; `false` waits for a rejection |
| `weekly_switch_threshold` | float | `98.0` | chain-wide 7d exhaustion line, 50-100 |
| `burn_aware_switching` | bool | `false` | project usage forward instead of comparing to the threshold |
| `burn_switch_floor_pct` | float | `98.0` | earliest point burn-aware may switch, 90-100 |
| `burn_horizon_cap_ms` | int | `60000` | how far ahead burn-aware projects |
| `wrap_off` | bool | `false` | switch off all accounts once the chain is out of quota |
| `spend_budget_switching` | bool | `false` | master switch for pay-as-you-go fallback |
| `switch_off_when_budget_spent` | bool | `true` | switch off once the spend ceiling is used up |
| `auto_rescue` | bool | `false` | lift an isolated run's transcripts into the global store before teardown |
| `default_divergence` | string | none | auto-resolve a credential mismatch: `Overwrite`, `NewProfile`, `Discard` |
| `theme` | string | auto | `full` or `compatible` |
| `reset_display` | string | `relative` | `relative`, `clock`, `both` |
| `clock_format` | string | `24h` | `24h` or `12h` |
| `show_estimates` | bool | `true` | burn estimates on the Usage tab |
| `show_pace` | bool | `false` | ideal-pace marker on usage bars |
| `count_cache` | bool | `false` | count cache tokens in the Tokens totals |
| `auth_broken` | list | `[]` | accounts quarantined after a permanent OAuth rejection; clauth writes this |

## `config.toml`

| Key | Type | Default | Controls |
|-----|------|---------|----------|
| `base_url` | string | none | API endpoint; unset means an OAuth account |
| `api_key` | string | none | key for that endpoint |
| `auto_start` | bool | `false` | the 1-token window-opening ping (alias `kick_timer`) |
| `disabled` | bool | `false` | hide from auto-switch, polling, and the status feed |
| `fallback_threshold` | float | `95.0` | 5h utilization % that switches away from this account |
| `weekly_threshold` | float | chain value | per-account override of the 7d line, 50-100 |
| `check_weekly` | bool | `true` | count the aggregate weekly window against this account |
| `check_scoped` | bool | `true` | count per-model weekly windows against this account |
| `last_resort` | bool | `false` | the chain's parking spot |
| `preferred` | bool | `false` | the home account clauth returns to once it is clear |
| `max_auto_spend` | float | `0.0` | dollar ceiling on pay-as-you-go fallback |
| `bell_threshold` | float | none | 5h % that fires a bell toast |
| `[env]` | table | `{}` | extra environment variables merged into `settings.json` while active |
| `[models]` | table | `{}` | `default`, `opus`, `sonnet`, `haiku`, `fable`, `subagent` |

`last_resort` and `preferred` are radio toggles across the chain: marking one clears it everywhere else, and no account can be both.

## Storage layout

```
~/.clauth/
  profiles.toml            # everything in the table above
  price_cache.json         # LiteLLM model prices for the cost lens
  status_cache.json        # status.claude.com incident feed
  status.json              # the daemon's published snapshot (see Daemon)
  clauth.log, daemon.log   # event lines from the TUI and the daemon
  completions/             # generated shell completion scripts
  jobs/<id>.json           # backgrounded delegate jobs, GC'd after an hour
  live_sessions/<sid>.json # one row per live `clauth start` session
  profiles/
    work/
      config.toml          # everything in the table above
      credentials.json     # OAuth snapshot (.pending while a rotation is mid-write)
      session-token.json   # long-lived setup-token login, when captured
      usage_cache.json     # last-known utilization and plan
      usage_history.jsonl  # 2 days of samples, feeding burn-aware switching
      third_party_cache.json
      account_id.json      # which account this is, so a re-login can be told apart
      profile_fetched.json # when the plan tier was last read
      kick_block.json      # messages-limiter block state
      throughput_cache.json# observed delegate tokens/sec per model
      runtime-<sid>/       # one CLAUDE_CONFIG_DIR tree per live session
      runtime-isolated-<sid>/
      sessions-<sid>/      # that session's PID file, flock-held while it runs
```

Lock files (`.lock`, `clauthd.lock`, `usage-fetch.lock`) sit alongside. Everything clauth owns is `0600`, every directory `0700`, re-tightened on each launch.

Deleting any `*_cache.json`, `usage_history.jsonl`, or `status.json` costs you history and nothing else. Deleting `credentials.json` or `session-token.json` signs that profile out.

The `-<sid>` suffix appears only where the OS grants symlinks. Windows without symlink privilege builds the runtime tree by copying `~/.claude/`, so every session of one profile shares a single unsuffixed `runtime/` instead of paying for a copy each.
