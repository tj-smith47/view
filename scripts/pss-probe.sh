#!/usr/bin/env bash
# Idle-footprint probe for the release binary, independent of the bench
# harness.
#
# The memory row of the bench matrix reports one number (pss_mb) and a
# breach of it says nothing about where the pages went. This reproduces that
# number outside the harness and decomposes it the two ways that attribute
# it: resident pages per mapping-and-permission (which tells apart code,
# read-only data and heap) and ELF section sizes (which tells what the
# linker put in the binary in the first place). A footprint breach that
# arrives with both is already halfway attributed.
#
# Idle, not post-workload, and deliberately so: this probe isolates the cost
# of merely existing -- binary pages faulted in during startup plus whatever
# the UI allocates to paint one screen -- from the workload growth the
# harness scenario measures on top. The two numbers are not interchangeable
# and the harness row remains the gated one.
#
#   bash scripts/pss-probe.sh [binary] [file-to-open]
#
# Defaults to target/release/view opening README.md at 120x40. Prints the
# host load, the commit and the binary's size in the header, because a PSS
# reading is only comparable against another reading that names all three.
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$root"

binary="${1:-target/release/view}"
open_file="${2:-README.md}"
cols="${PSS_PROBE_COLS:-120}"
rows="${PSS_PROBE_ROWS:-40}"
# The engine child has to finish starting before an idle reading means
# anything: a probe taken mid-startup measures how far the process got, not
# what it settles at.
settle_secs="${PSS_PROBE_SETTLE:-6}"
# Groups smaller than this fold into "other" so the mappings that carry the
# footprint are not buried under dozens of single-page entries.
report_floor_kb="${PSS_PROBE_FLOOR_KB:-16}"

for tool in python3 size; do
  command -v "$tool" >/dev/null 2>&1 || {
    echo "PSS PROBE FAIL: $tool not found on PATH" >&2
    exit 1
  }
done
[ -x "$binary" ] || { echo "PSS PROBE FAIL: $binary is not an executable" >&2; exit 1; }
[ -f "$open_file" ] || { echo "PSS PROBE FAIL: $open_file not found" >&2; exit 1; }

# Resolved once, so that the spawn below names the same file the header
# reported whatever shape the argument arrived in: a bare `view` must not
# reach `$PATH`, and an absolute path must not be prefixed into a relative
# one that resolves nowhere.
binary="$(cd "$(dirname "$binary")" && pwd)/$(basename "$binary")"

echo "== idle view PSS probe, $(date -u +%Y-%m-%dT%H:%M:%SZ) =="
uptime
git log --oneline -1 || true
ls -l "$binary"
echo "pty ${cols}x${rows}, settle ${settle_secs}s"

work="$(mktemp -d)"
pid_file="$work/pid"
driver="$work/pty-spawn.py"
child_pid=""
cleanup() {
  [ -n "$child_pid" ] && kill -TERM "$child_pid" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

# A sized pty rather than a pipe: view refuses to paint without a terminal,
# and the window size decides how many cells the surface allocates, so an
# unsized pty would make the reading depend on whatever size the kernel
# defaults to. The size is set on the slave before the fork, so the child
# cannot observe a resize between exec and its first query.
cat >"$driver" <<'PY'
import fcntl
import os
import pty
import struct
import sys
import termios

pid_file, cols, rows = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
argv = sys.argv[4:]

master, slave = pty.openpty()
fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", rows, cols, 0, 0))

pid = os.fork()
if pid == 0:
    os.setsid()
    fcntl.ioctl(slave, termios.TIOCSCTTY, 0)
    for target in (0, 1, 2):
        os.dup2(slave, target)
    os.close(master)
    if slave > 2:
        os.close(slave)
    os.execvp(argv[0], argv)

os.close(slave)
with open(pid_file, "w") as handle:
    handle.write(str(pid))

# Drain the child's paint output for as long as it lives. Left unread the
# pty buffer fills, the child blocks on write, and an idle-footprint probe
# would then be measuring a stalled process.
while True:
    try:
        if not os.read(master, 65536):
            break
    except OSError:
        break
PY

python3 "$driver" "$pid_file" "$cols" "$rows" "$binary" --clean "$open_file" >/dev/null 2>&1 &
driver_pid=$!

# The driver writes the pid only after its fork returns, so the file appears
# a moment after the background job starts.
for _ in $(seq 1 50); do
  [ -s "$pid_file" ] && break
  python3 -c 'import time; time.sleep(0.1)'
done
[ -s "$pid_file" ] || { echo "PSS PROBE FAIL: child pid never appeared" >&2; exit 1; }
child_pid="$(cat "$pid_file")"

python3 -c "import time; time.sleep($settle_secs)"

if [ ! -r "/proc/$child_pid/smaps_rollup" ]; then
  echo "PSS PROBE FAIL: pid $child_pid did not survive the settle window" >&2
  exit 1
fi

echo "pid=$child_pid cmdline=$(tr '\0' ' ' < "/proc/$child_pid/cmdline")"
echo "-- smaps_rollup --"
grep -E '^(Rss|Pss|Private_Clean|Private_Dirty|Shared_Clean|Shared_Dirty|Anonymous):' \
  "/proc/$child_pid/smaps_rollup"

echo "-- PSS by mapping+permission (kB) --"
awk -v floor="$report_floor_kb" '
  /^[0-9a-f]+-[0-9a-f]+ / {
    perms = $2
    path = ""
    for (i = 6; i <= NF; i++) path = (path == "" ? $i : path " " $i)
    if (path == "") path = "[anon]"
    n = split(path, parts, "/")
    base = parts[n]
    # Anonymous, heap and stack mappings are named by kind rather than by
    # file, and splitting them by permission bits would report the same
    # allocator arena under several headings.
    key = (base ~ /^\[/) ? base : base " " perms
    next
  }
  /^Pss:/ { pss[key] += $2 }
  END {
    other = 0
    for (k in pss) {
      if (pss[k] < floor) { other += pss[k]; continue }
      printf "%d %s\n", pss[k], k
    }
    if (other > 0) printf "%d other (each < %d kB)\n", other, floor
  }
' "/proc/$child_pid/smaps" | sort -rn

echo "-- ELF section sizes (bytes) --"
size -A -d "$binary" | awk 'NF == 3 && $2 ~ /^[0-9]+$/ { printf "%d %s\n", $2, $1 }' \
  | sort -rn | head -12

kill -TERM "$child_pid" 2>/dev/null || true
wait "$driver_pid" 2>/dev/null || true
child_pid=""
echo "-- done --"
uptime
