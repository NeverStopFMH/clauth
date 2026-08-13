#!/bin/sh
# Runs in a placement="popup" pane, so it owns every keystroke until it exits.
# `clauth list` is the account table and `clauth <profile>` is the switch, so
# this pane is a prompt around them rather than a second view of the same
# state: an account that is disabled or out of window reads the same here as
# it does everywhere else in clauth.
set -u

if ! clauth list; then
    printf '\npress enter to close> '
    IFS= read -r _ || true
    exit 1
fi

printf '\nprofile to switch to (empty cancels)> '
IFS= read -r profile || exit 0
[ -n "$profile" ] || exit 0

clauth "$profile" || true

printf '\npress enter to close> '
IFS= read -r _ || true
