#!/bin/sh
# Publishes the clauth account a herdr pane burns as pane metadata, so a
# sidebar row can name it. Runs from the agent event hooks and from the
# `clauth.which` action, which is why it takes the pane from the injected
# context rather than an argument.
#
# The pane's foreground process is the `claude` child, its parent is the
# `clauth start` supervisor, and only the supervisor's pid appears in the
# live-session registry, so the walk up the parent chain is the join.
set -u

herdr_bin="${HERDR_BIN_PATH:-herdr}"
sessions_dir="${CLAUTH_HOME:-$HOME/.clauth}/live_sessions"
pane="${HERDR_PANE_ID:-}"

# Prints the registry row owning $1 or one of its ancestors, empty if none.
session_row() {
    _pid=$1
    _depth=0
    while [ "${_pid:-0}" -gt 1 ] && [ "$_depth" -lt 8 ]; do
        # The delimiter alternative keeps the prefix exclusion a bare
        # "pid":$_pid would lose (123 against 1234) without pinning `pid` to
        # its current slot in the row.
        _row=$(grep -lE "\"pid\":$_pid(,|})" "$sessions_dir"/*.json 2>/dev/null | head -n 1)
        if [ -n "$_row" ]; then
            printf '%s\n' "$_row"
            return 0
        fi
        _pid=$(ps -o ppid= -p "$_pid" 2>/dev/null | tr -d ' ')
        _depth=$((_depth + 1))
    done
    return 1
}

# The agent hooks fire for every agent herdr detects, codex and cursor
# included, and those panes spend no clauth account. Both hooked events carry
# `agent`, and an action carries `focused_pane_agent` instead. Neither is set
# for a plain shell pane, which is the one case that still gets an answer.
agent=$(printf '%s' "${HERDR_PLUGIN_EVENT_JSON:-}" | sed -n 's/.*"agent":"\([^"]*\)".*/\1/p')
[ -n "$agent" ] || agent=$(printf '%s' "${HERDR_PLUGIN_CONTEXT_JSON:-}" | sed -n 's/.*"focused_pane_agent":"\([^"]*\)".*/\1/p')
case "$agent" in
    "" | claude) ;;
    *) exit 0 ;;
esac

profile=""
if [ -n "$pane" ]; then
    pids=$("$herdr_bin" pane process-info --pane "$pane" 2>/dev/null | grep -o '"pid":[0-9]*' | cut -d: -f2)
    for pid in $pids; do
        row=$(session_row "$pid") || continue
        # current_member is the chain member a --with-fallback session swapped
        # onto, and it is null until the first swap.
        profile=$(sed -n 's/.*"current_member":"\([^"]*\)".*/\1/p' "$row")
        [ -n "$profile" ] || profile=$(sed -n 's/.*"start_profile":"\([^"]*\)".*/\1/p' "$row")
        [ -n "$profile" ] && break
    done
fi

# No clauth-managed session in this pane: a bare `claude` there burns whatever
# owns the global credentials, so that answer is right rather than a guess.
[ -n "$profile" ] || profile=$(clauth which 2>/dev/null) || profile=""
[ -n "$profile" ] || exit 0

printf '%s\n' "$profile"

[ -n "$pane" ] || exit 0
# The pane id goes BEFORE the flags. `report-metadata --help` prints it last,
# and that order answers `unknown option: <value>` at exit 2 on 0.8.0.
"$herdr_bin" pane report-metadata "$pane" --source "${HERDR_PLUGIN_ID:-clauth}" --token "clauth=$profile"

# A --with-fallback session moves onto another account mid-run with no herdr
# event, so the one-shot report above goes stale until the next status change.
# Spawn a detached per-pane watcher to re-report on a timer instead. Only
# claude panes spend a clauth account; a plain shell pane resolves `agent`
# empty and is left alone. The pidfile makes later invocations skip the spawn
# while that watch lives, and the watcher removes it when the pane closes.
[ "$agent" = claude ] || exit 0
[ -n "$pane" ] || exit 0
state_dir="${HERDR_PLUGIN_STATE_DIR:-${TMPDIR:-/tmp}/clauth}"
mkdir -p "$state_dir" 2>/dev/null || exit 0
pidfile="$state_dir/watch-$pane.pid"
if [ -f "$pidfile" ] && kill -0 "$(cat "$pidfile" 2>/dev/null)" 2>/dev/null; then
    exit 0
fi
dir=$(dirname "$0")
"$dir/watch-profile.sh" "$pane" "$pidfile" </dev/null >/dev/null 2>&1 &
