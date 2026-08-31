#!/bin/sh
# Capture harness for the terminal capability probe: writes the query batch
# view sends at startup and dumps whatever the terminal answers, hex and
# C-escaped, beside the terminal's identity. Produces the raw material for
# docs/terminal-probe-wire-capture.md -- run it in a real emulator, then read
# the report file it wrote.
#
# Usage:
#   capture-terminal-probe.sh probe <outfile>   the batch view sends at startup
#   capture-terminal-probe.sh keys  <outfile>   drain mode: dumps every byte
#                                               that arrives, for capturing
#                                               what a keypress looks like
#
# The report goes to <outfile> because the terminal itself is stdout here: a
# pane's output is the thing being measured, not a place to put results.
set -eu

MODE="${1:-probe}"
OUT="${2:?usage: capture-terminal-probe.sh [probe|keys] <outfile>}"

exec </dev/tty >/dev/tty 2>/dev/null
[ -t 0 ] || { echo "no controlling terminal" >>"$OUT"; exit 2; }
exec 3>"$OUT"

RAW="$(mktemp)"
SAVED="$(stty -g)"
cleanup() { stty "$SAVED" 2>/dev/null || true; rm -f "$RAW"; }
trap cleanup EXIT INT TERM

emit() { printf '%s\n' "$*" >&3; }

# Renders a byte stream as `1b 5b 3f` hex and as `\x1b[?`-style escapes.
dump() {
  emit "  bytes:   $(wc -c <"$1" | tr -d ' ')"
  emit "  hex:     $(od -An -v -tx1 "$1" | tr -s ' \n' ' ' | sed 's/^ //;s/ $//')"
  emit "  escaped: $(od -An -v -tx1 "$1" | tr -s ' \n' '\n\n' | sed '/^$/d' | awk '
    BEGIN { h = "0123456789abcdef" }
    {
      n = (index(h, substr($0, 1, 1)) - 1) * 16 + index(h, substr($0, 2, 1)) - 1
      if (n == 92) { printf "\\\\" }
      else if (n >= 32 && n < 127) { printf "%c", n }
      else { printf "\\x%02x", n }
    }
    END { printf "\n" }')"
}

# Raw mode has to be entered before the queries go out, not before the read:
# canonical mode holds a reply that carries no newline -- which is every reply
# here -- inside the line discipline until one arrives, and echo would paint
# the reply bytes into the pane on their way past.
#
# VMIN 0 rather than 1, so a terminal that answers nothing at all ends the
# window on the timer instead of blocking forever; with VMIN > 0 the VTIME
# timer is an inter-byte one that never starts.
arm_raw() {
  : >"$RAW"
  stty raw -echo min 0 time "$1"
}

# Reads until the terminal goes quiet for a whole VTIME window: VMIN/VTIME
# turn a lull into a 0-byte read, which `head` takes as end of input. The
# wait is the line discipline's, not a sleep loop's.
#
# `head`, not `dd`: uutils `dd` -- the coreutils on this host -- truncates an
# append-mode stdout, so a straggler read silently erases the burst that
# preceded it.
read_replies() {
  head -c 4096 >"$RAW" || true
  stty "$SAVED"
}

emit "== host"
emit "  host:         $(hostname)"
emit "  uname:        $(uname -sr)"
emit "  TERM:         ${TERM-<unset>}"
emit "  COLORTERM:    ${COLORTERM-<unset>}"
emit "  TERM_PROGRAM: ${TERM_PROGRAM-<unset>}"
emit "  TMUX:         ${TMUX-<unset>}"
emit "  LANG:         ${LANG-<unset>}"

if [ "$MODE" = keys ]; then
  emit "== keystroke drain"
  arm_raw 100
  read_replies
  dump "$RAW"
  exit 0
fi

# The batch view writes at startup (crates/view-tui/src/tiers.rs), with the
# box-glyph CPR probe inserted ahead of the DA1 fence so the capture shows
# whether the fence still answers last.
emit "== sent"
emit '  escaped: \x1b[?2026$p\x1b[?u\x1b[48;2;1;2;3m\x1bP$qm\x1b\\\x1b[0m\r\xe2\x95\xad\x1b[6n\r\x1b[K\x1b[c'
arm_raw 10
printf '\033[?2026$p'
printf '\033[?u'
printf '\033[48;2;1;2;3m\033P$qm\033\\\033[0m'
printf '\r\342\225\255\033[6n\r\033[K'
printf '\033[c'

emit "== received"
read_replies
dump "$RAW"
