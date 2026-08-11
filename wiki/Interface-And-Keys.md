# Interface and keys

`clauth` opens on the Overview tab. <kbd>←</kbd> <kbd>→</kbd> move between tabs, <kbd>?</kbd> lists every binding for the tab you are on, <kbd>q</kbd> twice quits.

## Tabs

| Tab | Holds | You can |
|-----|-------|---------|
| **Overview** | account table, live 5h / 7d bars, chain position | switch accounts, reorder them |
| **Usage** | per-account window breakdown: 5h, 7d, per-model weeks, extra-usage spend, endpoint, merged env | refresh one account, toggle estimates and the pace marker |
| **Tokens** | global Claude Code token stats and API-equivalent cost | drill into models, change the period lens, count cache tokens |
| **Setup** | per-account endpoint, key, env, model routing, auto-start | edit any of it, log in, log out, disable, delete |
| **Fallback** | the auto-switch chain | reorder members, edit thresholds, flip gates, set a spend ceiling |
| **Config** | program-wide settings | change any of the rows in the table below |
| **Status** | incidents from status.claude.com with per-component health | open an incident's timeline or its page in a browser |
| **Plugin** | Claude Code wiring health and per-profile runtime state | apply one-key fixes |

The active account is orange. Usage numbers are cached on disk, so they stay on screen when the API is rate-limited or unreachable.

## Keys

### Everywhere

| Key | Action |
|-----|--------|
| <kbd>←</kbd> <kbd>→</kbd> (<kbd>tab</kbd> / <kbd>⇧tab</kbd> at the top level) | previous / next tab |
| <kbd>↑</kbd> <kbd>↓</kbd> | move the selection, or scroll a detail pane |
| <kbd>⏎</kbd> | act on the selected row (see below) |
| <kbd>n</kbd> | new account |
| <kbd>d</kbd> | open the divergence resolver, when one is pending |
| <kbd>x</kbd> | dismiss the newest toast, then the footer alert |
| <kbd>a</kbd> | action menu for the current row |
| <kbd>?</kbd> | keybinding help for this tab |
| <kbd>esc</kbd> | step back out of a sub-pane |
| <kbd>q</kbd> | step back, or arm quit at the top level; press again to confirm |
| <kbd>ctrl</kbd>+<kbd>c</kbd> | quit from anywhere |

### Tab-dependent

| Key | Behavior |
|-----|----------|
| <kbd>r</kbd> | Usage: refresh the selected account only. Tokens / Status / Plugin: reload that tab's data. Everywhere else: refresh every account |
| <kbd>t</kbd> | Tokens: cycle the period lens. Everywhere else: force-rotate every account's token, after a confirm |
| <kbd>⏎</kbd> | Overview: switch to the selected account. Tokens: open the model breakdown. Setup / Fallback: open a detail row, or commit an edit. Status / Plugin: open the detail |
| <kbd>⇧↑</kbd> <kbd>⇧↓</kbd> | Overview: reorder accounts. Fallback (chain focus): reorder chain members |
| <kbd>space</kbd> | Config: cycle a value. Setup `model` row and Fallback toggle rows: flip |
| <kbd>+</kbd> <kbd>-</kbd> | Fallback detail: step `rotate at` or `weekly at` by 5 |
| <kbd>e</kbd> | Usage: toggle burn estimates |
| <kbd>p</kbd> | Usage: toggle the ideal-pace marker |
| <kbd>c</kbd> | Tokens: count cache reads and writes in the token totals |
| <kbd>f</kbd> | Plugin: apply the selected row's fix |

On macOS, <kbd>t</kbd> skips any account holding a live `clauth start` session: that session's login lives in a Keychain item clauth cannot write, so rotating it would sign the session out.

## Action menus

<kbd>a</kbd> opens the actions available for whatever is selected. Config and Plugin have none.

