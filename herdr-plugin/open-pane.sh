#!/bin/sh
# Opens one of this plugin's popup entrypoints. herdr's popup is a session
# singleton, so a second open while one is up answers "popup already open".
# Someone pressing the same key twice is not an error, so that answer exits 0
# and every other failure still reaches the plugin log.
#
# The popup_width knob picks the sizing flags, failing safe to the shipped
# default (fit) when the clauth binary predates the subcommand. A herdr that
# refuses the sizing flags (measured 2026-08-26: 0.8.2 accepts them, hidden
# from --help) gets the plain call as a retry.
#
# `set -e` is load-bearing here: every risky command sits in a condition or a
# `&&`/`||` chain, so a snapshot or open failure falls through to the fallback
# arms rather than aborting the open. report-profile.sh and watch-profile.sh
# deliberately use `set -u` only, because a failed publish must never kill a
# hook.
set -eu

entrypoint="${1:?usage: open-pane.sh <entrypoint-id>}"
herdr_bin="${HERDR_BIN_PATH:-herdr}"
plugin_id="${HERDR_PLUGIN_ID:-clauth}"

width_mode=$(clauth herdr config get popup_width 2>/dev/null || printf 'fit')

# The open argv is the --plugin/--entrypoint pair plus the sizing flags the
# knob picked; the retry below runs the pair alone.
set -- --plugin "$plugin_id" --entrypoint "$entrypoint"
case "$width_mode" in
    full)
        set -- "$@" --width 100% --height 50%
        ;;
    half)
        # No width flag: herdr's default half width. The height stays pinned.
        set -- "$@" --height 50%
        ;;
    *)
        # fit: size against the focused pane's width. The snapshot names the
        # focused pane in `focused_pane_id`, and its layout row spells the
        # rect `{"height":H,"width":W,...}` on 0.8.2 (measured against the
        # real snapshot 2026-08-26; the pane records carry no rect, and the
        # layout rows put `pane_id` right before `rect`). Matching the pane
        # by id keeps the greedy prefix from landing on another tab's focused
        # row. A failed read leaves the flags off entirely, the pre-knob call
        # shape: the `|| snap=''` keeps a failing snapshot from aborting the
        # whole open under `set -e`.
        snap=$("$herdr_bin" api snapshot 2>/dev/null) || snap=''
        focused=$(printf '%s' "$snap" | sed -n 's/.*"focused_pane_id":"\([^"]*\)".*/\1/p')
        width=$(printf '%s' "$snap" |
            sed -n "s/.*\"pane_id\":\"$focused\",\"rect\":{\"height\":[0-9]*,\"width\":\([0-9]*\).*/\1/p")
        if [ -n "$width" ]; then
            if [ "$width" -ge 540 ]; then
                set -- "$@" --width 540 --height 50%
            else
                set -- "$@" --width 100% --height 50%
            fi
        fi
        ;;
esac

# One open attempt with the caller's argv. Exits 0 on success and on the
# singleton's "popup already open" answer; leaves the answer in $out and
# returns 1 for the caller to retry or log.
open_attempt() {
    out=$("$herdr_bin" plugin pane open "$@" 2>&1) && return 0
    case "$out" in
        *"popup already open"*) return 0 ;;
    esac
    return 1
}

if open_attempt "$@"; then
    exit 0
fi
# The pair above is four words, so anything past it is a sizing flag an older
# herdr answers "unknown option" on. Retry the plain call once; its answer,
# success or failure, decides the exit.
if [ "$#" -gt 4 ]; then
    if open_attempt --plugin "$plugin_id" --entrypoint "$entrypoint"; then
        exit 0
    fi
fi

printf '%s\n' "$out" >&2
exit 1
