#!/usr/bin/env bash
# Takes the recording of a felt-lag report so the user never has to.
#
# N alternating launches of view and of bare nvim over the same login-shaped
# fixture, each inside its own pty at the size the user's terminal reports,
# with the steps a lag report describes typed by this script: wait for the
# file to appear, wait a second, press `:`, wait for the palette, `Esc`,
# `:qa!`. Every run is a `script -I -O -T` recording, so what was typed is
# dated as exactly as what was painted -- the gap a `script -O` capture
# alone leaves, and the reason three reported moments could not be
# attributed to anything.
#
# The consumer of every run is `link-replay.js answer`: a live xterm.js
# instance reading the pty and writing its replies back into it. Without
# one, nothing answers the terminal queries an editor asks at startup and
# nvim spends 100 ms in `vim.wait` on an OSC 11 reply that never comes --
# a wait no terminal-attached session pays, and enough on its own to invert
# the comparison.
#
# Dates come from `link-replay.js report`, which replays each recording
# through the same emulator and reports the cell-exact frame each moment
# landed on. view's own `VIEW_LOG` lines are collected beside them, and so
# is the engine's `--startuptime`, which view passes through: nvim's own
# first screen update then sits beside view's first content frame on every
# run.
#
# Dev-only. Nothing in `task ci` runs this.
#
#   scripts/dogfood/link-record.sh [-n RUNS] [-f FILE] [-o OUTDIR]
#                                  [-s NEEDLE] [-c COLS] [-r ROWS]
#                                  [--cold] [--home] [--no-log]
#                                  [-l|--link] [--rate KBIT] [--delay MS]
#
# `-s NEEDLE` is the word both waits and both readings are written against:
# the file is on the wire when the needle appears, and the colours arrived
# when the needle's own line gains a colour it did not have. So the needle
# picks the line whose colouring is dated -- a word on a comment line dates
# the comments arriving, a word in code dates that line's own syntax.
#
# `--home` drops the fixture and launches both editors on the real `$HOME`
# with its XDG directories untouched, which is the config the user actually
# reports lag on. Nothing under `$HOME` is removed or reset: a launch writes
# to its own state and cache the way any launch does. It cannot be combined
# with `--cold`, whose whole subject is a state directory of its own.
#
# `--cold` gives each run a data directory of its own, holding nothing but
# a link to the installed plugins, so everything the editors write beside
# those plugins is absent the way it is on a machine that has not run them
# before. State and cache are per-run in both arms already; the data
# directory is what a warm HOME still carries, and the moment it moves is
# VimEnter to the first content frame.
#
# `--no-log` runs the view side with no `VIEW_LOG` at all, which is what
# prices the instrument itself: the `highlight` topic reads every cell of the
# window grid on each flushed frame for as long as the watch runs, and every
# figure this recorder publishes was taken with that reading on. The columns
# that come out of the log are empty on this arm; `text_ms`, `hl_ms` and
# `colon_ms` are read off the wire and are the comparison.
#
# `--link` puts a real link under the run instead of a local pty: the editor
# runs over `ssh localhost` and a netem qdisc shapes the loopback traffic to
# that ssh port for the duration. It answers whether view's writer blocks on
# the drain of its full-grid chrome frame and holds the file's own frame
# behind it, which a rate-limited reader on a local pipe cannot: the pipe
# and the reader's own buffer together hold more than a whole session's
# bytes, so nothing ever pushed back. It needs root and key-based ssh from
# this host to itself, and refuses rather than falling back.
set -euo pipefail

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
REPO=$(cd -- "$HERE/../.." && pwd)

RUNS=5
FILE=$REPO/crates/view-core/src/model.rs
OUT=$HOME/.claude/tmp/link-record/$(date +%Y%m%d-%H%M%S)
NEEDLE=
COLS=263
ROWS=88
LINK=0
COLD=0
HOME_ARM=0
NO_LOG=0
# the link the user's own recording showed: a 24 kB chrome frame arriving
# over 37 ms, and a round trip in the tens of milliseconds
RATE_KBIT=5200
DELAY_MS=25
SSH_PORT=22

