#!/bin/sh
# Opens one of this plugin's popup entrypoints. herdr's popup is a session
# singleton, so a second open while one is up answers "popup already open".
# Someone pressing the same key twice is not an error, so that answer exits 0
# and every other failure still reaches the plugin log.
#
# The popup_width knob picks the sizing flags, failing safe to the shipped
# default (fit) when the clauth binary predates the subcommand. An older
# herdr that refuses the flags gets the plain call as a retry.
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
        # fit: size against the focused pane's width. The snapshot is one
        # compact JSON line, and only its layout panes put `focused` right
        # ahead of `rect`, so the greedy prefix lands on the focused pane's
        # own width and the whole-line match leaves just the capture. The
        # workspace/tab/pane rows carry `focused` too, but never in front of
        # a `rect`. A failed read leaves the flags off entirely, the pre-knob
        # call shape.
        width=$("$herdr_bin" api snapshot 2>/dev/null |
            sed -n 's/.*"focused":true,"rect":{"x":[0-9]*,"y":[0-9]*,"width":\([0-9]*\).*/\1/p')
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