| Tab | Entries |
|-----|---------|
| Overview | `switch to selected`, `new account`, `refresh usage`, `rotate access token` |
| Usage | `refresh usage`, `toggle estimates`, `toggle pace marker` |
| Tokens | `period: lifetime` / `daily` / `weekly` / `monthly`, `show all models` / `show claude models` / `show other models`, `toggle cache counting`, `reload stats` |
| Setup (account list) | `configure`, `new account` |
| Setup (a settings row) | the row's own action: `edit field`, `remove field`, `toggle auto-start`, `log in`, `log out`, `disable account` / `enable account`, `delete account`, `create account` |
| Fallback (chain) | `open`, `reorder up`, `reorder down` |
| Fallback (member detail) | `edit threshold`, `edit weekly at`, `toggle weekly gate`, `toggle scoped gate`, `toggle last resort`, `toggle preferred`, `edit max auto-spend`, `remove member` |
| Status | `refresh status`, `open in browser` |

The active period or model filter is omitted from the Tokens menu, so the entries you see are the ones that would change something.

## Setup tab rows

| Row | Sets |
|-----|------|
| `name` | the profile name |
| `auto-start` | whether clauth opens the 5h window with a 1-token ping ([Configuration](Configuration#auto-start-the-5-hour-window)) |
| `base url` | the API endpoint; blank means an OAuth account |
| `api key` | the key for that endpoint |
| `model` | the account's default model; <kbd>space</kbd> cycles presets, <kbd>⏎</kbd> types a full id |
| `+ model override` | expands to `opus`, `sonnet`, `haiku`, `subagent` id overrides |
| env entries | extra environment variables merged into `settings.json` while this account is active |
| `disable account` / `enable account` | hides the account from auto-switch and polling, keeping its files |
| `+ login` / `re-login` | browser OAuth login for this profile |
| `log out` | drops the stored credentials, keeps the profile |
| `token` | read-only state of a stored long-lived setup token: its remaining life, or `expired` / `mis-filled` with the fix beneath it ([Configuration](Configuration#account-types)) |
| `clear long-lived token` | drops that token so the account's own OAuth login installs again; appears only when one is stored, arms on the first press, clears on the second. Faint and inert when the account has no other login to fall back to |
| `delete account` | removes the profile; arms on the first press, deletes on the second |

## Config tab rows

| Row | Options | Default |
|-----|---------|---------|
| `theme` | `full`, `compatible` | auto-detected |
| `reset display` | `relative`, `clock`, `both` | `relative` |
| `clock` | `24h`, `12h` | `24h` |
| `on mismatch` | `ask`, `overwrite`, `new`, `discard` | `ask` |
| `refresh` | 15 / 30 / 60 / 90 / 120 / 300 s, or a typed value from 10 s to 1 h | `90s` |
| `refresh spent` | keep polling accounts already at 100% | on |
| `rotation` | `preemptive`, `lazy` | `preemptive` |
| `weekly limit` | chain-wide 7d exhaustion line, 50-100% | `98%` |
| `switch mode` | `static`, `burn-aware` | `static` |
| `burn floor` | earliest projected-switch point, 90-100% | `98%` |
| `burn horizon` | how far ahead burn-aware projects | `60s` |
| `quota spent` | `stay on active`, `switch off all` | `stay on active` |
| `allow extra usage` | `off`, `pay-as-you-go` | `off` |
| `extra usage spent` | `stay on active`, `switch off all` | `switch off all` |

`clock` is inert unless `reset display` shows one. `burn floor` and `burn horizon` are inert unless `switch mode` is `burn-aware`. `extra usage spent` is inert unless `allow extra usage` is on. What each of the auto-switch rows does: [Auto-switch](Auto-Switch).

## Plugin tab

Each row is a check on your Claude Code wiring: `clauth` on `PATH`, the `mcpServers` entry, the plugin install record, `claude --version`, and each profile's runtime state. <kbd>f</kbd> applies a fix on rows that offer one, behind a confirm that defaults to cancel:

| Fix | When it appears |
|-----|-----------------|
| `wire mcpServers into ~/.claude.json` | the entry is missing, project-local only, or points somewhere stale |
| `repair credentials` | the active profile's stored login disagrees with the live one |
| `relink credentials` | the active profile's credential link is missing while its stored credentials are intact |
| `install plugin` | guidance only: the two `/plugin` commands to copy |