while [ "$#" -gt 0 ]; do
  case "$1" in
    (-n) RUNS=$2; shift 2 ;;
    (-f) FILE=$2; shift 2 ;;
    (-o) OUT=$2; shift 2 ;;
    (-s) NEEDLE=$2; shift 2 ;;
    (-c) COLS=$2; shift 2 ;;
    (-r) ROWS=$2; shift 2 ;;
    (--cold) COLD=1; shift ;;
    (--home) HOME_ARM=1; shift ;;
    (--no-log) NO_LOG=1; shift ;;
    (-l|--link) LINK=1; shift ;;
    (-h|--help)
      awk 'NR > 1 { if ($0 !~ /^#/) exit; sub(/^# ?/, ""); print }' "$0"
      exit 0 ;;
    (--rate) RATE_KBIT=$2; shift 2 ;;
    (--delay) DELAY_MS=$2; shift 2 ;;
    (*) echo "link-record: see the header of $0" >&2; exit 2 ;;
  esac
done

if [ "$COLD" = "1" ] && [ "$HOME_ARM" = "1" ]; then
  echo "link-record: --cold gives the run a state directory of its own and" \
       "--home uses the user's; pick one" >&2
  exit 2
fi

VIEW_BIN=${VIEW_BIN:-$REPO/target/release/view}
NVIM_BIN=${NVIM_BIN:-$(command -v nvim || true)}
# the login-shaped fixture the first-paint matrix measures on, and the
# plugin cache its generated init resolves every `dir` entry against
FIXTURE=${FIXTURE:-$REPO/target/bench-fixtures/user}
PLUGINS=${PLUGINS:-$REPO/compat/.cache/2895d052d37fc12c}
DEPS=${DEPS:-$HOME/.cache/view-link-replay}

for required in "$VIEW_BIN" "$NVIM_BIN" "$FILE"; do
  if [ ! -e "$required" ]; then
    echo "link-record: $required is missing; build it first" >&2
    exit 1
  fi
done
if [ ! -d "$FIXTURE/nvim" ]; then
  echo "link-record: no user fixture at $FIXTURE; run: task user-fixture" >&2
  exit 1
fi
if [ ! -d "$PLUGINS/nvim/lazy" ]; then
  echo "link-record: no plugin cache at $PLUGINS; run: task compat" >&2
  exit 1
fi
if [ ! -d "$DEPS/node_modules/@xterm/headless" ]; then
  mkdir -p "$DEPS"
  cp "$HERE/package.json" "$DEPS/package.json"
  cp "$HERE/package-lock.json" "$DEPS/package-lock.json"
  npm install --prefix "$DEPS" --silent
fi

# The link arm needs to reach this host over ssh and to shape the loopback
# traffic that carries it, and both are refused rather than worked around:
# a run that silently fell back to a local pty would be published as a link
# reading.
if [ "$LINK" = "1" ]; then
  if [ "$(id -u)" != "0" ]; then
    echo "link-record: --link needs root for the netem qdisc on lo" >&2
    exit 1
  fi
  if ! command -v tc > /dev/null 2>&1; then
    echo "link-record: --link needs tc (iproute2)" >&2
    exit 1
  fi
  if ! ssh -o BatchMode=yes -o StrictHostKeyChecking=no localhost true \
       > /dev/null 2>&1; then
    echo "link-record: --link needs key-based ssh to localhost; add this" \
         "host's own public key to ~/.ssh/authorized_keys" >&2
    exit 1
  fi
fi

# The word the replay looks for on screen. A word rather than a phrase: a
# coloured buffer puts every token in its own run on the wire, so a phrase
# that reads as one line on screen is several writes and several cells, and
# the wait below would never see it.
if [ -z "$NEEDLE" ]; then
  NEEDLE=$(awk 'NR > 1 && NR < 16 {
    n = split($0, words, /[^A-Za-z_]+/)
    for (i = 1; i <= n; i++) {
      if (length(words[i]) >= 6) { print words[i]; exit }
    }
  }' "$FILE")
fi
if [ -z "$NEEDLE" ]; then
  echo "link-record: no word of six characters near the top of $FILE" >&2
  exit 1
fi

# A directory that already holds a run is refused: the fifo below cannot be
# created twice, and a second batch written over the first leaves a table
# whose rows come from two hosts.
if [ -e "$OUT/runs.tsv" ] || [ -e "$OUT/view-1/in" ]; then
  echo "link-record: $OUT already holds a recording; name another -o" >&2
  exit 1
