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
# Dates come from `link-replay.js`, which replays each recording through
# xterm.js headless and reports the cell-exact frame each moment landed on.
# view's own `VIEW_LOG` lines are collected beside them, and so is the
# engine's `--startuptime`, which view passes through: nvim's own first
# screen update then sits beside view's first content frame on every run.
#
# Dev-only. Nothing in `task ci` runs this.
#
#   scripts/dogfood/link-record.sh [-n RUNS] [-f FILE] [-o OUTDIR]
#                                  [-t BYTES_PER_SECOND] [-s NEEDLE]
#
# `-t` puts a rate-limited reader where `script` writes, which is the link
# the user is on rather than a local pty: it answers whether view's writer
# blocks on the drain of its full-grid chrome frame and holds the file's
# own frame behind it.
set -euo pipefail

HERE=$(cd -- "$(dirname -- "$0")" && pwd)
REPO=$(cd -- "$HERE/../.." && pwd)

RUNS=5
FILE=$REPO/crates/view-core/src/model.rs
OUT=$HOME/.claude/tmp/link-record/$(date +%Y%m%d-%H%M%S)
RATE=0
NEEDLE=
COLS=263
ROWS=88

while getopts "n:f:o:t:s:c:r:" opt; do
  case "$opt" in
    (n) RUNS=$OPTARG ;;
    (f) FILE=$OPTARG ;;
    (o) OUT=$OPTARG ;;
    (t) RATE=$OPTARG ;;
    (s) NEEDLE=$OPTARG ;;
    (c) COLS=$OPTARG ;;
    (r) ROWS=$OPTARG ;;
    (*) echo "link-record: see the header of $0" >&2; exit 2 ;;
  esac
done

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
  npm install --prefix "$DEPS" --silent
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

mkdir -p "$OUT"
echo "link-record: $RUNS runs per side into $OUT (needle $NEEDLE)"

CHILD=
cleanup() {
  if [ -n "$CHILD" ]; then
    kill "$CHILD" 2>/dev/null || true
  fi
}
trap cleanup EXIT

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
  cut -d ' ' -f 1 /proc/loadavg > "$dir/load" 2>/dev/null || echo 0 > "$dir/load"
  mkfifo "$dir/in"

  {
    echo "stty rows $ROWS cols $COLS"
    echo "export XDG_CONFIG_HOME=$FIXTURE"
    echo "export XDG_DATA_HOME=$PLUGINS"
    echo "export XDG_STATE_HOME=$dir/state"
    echo "export XDG_CACHE_HOME=$dir/cache"
    echo "export TERM=xterm-256color COLORTERM=truecolor"
    case "$side" in
      # view forwards the flag to the engine it spawns, so the engine's own
      # first screen update is recorded beside view's first content frame
      (view) echo "export VIEW_LOG=$dir/view.log"
             echo "exec $VIEW_BIN --startuptime $dir/startuptime $FILE" ;;
      (*) echo "exec $NVIM_BIN --startuptime $dir/startuptime $FILE" ;;
    esac
  } > "$dir/cmd.sh"

  if [ "$RATE" = "0" ]; then
    script -q -e -I "$dir/in.log" -O "$dir/out.log" -T "$dir/tm.log" \
      -c "bash $dir/cmd.sh" < "$dir/in" > "$dir/drain" 2>&1 &
  else
    script -q -e -I "$dir/in.log" -O "$dir/out.log" -T "$dir/tm.log" \
      -c "bash $dir/cmd.sh" < "$dir/in" 2>&1 \
      | NODE_PATH=$DEPS/node_modules node "$HERE/link-replay.js" \
          throttle "$RATE" &
  fi
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
  rm -f "$dir/in"
}

# The moments of one recording, as `name_ms=` lines.
replay_one() {
  dir=$1
  side=$2
  case "$side" in
    # view's palette is a framed box whose query row carries the typed `:`
    # inside its border; nvim draws its command line at the screen's foot
    (view) pattern='[|]?[[:space:]]*:' ;;
    (*) pattern='^:' ;;
  esac
  NODE_PATH=$DEPS/node_modules node "$HERE/link-replay.js" report \
    --out "$dir/out.log" --timing "$dir/tm.log" \
    --cols "$COLS" --rows "$ROWS" --needle "$NEEDLE" \
    --cmd-re "$pattern" --frames "$dir/frames" > "$dir/moments.txt"
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
printf 'side\trun\tload\tbusy\ttext_ms\thl_ms\tcolours\tcolon_ms\texit_ms\tengine_ms\tcontent_ms\n' \
  > "$RUNS_TSV"

index=1
while [ "$index" -le "$RUNS" ]; do
  for side in view nvim; do
    record_one "$side" "$index"
    dir=$OUT/$side-$index
    replay_one "$dir" "$side"
    load=$(cat "$dir/load")
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' \
      "$side" "$index" "$load" \
      "$(awk -v l="$load" 'BEGIN { print (l + 0 >= 2.0) ? "busy" : "" }')" \
      "$(moment "$dir/moments.txt" text_ms)" \
      "$(moment "$dir/moments.txt" highlight_ms)" \
      "$(moment "$dir/moments.txt" base_colours)" \
      "$(delta "$(moment "$dir/moments.txt" cmdline_ms)" \
               "$(moment "$dir/moments.txt" typed1_ms)")" \
      "$(delta "$(moment "$dir/moments.txt" handback_ms)" \
               "$(moment "$dir/moments.txt" typed3_ms)")" \
      "$(first_screen "$dir/startuptime")" \
      "$(content_frame "$dir/view.log")" >> "$RUNS_TSV"
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
  awk -F'\t' '{
    printf "%-5s %-4s %-6s %-5s %-9s %-9s %-8s %-9s %-9s %-10s %-11s\n", \
      $1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11
  }' "$RUNS_TSV"
  echo
  echo "spread per moment per side, milliseconds from the launch"
  for side in view nvim; do
    spread "$side" 5 text
    spread "$side" 6 highlight
    spread "$side" 8 colon
    spread "$side" 9 handback
    spread "$side" 10 engine
    spread "$side" 11 content
  done
} > "$TABLE"

cat "$TABLE"
echo
echo "frames and logs: $OUT"
