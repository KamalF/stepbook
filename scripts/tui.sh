#!/bin/bash
# Drives stepbook in tmux for a check, within one command: the sandbox of
# Claude Code stops the tmux server when a command ends.
#
# Usage: scripts/tui.sh STEPS_FILE
#
# STEPS_FILE is bash that calls these functions:
#   start WIDTH HEIGHT COMMAND...   start COMMAND in a new pane
#   keys KEY...                     send tmux keys, such as j, End or q
#   cap [-e]                        print the pane, with -e its escapes too
#   wait_s SECONDS                  wait
# START_WAIT and KEY_WAIT set the waits after start and keys.
#
# Example:
#   START_WAIT=1.5 start 80 24 target/release/stepbook --no-log run.md
#   keys j
#   cap
#   keys q
#   cap
#
# Use one STEPS_FILE per start: a second start in the same file can race
# the restart of the tmux server.
set -u
export TMUX_TMPDIR=${TMPDIR:-/tmp}/tui-tmux
mkdir -p "$TMUX_TMPDIR"
# A server of its own: two checks at the same time must not kill each
# other's.
T="tmux -L tui-$$"

start() {
    local w=$1 h=$2
    shift 2
    $T kill-server 2>/dev/null
    $T new-session -d -s s -x "$w" -y "$h" "bash --norc --noprofile"
    $T set-option -t s remain-on-exit on
    $T send-keys -t s "PS1='$ '; clear; $*; echo \"[exit \$?]\"" Enter
    sleep "${START_WAIT:-0.6}"
}
keys() {
    $T send-keys -t s "$@"
    sleep "${KEY_WAIT:-0.3}"
}
cap() {
    echo "----- pane -----"
    $T capture-pane -p -t s ${1:-}
    echo "----------------"
}
wait_s() { sleep "$1"; }

source "$1"
$T kill-server 2>/dev/null