fi

mkdir -p "$OUT"
echo "link-record: $RUNS runs per side into $OUT (needle $NEEDLE)"

SHAPED=0
CHILD=
READER=
cleanup() {
  # only the pids this script started, and the editor's own `script` before
  # the reader that was watching it: killing the reader first leaves the
  # editor running with nothing draining its pty
  if [ -n "$CHILD" ]; then
    # the editor is a child of the `script` this loop started, so killing
    # only `script` leaves a `view` or `nvim` running on a shared host with
    # nothing draining its pty. Scoped to that one pid's children, never to
    # a name: a peer session's live-nvim test is not this script's to reap
    pkill -P "$CHILD" 2>/dev/null || true
    kill "$CHILD" 2>/dev/null || true
  fi
  if [ -n "$READER" ]; then
    kill "$READER" 2>/dev/null || true
  fi
  if [ "$SHAPED" = "1" ]; then
    tc qdisc del dev lo root 2>/dev/null || true
    SHAPED=0
  fi
}
trap cleanup EXIT

# Shapes only the loopback traffic on the ssh port, so the rest of this
# host's loopback -- other sessions, local services -- is untouched by a
# measurement that has no business slowing it down.
if [ "$LINK" = "1" ]; then
  tc qdisc del dev lo root 2>/dev/null || true
  tc qdisc add dev lo root handle 1: prio
  tc qdisc add dev lo parent 1:3 handle 30: netem \
    rate "${RATE_KBIT}kbit" delay "${DELAY_MS}ms" limit 20
  tc filter add dev lo protocol ip parent 1: prio 1 u32 \
    match ip dport "$SSH_PORT" 0xffff flowid 1:3
  tc filter add dev lo protocol ip parent 1: prio 1 u32 \
    match ip sport "$SSH_PORT" 0xffff flowid 1:3
  SHAPED=1
  echo "link-record: lo shaped for port $SSH_PORT at ${RATE_KBIT}kbit" \
       "delay ${DELAY_MS}ms"
fi

# Bytes on the wire so far, which is what every wait below is written
# against: a settle is that number holding still, and the file arriving is
# the needle appearing in it.
wire_size() {
  wc -c < "$1" | tr -d ' '
}

# Waits until `grep` finds the needle in the recording, or the bound runs
# out. A bound rather than a wait with no end: a run whose file never
# appears has to be reported as one.
await_needle() {
  waited=0
  while [ "$waited" -lt 200 ]; do
    if grep -qF -- "$NEEDLE" "$1" 2>/dev/null; then
      return 0
    fi
    sleep 0.1
    waited=$((waited + 1))
  done
  return 1
}

# Waits until the recording stops growing for three reads running, which is
# what "the screen settled" is from outside the session.
await_settle() {
  waited=0
  still=0
  last=$(wire_size "$1")
  while [ "$waited" -lt 100 ]; do
    sleep 0.1
    waited=$((waited + 1))
    now=$(wire_size "$1")
    if [ "$now" = "$last" ]; then
      still=$((still + 1))
      if [ "$still" -ge 3 ]; then
        return 0
      fi
    else
      still=0
      last=$now
    fi
  done
  return 1
}

