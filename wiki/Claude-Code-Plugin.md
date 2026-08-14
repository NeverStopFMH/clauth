# Claude Code plugin

clauth ships an MCP server that hands your profiles to a live Claude Code session: compare usage across accounts, relink the active one, or hand a whole prompt to another account without spending the window you are in.

## Install

In Claude Code:

```
/plugin marketplace add uwuclxdy/clauth
/plugin install clauth@clauth
```

Claude Code launches `clauth mcp` in the background for the session's lifetime. `clauth` has to be on `PATH`, which it is after any standard install.

To wire the server by hand instead, add this to `mcpServers` in `~/.claude.json`:

```json
"clauth": { "type": "stdio", "command": "clauth", "args": ["mcp"] }
```

The TUI's Plugin tab writes exactly that entry for you with <kbd>f</kbd>. The manual route gives you the same five tools, minus the bundled hook that delivers backgrounded `delegate` results on its own: without the plugin, the session has to poll `delegate_result`.

## Tools

| Tool | Input | Returns | Cost |
|------|-------|---------|------|
| `list_profiles` | `names` (optional, case-insensitive), `format` | every profile with cached 5h / 7d percentages, provider, tier, endpoint host, active flag | none, reads the disk cache |
| `which` | `format` | the profile owning this session's credentials, its plan, its throughput | none |
| `switch` | `name`, `format` | relinks the global active profile | none |
| `delegate` | see below | the target account's answer, or a `job_id` | **a real usage window on the target account** |
| `delegate_result` | exactly one of `job_id` / `job_ids` (a list is capped at 256), `wait_secs` (0-60, default 0), `format` | a backgrounded job's envelope, or its running status; a batch returns one result per id | none |

Every tool takes `format` and answers in prose by default; pass `format: "json"` for the structured payload. An unrecognized value is refused by name, never treated as prose.

`which` is the authority on which account owns the current session. `list_profiles` reads a cache and can lag it.

`list_profiles` answers for every profile by default, and `names` narrows it to the ones you ask for. Two fields appear only when they have something to say: the live-session flag when a clauth-managed session already owns that profile, and the throughput rows when a model there is degraded or was recently rate-limited. On a 27-profile fleet that reply is just over half the size it would otherwise be, which matters because the model is told to call it at the start of every session.

## `switch` inside a session

`switch` repoints the global `~/.claude` credentials. A session running on those adopts the new account at its next token refresh, mid-task. A `clauth start` session runs against its own profile and is unaffected.

To use another account without disturbing the current session, use `delegate`.

## `delegate`

Runs a headless `claude -p` under another profile and returns what it produced.

| Field | Type | Default |
|-------|------|---------|
| `profile` | string | exactly one of `profile` / `profiles` |
| `profiles` | array of strings | exactly one of `profile` / `profiles`; background-only fan-out |
| `prompt` | string | exactly one of `prompt` / `prompt_file` |
| `prompt_file` | string | exactly one of `prompt` / `prompt_file`; path relative to `cwd` |
| `model` | string | the profile's own default |
| `cwd` | string | the server's working directory |
| `env` | object | none |
| `args` | array | none, appended to the `claude` invocation |
| `idle_secs` | int | `300`, max 3600 |
| `timeout_secs` | int | `3600`, max 3600 |
| `resume` | string | none, a session id |
| `isolated` | bool | `false` |
| `background` | bool | `false` |
| `monitor` | bool | `false` |
| `format` | string | `prose` (default), or `json` |

**Cost.** A delegate to a subscription account opens a real 5-hour window there. To a pay-as-you-go API-key account it bills real money. To a prepaid plan account it draws down quota you already bought, so it costs nothing extra. Call `list_profiles` first to pick the account with headroom.

**What it sees.** Only the prompt you pass. It has no view of the calling conversation, so the prompt has to carry the whole task.

**Prompt file.** `prompt_file` reads the prompt from a path relative to the delegate's `cwd` instead of passing it inline, so a long reusable prompt costs your context nothing to hand over. It is validated against `cwd` and refused by name when it is absolute, escapes `cwd`, resolves through a symlink outside `cwd`, is not a regular file, or is over 64 KiB. Give exactly one of `prompt` / `prompt_file`.

**Fan-out.** `profiles` spawns one delegate per named account, background-only, and spends one real usage window per account. It returns one `job_id` per account and echoes the resolved target list. Duplicate names (case-insensitive), unknown names, a member with no api key, an empty list, and a blocking call are refused before any spawn. Give exactly one of `profile` / `profiles`.

**Recursion.** Hard-capped at depth 1. A delegated session cannot call `delegate` again.

**Kill rules.** It dies once nothing has arrived for `idle_secs`, or once `timeout_secs` of wall clock passes, whichever comes first. A long run that keeps producing output is never cut off. A killed run still returns the text it had written, in `partial_result`, along with `timed_out`, `elapsed_secs`, and a `session_id` when the work is resumable.

**Resume.** Pass a killed run's `session_id` back as `resume` rather than paying for the work again. clauth runs the resume in the workspace that session was recorded in, and refuses a `cwd` that disagrees with it. A shared delegate is always resumable; an isolated one needs `auto_rescue` on, since its throwaway runtime is deleted at teardown otherwise.

**Isolated.** `isolated: true` drops your operator memory, plugins, hooks, and every MCP server, keeping the account's auth. Good for blind runs and evals.

**Background.** `background: true` returns a `job_id` immediately so the session keeps working. With the plugin installed, a bundled `PostToolUse` hook delivers the result as soon as the job finishes. A fan-out is delivered the same way: the hook waits on every job and prints each finished envelope together. A job still running at the hook's deadline is named, so collect it with `delegate_result`. Otherwise call `delegate_result` with each `job_id`, or pass them all as `job_ids` (capped at 256) for one result per id: the done envelope, a running status, or `unknown` for an absent id. Either spelling long-polls up to 60 s. Jobs live in `~/.clauth/jobs/`, swept an hour after they finish.

**Permissions.** A delegate spawns with Claude Code's permission gate armed and nobody to answer it, so a task that writes files fails on a denial rather than doing the work. Pass the permission flag through `args` when the delegate is meant to edit anything, and add `--add-dir` for reads outside `cwd`. Denials come back in a `permission_denials` array, so check that field rather than the prose result.

**Throughput.** clauth records observed tokens/sec per model per profile and flags an account as degraded or recently rate-limited in `list_profiles` and `which`. Subscription throttling is per model and absent from the usage snapshot, so this is the only signal for it.

## What the server tells the model

On connect it sends a short brief: a one-line index of the five tools, their cost model, what `switch` would do to this specific session, and a roster of your profiles as of session start. A `clauth start` session gets one more note: its runtime directory is mostly symlinks onto your real `~/.claude`, so an edit there is an edit to the global file.

The roster groups profiles that share a provider, tier and endpoint host onto one line, and leads with the account that has the most window left. Live usage numbers are deliberately left out of that snapshot, since they go stale immediately; only the ordering reflects them. `list_profiles` is the live read.

Everything specific to one tool rides that tool's own description rather than the brief: what a delegate costs, that it sees only its prompt, the depth cap. It loads when the tool does, and is never stated twice.
