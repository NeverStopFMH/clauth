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

`plugin install` runs the plugin's commands as you, so read them first. There are three files and none of them is long.

## What it adds

| Action | Qualified id | What it does |
|---|---|---|
| Open clauth | `clauth.open` | The clauth dashboard in a popup. Quit it with `q`, the same as anywhere else. |
| Switch clauth account | `clauth.switch` | The account table plus a prompt for the profile new Claude Code sessions link to. |

Both open a popup. herdr allows one popup per session, so a second press with clauth already up does nothing instead of reporting an error.

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

## Files

| File | Role |
|---|---|
| `herdr-plugin.toml` | Manifest: two popup entrypoints, two actions |
| `open-pane.sh` | Opens an entrypoint, treating "popup already open" as a no-op |
| `switch.sh` | The account picker popup |