# One launch, recorded. `$1` is the side, `$2` the run number.
record_one() {
  side=$1
  index=$2
  dir=$OUT/$side-$index
  mkdir -p "$dir/state" "$dir/cache" "$dir/frames"
  # a data directory holding the plugins and nothing else, so the logs,
  # histories and per-plugin scratch a warm HOME carries beside them are
  # missing the way they are on a machine that has not run the editors
  # before. The plugins themselves stay, because the fixture's config
  # resolves them under the data home and installs nothing that is absent
  data=$PLUGINS
  if [ "$COLD" = "1" ]; then
    mkdir -p "$dir/data/nvim"
    ln -sn "$PLUGINS/nvim/lazy" "$dir/data/nvim/lazy"
    data=$dir/data
  fi
  cut -d ' ' -f 1 /proc/loadavg > "$dir/load" 2>/dev/null || echo 0 > "$dir/load"
  mkfifo "$dir/in" "$dir/wire" "$dir/ready"

  {
    # the local arm's editor runs in the pty `script` made, which starts at
    # 0x0 because this script's stdin is not a terminal; over ssh the size
    # travels from the client's pty instead and is set there
    if [ "$LINK" = "0" ]; then
      echo "stty rows $ROWS cols $COLS"
    fi
    # the home arm exports none of these, so every editor resolves its
    # config, plugins, state and cache exactly where the user's own launch
    # resolves them
    if [ "$HOME_ARM" = "0" ]; then
      echo "export XDG_CONFIG_HOME=$FIXTURE"
      echo "export XDG_DATA_HOME=$data"
      echo "export XDG_STATE_HOME=$dir/state"
      echo "export XDG_CACHE_HOME=$dir/cache"
    fi
    echo "export TERM=xterm-256color COLORTERM=truecolor"
    case "$side" in
      # view forwards the flag to the engine it spawns, so the engine's own
      # first screen update is recorded beside view's first content frame
      (view) if [ "$NO_LOG" = "0" ]; then
               echo "export VIEW_LOG=$dir/view.log"
             fi
             echo "exec $VIEW_BIN --startuptime $dir/startuptime $FILE" ;;
      (*) echo "exec $NVIM_BIN --startuptime $dir/startuptime $FILE" ;;
    esac
  } > "$dir/cmd.sh"

  if [ "$LINK" = "1" ]; then
    {
      echo "stty rows $ROWS cols $COLS"
      echo "exec ssh -tt -o BatchMode=yes -o StrictHostKeyChecking=no" \
           "localhost bash $dir/cmd.sh"
    } > "$dir/wrap.sh"
    INNER=$dir/wrap.sh
  else
    INNER=$dir/cmd.sh
  fi

  NODE_PATH=$DEPS/node_modules node "$HERE/link-replay.js" answer \
    --cols "$COLS" --rows "$ROWS" --wire "$dir/wire" --reply "$dir/in" \
    --ready "$dir/ready" > "$dir/answer.log" 2>&1 &
  READER=$!
  # node's own startup is some 30 ms, and an editor that asks its terminal a
  # question inside that window waits for the answer: read the emulator's
  # own ready line before the editor is started, which blocks until it is up.
  # Opened read-write and bounded, because a fifo opened for reading alone
  # blocks in the open until a writer arrives -- where `read -t` bounds only
  # the read -- and an emulator that died before writing its line would
  # otherwise hold this run open for good
  exec 4<> "$dir/ready"
  if ! read -r -t 30 _ <&4; then
    echo "  $side-$index: the emulator never reported ready" >&2
  fi
  exec 4>&-
  script -q -e -I "$dir/in.log" -O "$dir/out.log" -T "$dir/tm.log" \
    -c "bash $INNER" < "$dir/in" > "$dir/wire" 2>&1 &
  CHILD=$!

  exec 3> "$dir/in"
  if ! await_needle "$dir/out.log"; then
    echo "  $side-$index: the file never reached the wire" >&2
  fi
  # the pause a lag report describes: the file is on screen and the user is
  # looking at it before reaching for the command line
  sleep 1
  printf ':' >&3
  await_settle "$dir/out.log" || true
  printf '\033' >&3
  await_settle "$dir/out.log" || true
  printf ':qa!\r' >&3
  wait "$CHILD" 2>/dev/null || true
  CHILD=
  exec 3>&-
  wait "$READER" 2>/dev/null || true
  READER=
  rm -f "$dir/in" "$dir/wire" "$dir/ready"
}

# The moments of one recording, as `name_ms=` lines.
replay_one() {
  dir=$1
  side=$2
  case "$side" in
    # view answers a typed `:` with its own framed palette; bare nvim
    # echoes it into the first cell of the last row
    (view) chrome=--palette ;;
    (*) chrome= ;;
  esac
  NODE_PATH=$DEPS/node_modules node "$HERE/link-replay.js" report \
    --out "$dir/out.log" --timing "$dir/tm.log" --in "$dir/in.log" \
    --cols "$COLS" --rows "$ROWS" --needle "$NEEDLE" \
    $chrome --frames "$dir/frames" > "$dir/moments.txt"
}

# One `name=value` line out of a moments file, or an empty string.
moment() {
  awk -F= -v key="$2" '$1 == key { print $2 }' "$1"
}

