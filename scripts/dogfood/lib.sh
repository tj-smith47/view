#!/bin/sh
# WHY: shared by every dogfood tmux capture script (cap.sh and the scripts
# under tapes/), so the private-socket teardown is defined once. Two
# scripts carrying the same function body drift apart on the next edit to
# either one, with nothing to say the other exists
# (view-oracle's shell_guards.rs, no_two_scripts_define_the_same_function_body).
#
# Sourced, never executed. The caller defines SOCKET before its trap fires.
cleanup() { tmux -L "$SOCKET" kill-server 2>/dev/null || true; }
