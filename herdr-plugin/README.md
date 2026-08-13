# clauth herdr plugin

Opens [clauth](https://github.com/uwuclxdy/clauth) in a [herdr](https://herdr.dev) popup: the account table, the usage windows, and the auto-switch chain, over whatever you were doing, without a pane of its own.

## Requires

- herdr 0.8.0 or newer. The manifest declares it, so an older herdr refuses to link with `plugin_requires_newer_herdr`.
- `clauth` on `PATH`.
- Linux or macOS. The entrypoints are POSIX shell, and herdr's own Windows support is preview-only.

## Install

```sh
herdr plugin install uwuclxdy/clauth/herdr-plugin
```

Local checkout instead:

```sh
herdr plugin link /path/to/clauth/herdr-plugin
```

`plugin install` runs the plugin's commands as you, so read them first. It is a manifest and two short shell scripts.

## What it adds

| Action | Qualified id | What it does |
|---|---|---|
| Open clauth | `clauth.open` | The clauth dashboard in a popup. Quit it with `q`, the same as anywhere else. |
| Show this pane's clauth account | `clauth.which` | Re-reads the account the focused pane burns and republishes it as pane metadata. |

Switching accounts is a keystroke inside the dashboard, so the plugin ships no picker of its own. herdr allows one popup per session, so pressing the open key again with clauth already up does nothing instead of reporting an error.

## Bind a key yourself

A herdr plugin cannot declare a keybinding. That line lives in your own herdr `config.toml`, and nothing happens until you add it:

```toml
[[keys.command]]
key = "prefix+a"
type = "plugin_action"
command = "clauth.open"
description = "clauth accounts"
```

`command` takes the qualified action id from the table above. Without a binding the actions are still reachable from herdr's action menu and from `herdr plugin action invoke clauth.open`.

## Show the account each pane burns

Every herdr pane running Claude Code spends some account, and which one is invisible from the pane itself. The plugin hooks agent detection and publishes the answer as pane metadata under the name `clauth`, refreshing it on every agent status change so a `clauth start --with-fallback` session that moves onto the next chain member stops naming the account it left. herdr detects other agents too, and a pane running one of those is left untagged rather than labelled with an account it never touches.

herdr renders a reported value only where your own agent-row template asks for it, so **the tag stays invisible until you add `$clauth` to a row** in your herdr `config.toml`. Claude Code panes take the `rows_by_agent` template rather than the generic one:

```toml
[ui.sidebar.agents.rows_by_agent]
claude = [["state_icon", "workspace", "tab"], ["terminal_title_stripped"], ["agent", "$clauth"]]
```

That row reads `claude · D1` in the sidebar for a pane started as `clauth start D1`. A pane running Claude Code some other way reports whichever account owns the global credentials. Point `CLAUDE_CONFIG_DIR` somewhere else yourself and the tag stops matching what that pane spends.

## What this plugin cannot do, by design of herdr's plugin v1

Plugin UI is pane-scoped. herdr documents runtime action registration and native non-terminal plugin UI as out of plugin v1, so none of the following is a missing feature here:

- no button or row beside the sidebar spaces list, and no status-bar item
- no menu outside a pane
- no mouse binding of any kind. herdr's key parser rejects mouse tokens, and the only click routed to a plugin is a Control-click on a URL matching a `link_handlers` pattern
- no click-outside dismiss. A popup holds every keystroke, Escape included, until its command exits

## Files

| File | Role |
|---|---|
| `herdr-plugin.toml` | Manifest: one popup entrypoint, two actions, two event hooks |
| `open-pane.sh` | Opens an entrypoint, treating "popup already open" as a no-op |
| `report-profile.sh` | Resolves the account a pane burns and publishes it as pane metadata |