# The engine's own `first screen update`, in milliseconds, out of the
# `--startuptime` file both sides write -- view's because it forwards the
# flag to the engine it spawns.
first_screen() {
  if [ ! -f "$1" ]; then
    echo ""
    return 0
  fi
  awk '/first screen update/ { print $1; exit }' "$1"
}

# view's own reading of the same moment, off the `startup` topic: the frame
# it handed the terminal with the file's text on it. Empty for the nvim
# side, which writes no such log.
content_frame() {
  if [ ! -f "$1" ]; then
    echo ""
    return 0
  fi
  awk '/first content frame written/ { print $1; exit }' "$1"
}

# The first reading the `startup` topic wrote for one milestone, in
# milliseconds from the launch. Empty for the nvim side, which writes no
# such log.
milestone() {
  if [ ! -f "$1" ]; then
    echo ""
    return 0
  fi
  awk -v want="$2" 'index($0, want) { print $1; exit }' "$1"
}

# How many redraw batches the engine sent before the frame with the file's
# text on it. The count view itself wrote on that line, so a batch the
# recorder cannot see from outside is still counted.
redraw_batches() {
  if [ ! -f "$1" ]; then
    echo ""
    return 0
  fi
  awk '/first content frame written/ {
    for (i = 1; i <= NF; i++) {
      if (substr($i, 1, 8) == "redraws=") {
        print substr($i, 9)
        exit
      }
    }
  }' "$1"
}

# Where the engine first placed the file's own window, off the `layout`
# topic. The global grid is skipped: it is the one view's chrome frame
# draws into, and it is placed before the file's window exists.
window_placed() {
  if [ ! -f "$1" ]; then
    echo ""
    return 0
  fi
  awk '/layout win_pos grid=/ {
    for (i = 1; i <= NF; i++) {
      if (substr($i, 1, 5) == "grid=" && substr($i, 6) != "1") {
        print $1
        exit
      }
    }
  }' "$1"
}

# What view itself measured between the typed `:` and the frame carrying
# the palette, in milliseconds off the `key` topic's own microseconds. The
# recorder's wire reading covers the same moment plus the link; this is the
# half that belongs to view.
palette_wait() {
  if [ ! -f "$1" ]; then
    echo ""
    return 0
  fi
  # the first `:` of the session and no other: a walk that kept looking
  # reported the `:` of the closing `:qa!` in the same column, with nothing
  # saying the palette's own line carried no reading
  awk '/^[0-9]+ key .*notation=":"/ {
    for (i = 1; i <= NF; i++) {
      if (substr($i, 1, 10) == "waited_us=") {
        printf "%.1f", substr($i, 11) / 1000
      }
    }
    exit
  }' "$1"
}

# `colon_ms`, or an empty field and a message naming the run where the
# subtraction cannot be a reading. Two writers share the `in` fifo -- this
# script's steps and the emulator's replies -- so `script` can coalesce a
# reply and a step into one input record, and the `:` is then dated at the
# `Esc` typed after it. Silent before this: the column held a negative or
# absurd number in the same place a real reading goes.
bounded_colon() {
  awk -v a="$1" -v b="$2" -v run="$3" 'BEGIN {
    if (a == "" || b == "") exit
    d = a - b
    if (d < 0 || d > 1000) {
      printf "  %s: the cmdline is %.1f ms after the typed step, which dates \
the `:` at another input record; colon_ms left empty\n", run, d \
        > "/dev/stderr"
      exit
    }
    printf "%.1f", d
  }'
}

# The difference between two readings, or an empty field where either side
# of it is missing.
delta() {
  awk -v a="$1" -v b="$2" \
    'BEGIN { if (a == "" || b == "") print ""; else printf "%.1f", a - b }'
}

# Tab-separated, and rendered into a table below: a moment the replay never
# found leaves its field empty, and a blank-separated column would then
# collapse into the one beside it and be read as that one.
RUNS_TSV=$OUT/runs.tsv
printf 'side\trun\tload\tbusy\ttext_ms\thl_ms\tcolours\tcolon_ms\tpalette_ms\texit_ms\tengine_ms\tcontent_ms\tvim_ms\tchrome_ms\tforeign_ms\tredraws\twinpos_ms\n' \
  > "$RUNS_TSV"

index=1
while [ "$index" -le "$RUNS" ]; do
  for side in view nvim; do
    record_one "$side" "$index"
    dir=$OUT/$side-$index
    replay_one "$dir" "$side"
    load=$(cat "$dir/load")
    content=$(content_frame "$dir/view.log")
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$side" "$index" "$load" \
      "$(awk -v l="$load" 'BEGIN { print (l + 0 >= 2.0) ? "busy" : "" }')" \
      "$(moment "$dir/moments.txt" text_ms)" \
      "$(moment "$dir/moments.txt" highlight_ms)" \
      "$(moment "$dir/moments.txt" base_colours)" \
      "$(bounded_colon "$(moment "$dir/moments.txt" cmdline_ms)" \
                       "$(moment "$dir/moments.txt" typed1_ms)" \
                       "$side-$index")" \
      "$(palette_wait "$dir/view.log")" \
      "$(delta "$(moment "$dir/moments.txt" handback_ms)" \
               "$(moment "$dir/moments.txt" typed3_ms)")" \
      "$(first_screen "$dir/startuptime")" \
      "$content" \
      "$(delta "$content" \
               "$(milestone "$dir/view.log" "vim_enter received")")" \
      "$(delta "$content" \
               "$(milestone "$dir/view.log" "chrome frame written")")" \
      "$(milestone "$dir/view.log" "notify-sink foreign=true")" \
      "$(redraw_batches "$dir/view.log")" \
      "$(window_placed "$dir/view.log")" >> "$RUNS_TSV"
  done
  index=$((index + 1))
done

# min, median and max of one column, per side. `sort -n` is handed only the
# rows that carry a reading, so a spread is over the runs that recorded one
# and `n=` says how many that was.
spread() {
  awk -F'\t' -v side="$1" -v col="$2" \
    'NR > 1 && $1 == side && $col != "" { print $col }' "$RUNS_TSV" \
    | sort -n | awk -v side="$1" -v name="$3" '
    { v[NR] = $1 }
    END {
      if (NR == 0) { printf "%-5s %-9s no run recorded one\n", side, name; exit }
      mid = (NR % 2) ? v[int(NR / 2) + 1] : (v[NR / 2] + v[NR / 2 + 1]) / 2
      printf "%-5s %-9s n=%-3d min=%-8.1f median=%-8.1f max=%-8.1f\n", \
        side, name, NR, v[1], mid, v[NR]
    }'
}

TABLE=$OUT/table.txt
{
  if [ "$LINK" = "1" ]; then
    echo "arm: link (ssh localhost, lo shaped ${RATE_KBIT}kbit" \
         "delay ${DELAY_MS}ms on port $SSH_PORT)"
  else
    echo "arm: local pty"
  fi
  echo "consumer: live xterm.js answering the terminal's own queries"
  if [ "$NO_LOG" = "1" ]; then
    echo "log: none -- the view side runs with no VIEW_LOG, so every column" \
         "read off it is empty"
  fi
  if [ "$HOME_ARM" = "1" ]; then
    echo "state: the real \$HOME, its XDG directories untouched"
  elif [ "$COLD" = "1" ]; then
    echo "state: cold (per-run state, cache and data; plugins linked in)"
  else
    echo "state: warm data home, per-run state and cache"
  fi
  echo
  awk -F'\t' '{
    printf "%-5s %-4s %-6s %-5s %-9s %-9s %-8s %-9s %-11s %-9s %-10s %-11s %-8s %-10s %-11s %-8s %-10s\n", \
      $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15, $16, \
      $17
  }' "$RUNS_TSV"
  echo
  echo "spread per moment per side, milliseconds from the launch"
  for side in view nvim; do
    spread "$side" 5 text
    spread "$side" 6 highlight
    spread "$side" 8 colon
    spread "$side" 9 palette
    spread "$side" 10 handback
    spread "$side" 11 engine
    spread "$side" 12 content
    spread "$side" 13 vim-enter
    spread "$side" 14 chrome
    spread "$side" 15 foreign
    spread "$side" 16 redraws
    spread "$side" 17 winpos
  done
} > "$TABLE"

cat "$TABLE"
echo
echo "frames and logs: $OUT"
