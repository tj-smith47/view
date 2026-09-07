#!/usr/bin/env bash
# Case matrix for the two width walks in check-style.sh (the Lua chunk walk
# and the multi-line string literal walk) and for its acceptance-expectation
# ban. Every case builds a scratch tree, points the check at it, and asserts
# BOTH the exit status and the
# exact set of reported lines: a case expecting one over-width line has to
# fail when a second one is reported, and a case expecting silence has to
# fail when a walk narrows itself and reports nothing for the wrong reason.
#
#   bash scripts/check-style-cases.sh
#   bash scripts/check-style-cases.sh --checker /path/to/copy
#
# Written to stock POSIX-ish bash: macOS ships /bin/bash 3.2, and the walks'
# own portability (BSD awk, BSD find, BSD grep) is only proven by running
# this there. Both empty-population guards below are unreachable on a real
# tree, so nothing else in the gate exercises them; the three blind spots
# the width checks have shipped (a content heuristic, hardcoded counts, a
# silent exit under set -e) were each found by hand, and these cases are
# those fixtures frozen so the next one trips here instead.
set -uo pipefail

CHECKER=""
while [ $# -gt 0 ]; do
  case "$1" in
    --checker)
      CHECKER="${2:-}"
      shift 2
      ;;
    -h | --help)
      printf 'usage: %s [--checker PATH]\n' "$0"
      exit 0
      ;;
    *)
      printf 'unknown argument: %s\n' "$1" >&2
      exit 2
      ;;
  esac
done
# Resolved from this file's own directory rather than $PWD, so a run started
# from anywhere grades the checker that ships beside these cases.
if [ -z "$CHECKER" ]; then
  CHECKER="$(cd "$(dirname "$0")" && pwd)/check-style.sh"
fi
if [ ! -f "$CHECKER" ]; then
  printf 'checker not found: %s\n' "$CHECKER" >&2
  exit 2
fi

printf 'checker under test: %s\n' "$CHECKER"

WORK=$(mktemp -d "${TMPDIR:-/tmp}/check-style-cases.XXXXXX")
trap 'rm -rf "$WORK"' EXIT

n=0
failures=0
CASE=""

SRC='crates/view-engine/src'
TESTS='crates/view-engine/tests'
NVIM='crates/view-engine/src/nvim_api.rs'
LIT='crates/view-engine/tests/lit.rs'

new_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/$SRC" "$CASE/$TESTS"
}

# A line of exactly WIDTH columns, opened by PREFIX and closed by SUFFIX,
# padded between them. The widths are computed rather than written out: a
# fixture whose 81st column came from a hand-counted string is one editor
# reflow away from testing 79.
pad() {
  fill=$(($1 - ${#2} - ${#3}))
  printf '%s%s%s\n' "$2" "$(printf "%${fill}s" '' | tr ' ' 'a')" "$3"
}

# The same line padded with a three-byte character rather than an ASCII one:
# a walk counting bytes calls this three times its width, which is how a
# 77-column comment reddened beside a wider one whose long token was
# subtracted whole.
pad_wide() {
  fill=$(($1 - ${#2} - ${#3}))
  printf '%s%s%s\n' "$2" \
    "$(awk -v n="$fill" 'BEGIN { while (i++ < n) printf "\342\224\200" }')" "$3"
}

# The chunk walk's one shape: a const declaration opening a concat!, whose
# body line is padded to the width the case is about.
plant_chunk() {
  {
    printf 'const A_CHUNK: &str = concat!(\n'
    "${2:-pad}" "$1" '    "' '\n",'
    printf ');\n'
  } > "$CASE/$NVIM"
}

# Both literal opener shapes in one file: the literal that opens on its own
# line (padded to the first width) and the one sharing the line with the
# assignment that carries it (whose body carries the second width).
plant_literals() {
  {
    printf 'fn a() {\n    let s = format!(\n'
    "${3:-pad}" "$1" '        "' ' \'
    printf '        continues here");\n}\n'
    printf 'const M: &str = "\\\n'
    "${3:-pad}" "$2" '' '\'
    printf 'the tail line here";\n'
  } > "$CASE/$LIT"
}

# Both walks report the same two ways: `STYLE FAIL:` headers, each of which
# names one guard, and `file:line: N columns` lines naming an over-width
# line. Headers collapse to a guard token so that rewording a diagnostic is
# not a regression, while the two mismatch guards keep both of their counts
# (walked/declared) -- a mismatch reporting the wrong pair is the bug those
# guards exist to catch, and the count is the whole content of the report.
# The advice lines under a header are not tokens: they repeat no fact.
findings() {
  awk '
    /^STYLE FAIL: .* missing; cannot check Lua chunk width$/ {
      print "chunk-missing"; next
    }
    /^STYLE FAIL: no _CHUNK declaration found/ { print "chunk-none"; next }
    /^STYLE FAIL: the width check walked [0-9]+ Lua chunks/ {
      walked = $7; guard = "chunk-walk"; next
    }
    /^STYLE FAIL: a Lua chunk line is over 80 columns$/ {
      print "chunk-width"; next
    }
    /^STYLE FAIL: no view-engine sources found/ { print "lit-missing"; next }
    /^STYLE FAIL: no multi-line string literal found/ {
      print "lit-none"; next
    }
    /^STYLE FAIL: the width check walked [0-9]+ multi-line string literals/ {
      walked = $7; guard = "lit-walk"; next
    }
    /^STYLE FAIL: a line inside a string literal is over 80 columns$/ {
      print "lit-width"; next
    }
    /^ +but grep counts [0-9]+ / {
      if (guard != "") { print guard ":" walked "/" $4; guard = "" }
      next
    }
    /:[0-9]+: [0-9]+ columns$/ {
      loc = $1; sub(/:$/, "", loc); print loc ":" $2; next
    }
  ' | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//'
}

expect() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "$CHECKER" --widths "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | findings)
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'not ok %s - %s\n  want rc=%s findings [%s]\n  got  rc=%s findings [%s]\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got"
  printf '%s\n' "$out" | sed 's/^/  | /'
}

# ---------------------------------------------------------------------------
# a tree both walks reach fully, with nothing to report
# ---------------------------------------------------------------------------
new_case
plant_chunk 60
plant_literals 60 60
expect 0 '' 'a tree whose chunk and both literal shapes are inside the width'

new_case
plant_chunk 80
plant_literals 80 80
expect 0 '' 'exactly 80 columns in each of the three shapes is inside the width'

# ---------------------------------------------------------------------------
# one column past, in each shape: the finding is the line, and only it
# ---------------------------------------------------------------------------
new_case
plant_chunk 81
plant_literals 60 60
expect 1 "chunk-width $NVIM:2:81" 'a chunk body line one column over'

new_case
plant_chunk 60
plant_literals 81 60
expect 1 "$LIT:3:81 lit-width" 'a literal opening on its own line, one column over'

new_case
plant_chunk 60
plant_literals 60 81
expect 1 "$LIT:7:81 lit-width" 'a literal opened by an assignment, its body one column over'

# ---------------------------------------------------------------------------
# the same shapes written in a character wider than a byte: the walks count
# characters, which is what an editor shows and what the exemption for a
# long run is counted in, so the two cannot disagree about which of two
# lines is the wider
# ---------------------------------------------------------------------------
new_case
plant_chunk 77 pad_wide
plant_literals 77 77 pad_wide
expect 0 '' 'the three shapes inside the width in characters and past it in bytes'

new_case
plant_chunk 81 pad_wide
plant_literals 60 60
expect 1 "chunk-width $NVIM:2:81" 'a chunk body line of wide characters one column over'

new_case
plant_chunk 60
plant_literals 81 60 pad_wide
expect 1 "$LIT:3:81 lit-width" 'a literal of wide characters one column over'

# ---------------------------------------------------------------------------
# a declaration shape that drifts out of a walk's own match: the walk reaches
# less than the grep counts, which is the one direction that would otherwise
# pass silently while vouching for lines nobody read
# ---------------------------------------------------------------------------
new_case
plant_chunk 60
plant_literals 60 60
sed 's/^const A_CHUNK/pub static A_CHUNK/' "$CASE/$NVIM" > "$CASE/$NVIM.tmp"
mv "$CASE/$NVIM.tmp" "$CASE/$NVIM"
expect 1 'chunk-walk:0/1' 'a chunk declared as a static is counted but not walked'

new_case
plant_chunk 60
plant_literals 60 60
printf 'const N: &str = "\nnot followed";\n' >> "$CASE/$LIT"
expect 1 'lit-walk:2/3' 'a literal opener with no continuation backslash is counted but not walked'

# ---------------------------------------------------------------------------
# empty populations: a walk that reached nothing must say so rather than
# report a clean tree, and neither can happen on a real checkout
# ---------------------------------------------------------------------------
new_case
printf 'fn a() {}\n' > "$CASE/$NVIM"
plant_literals 60 60
expect 1 'chunk-none' 'a nvim_api.rs carrying no chunk declaration at all'

new_case
plant_chunk 60
expect 1 'lit-none' 'a view-engine carrying no multi-line string literal at all'

new_case
rm -rf "$CASE/crates"
expect 1 'chunk-missing lit-missing' 'a tree with no crate path for either walk to read'

# ---------------------------------------------------------------------------
# the acceptance-expectation ban: an expected color comes from a probe of the
# live scheme, never out of a config's text. The three evasions below are the
# shapes the deleted code actually had -- a path behind a variable, a reader
# wrapped onto its own line, a pipe between the two -- each of which slips
# past any pattern that tries to see a reader and a path on one line
# ---------------------------------------------------------------------------
ACC='scripts/acceptance/leg.sh'

new_accept_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/scripts/acceptance/fixtures/themed/nvim/colors"
  printf "vim.api.nvim_set_hl(0, 'Normal', { bg = '#282a36' })\n" \
    > "$CASE/scripts/acceptance/fixtures/themed/nvim/colors/scheme.lua"
}

expect_accept() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "$CHECKER" --acceptance "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | awk '
    /^STYLE FAIL: an acceptance script names a colorscheme file/ {
      print "names-scheme"; next
    }
    /^STYLE FAIL: a color literal in an acceptance script/ {
      print "color-literal"; next
    }
  ' | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//')
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'FAIL %s - %s\n  want rc=%s [%s]\n  got  rc=%s [%s]\n%s\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got" "$out"
}

new_accept_case
{
  printf '#!/usr/bin/env bash\n'
  printf 'SCHEME=$DIR/fixtures/themed/nvim/colors/scheme.lua\n'
  printf 'bg=$(sed -n "s/.*bg = .\\(......\\)./\\1/p" \\\n'
  printf '    "$SCHEME")\n'
} > "$CASE/$ACC"
expect_accept 1 'names-scheme' 'a path in a variable, its reader wrapped onto another line'

new_accept_case
{
  printf '#!/usr/bin/env bash\n'
  printf 'bg=$(cat fixtures/themed/nvim/colors/scheme.lua | sed -n 1p)\n'
} > "$CASE/$ACC"
expect_accept 1 'names-scheme' 'a reader with a pipe between it and the path'

new_accept_case
{
  printf '#!/usr/bin/env bash\n'
  printf 'want=%s\n' '"#282a36"'
} > "$CASE/$ACC"
expect_accept 1 'color-literal' 'an expected color written into the script itself'

new_accept_case
{
  printf '#!/usr/bin/env bash\n'
  printf 'cp -R "$FIXTURE/nvim" "$CONFIG_HOME/nvim"\n'
  printf 'PALETTE=$(nvim --headless -c "luafile $probe" -c qa!)\n'
} > "$CASE/$ACC"
expect_accept 0 '' 'a leg that copies a whole config and probes the live scheme'

# ---------------------------------------------------------------------------
# the written-program pin: a program a test runs is a committed fixture, so
# the set of sources that make a file runnable is pinned per file. Its own
# blind spot is a walk that stops matching the spelling a site uses, which
# reads exactly like a clean tree -- the shape below plants each population
# in the UFCS spelling the site this pin exists for actually used, which is
# the one a call-anchored pattern reads straight past.
#
# The four rows mirror the checker's own WRITTEN_PROGRAM_SITES: a pin that
# legitimately moves moves here too, and a clean case that stopped matching
# the tree is a case nobody watched fail.
# ---------------------------------------------------------------------------
plant_modes() {
  mkdir -p "$(dirname "$CASE/$1")"
  : > "$CASE/$1"
  i=0
  while [ "$i" -lt "$2" ]; do
    printf 'std::os::unix::fs::PermissionsExt::set_mode(&mut perms, 0o755);\n' \
      >> "$CASE/$1"
    i=$((i + 1))
  done
}

new_written_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/crates"
  plant_modes 'crates/view-ai/src/provision.rs' 3
  plant_modes 'crates/view-engine/src/process.rs' 3
  plant_modes 'crates/view-engine/tests/checktime_live.rs' 1
  plant_modes 'crates/view-oracle/tests/smoke.rs' 2
}

# The report's whole content is the population it walked, so the found rows
# are the tokens; the header collapses to a guard name so rewording the
# advice under it is not a regression.
expect_written() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "$CHECKER" --written-programs "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | awk '
    /^found:$/ { infound = 1; next }
    /^STYLE FAIL: a source makes a file executable outside the pinned set$/ {
      infound = 0; print "written-programs"; next
    }
    infound && NF == 2 { print $1 "=" $2 }
  ' | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//')
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'not ok %s - %s\n  want rc=%s findings [%s]\n  got  rc=%s findings [%s]\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got"
  printf '%s\n' "$out" | sed 's/^/  | /'
}

new_written_case
expect_written 0 '' 'the pinned population, in the spelling a call-anchored pattern misses'

new_written_case
plant_modes 'crates/view-native/src/tree/git.rs' 1
expect_written 1 "crates/view-ai/src/provision.rs=3 \
crates/view-engine/src/process.rs=3 \
crates/view-engine/tests/checktime_live.rs=1 \
crates/view-native/src/tree/git.rs=1 crates/view-oracle/tests/smoke.rs=2 \
written-programs" \
  'a source outside the pinned set writing a program of its own'

new_written_case
plant_modes 'crates/view-oracle/tests/smoke.rs' 3
expect_written 1 "crates/view-ai/src/provision.rs=3 \
crates/view-engine/src/process.rs=3 \
crates/view-engine/tests/checktime_live.rs=1 crates/view-oracle/tests/smoke.rs=3 \
written-programs" \
  'a second site inside a file the pin already lists'

new_written_case
rm -f "$CASE/crates/view-engine/tests/checktime_live.rs"
expect_written 1 "crates/view-ai/src/provision.rs=3 \
crates/view-engine/src/process.rs=3 crates/view-oracle/tests/smoke.rs=2 \
written-programs" \
  'a pinned site that no longer exists, which the pin would otherwise vouch for'

# ---------------------------------------------------------------------------
# the tied-spawn pin: a long-lived child goes out through view-proc's spawn,
# and every other spawn says in a row of its own how it cannot outlive its
# parent. Its blind spot is the same as the walk above's -- a spelling the
# pattern stops matching reads exactly like a tree with no new spawns -- so
# each population below is planted in all three spellings the tree uses --
# `Command::new`, portable-pty's `CommandBuilder::new`, and the `.spawn(`
# that starts one -- with a doc comment quoting the same call to prove the
# classifier drops it.
#
# The rows mirror the checker's own TIED_SPAWN_SITES.
# ---------------------------------------------------------------------------
plant_spawns() {
  mkdir -p "$(dirname "$CASE/$1")"
  : > "$CASE/$1"
  printf '/// A doc comment naming Command::new and .spawn(), neither a spawn.\n' >> "$CASE/$1"
  i=0
  while [ "$i" -lt "$2" ]; do
    case $((i % 3)) in
      0) printf 'let mut cmd = Command::new(program);\n' >> "$CASE/$1" ;;
      1) printf 'let mut cmd = CommandBuilder::new(program);\n' >> "$CASE/$1" ;;
      *) printf 'let worker = builder.spawn(move || run());\n' >> "$CASE/$1" ;;
    esac
    i=$((i + 1))
  done
}

new_tied_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/crates"
  # the walk asks the god-file scanner which lines are production, and that
  # scanner reads tracked files: a scratch root has to be a repository for
  # the classifier to see anything at all
  git -C "$CASE" init -q
  plant_spawns 'crates/view-ai/src/acp/session.rs' 5
  plant_spawns 'crates/view-ai/src/provision.rs' 3
  plant_spawns 'crates/view-ai/src/watch.rs' 5
  plant_spawns 'crates/view-bench/src/remote_ui.rs' 1
  plant_spawns 'crates/view-bench/src/scenarios/echo_speculated_rtt.rs' 3
  plant_spawns 'crates/view-bench/src/session.rs' 1
  plant_spawns 'crates/view-engine/src/process.rs' 4
  plant_spawns 'crates/view-harness/src/bin/bench.rs' 1
  plant_spawns 'crates/view-harness/src/bin/bench/replicates.rs' 1
  plant_spawns 'crates/view-harness/src/bin/oracle/compat.rs' 3
  plant_spawns 'crates/view-harness/src/fixture.rs' 1
  plant_spawns 'crates/view-native/src/tree/git.rs' 2
  plant_spawns 'crates/view-oracle/src/compat.rs' 4
  plant_spawns 'crates/view-oracle/src/hang.rs' 1
  plant_spawns 'crates/view-oracle/src/pty.rs' 2
  plant_spawns 'crates/view-oracle/src/remote.rs' 1
  plant_spawns 'crates/view-proc/src/lib.rs' 1
  plant_spawns 'crates/view-test-support/src/lib.rs' 1
  plant_spawns 'crates/view/src/ai_context_worker.rs' 1
  plant_spawns 'crates/view/src/clipboard.rs' 2
  plant_spawns 'crates/view/src/remote_guard.rs' 2
  plant_spawns 'crates/view/src/runtime.rs' 1
}

# The pinned population is long, so a failing case is graded on the rows that
# differ from it rather than on the whole listing: what a case proves is that
# the walk saw the planted change, and the header collapses to a guard name
# the same way the walk above's does.
expect_tied() {
  want_rc="$1"
  want="$2"
  desc="$3"
  git -C "$CASE" add -A
  out=$(bash "$CHECKER" --tied-spawns "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | awk '
    /^pinned:$/ { inpinned = 1; next }
    /^found:$/ { inpinned = 0; infound = 1; next }
    /^STYLE FAIL: a production spawn site outside the pinned set$/ {
      infound = 0; print "tied-spawns"; next
    }
    inpinned && NF == 2 { pinned[$1 "=" $2] = 1; next }
    infound && NF == 2 { if (!($1 "=" $2 in pinned)) print $1 "=" $2 }
  ' | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//')
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'not ok %s - %s\n  want rc=%s findings [%s]\n  got  rc=%s findings [%s]\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got"
  printf '%s\n' "$out" | sed 's/^/  | /'
}

new_tied_case
expect_tied 0 '' 'the pinned population, in all three spellings, with a doc comment quoting the call'

new_tied_case
plant_spawns 'crates/view/src/startup.rs' 1
expect_tied 1 'crates/view/src/startup.rs=1 tied-spawns' \
  'a production file outside the pinned set spawning a child of its own'

new_tied_case
plant_spawns 'crates/view-bench/src/remote_ui.rs' 2
expect_tied 1 'crates/view-bench/src/remote_ui.rs=2 tied-spawns' \
  'a second spawn inside a file the pin already lists'

new_tied_case
printf 'let worker = std::thread::Builder::new().spawn(move || run());\n' \
  > "$CASE/crates/view/src/startup.rs"
expect_tied 1 'crates/view/src/startup.rs=1 tied-spawns' \
  'a file whose only spawn is the call, with no constructor beside it'

new_tied_case
rm -f "$CASE/crates/view-oracle/src/hang.rs"
expect_tied 1 'tied-spawns' \
  'a pinned site that no longer exists, which the pin would otherwise vouch for'

# ---------------------------------------------------------------------------
# the geometry pin: every production site that names a (width, height) to
# the engine is pinned per file with where that pair came from, because a
# site spending the terminal own reading instead of grid_target_for is
# refused by the engine below ENGINE_MIN_SIZE and relayouts every window
# above it. Its blind spots are a spelling the pattern stops matching and a
# classifier that reads a comment where the code has none, so the population
# below is planted in all six spellings the tree uses -- the attach as a
# method, as an effect variant and as the wire method name, and the resize
# the same three ways -- with a doc comment quoting the call and a string
# holding a // ahead of one.
#
# The rows mirror the checker own GEOMETRY_SITES.
# ---------------------------------------------------------------------------
plant_geometry() {
  mkdir -p "$(dirname "$CASE/$1")"
  : > "$CASE/$1"
  printf '/// A doc comment naming ui_attach and try_resize(, neither a call.\n' >> "$CASE/$1"
  i=0
  while [ "$i" -lt "$2" ]; do
    case $((i % 6)) in
      (0) printf 'ops.ui_attach(width, height, surfaces)?;\n' >> "$CASE/$1" ;;
      (1) printf 'Effect::Rpc(RpcCall::UiAttach { width, height })\n' >> "$CASE/$1" ;;
      (2) printf 'handle.request("nvim_ui_attach", args)?;\n' >> "$CASE/$1" ;;
      (3) printf 'ops.try_resize(width, height)?;\n' >> "$CASE/$1" ;;
      (4) printf 'Effect::Rpc(RpcCall::TryResize { width, height })\n' >> "$CASE/$1" ;;
      (*) printf 'handle.request("nvim_ui_try_resize", args)?;\n' >> "$CASE/$1" ;;
    esac
    i=$((i + 1))
  done
}

# A bare `release(`, the spelling that carries a geometry only in a crate
# that owns an engine attach.
plant_release() {
  mkdir -p "$(dirname "$CASE/$1")"
  : > "$CASE/$1"
  i=0
  while [ "$i" -lt "$2" ]; do
    printf 'guard.release(size);\n' >> "$CASE/$1"
    i=$((i + 1))
  done
}

new_geometry_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/crates"
  # the walk asks the god-file scanner which lines are production, and that
  # scanner reads tracked files: a scratch root has to be a repository for
  # the classifier to see anything at all
  git -C "$CASE" init -q
  plant_geometry 'crates/view-core/src/model.rs' 1
  plant_geometry 'crates/view-core/src/msg.rs' 2
  plant_release 'crates/view-core/src/update/ai_fs.rs' 4
  plant_geometry 'crates/view-core/src/update/mod.rs' 1
  plant_geometry 'crates/view-core/src/update/ui_event.rs' 1
  plant_geometry 'crates/view-engine/src/nvim_api.rs' 7
  plant_geometry 'crates/view-oracle/src/hang.rs' 4
  plant_geometry 'crates/view-oracle/src/lib.rs' 2
  plant_geometry 'crates/view-oracle/src/reference.rs' 2
  plant_geometry 'crates/view-oracle/src/speculate.rs' 2
  plant_geometry 'crates/view/src/engine_ops.rs' 14
  plant_release 'crates/view/src/main.rs' 1
  plant_geometry 'crates/view/src/native.rs' 1
  plant_geometry 'crates/view/src/runtime/executor.rs' 3
  plant_geometry 'crates/view/src/startup.rs' 5
}

# Graded on the rows that differ from the pinned listing, the way the
# tied-spawn cases above are: what a case proves is that the walk saw the
# planted change.
expect_geometry() {
  want_rc="$1"
  want="$2"
  desc="$3"
  git -C "$CASE" add -A
  out=$(bash "$CHECKER" --geometry-sites "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | awk '
    /^pinned:$/ { inpinned = 1; next }
    /^found:$/ { inpinned = 0; infound = 1; next }
    /^STYLE FAIL: a production geometry site outside the pinned set$/ {
      infound = 0; print "geometry-sites"; next
    }
    inpinned && NF == 2 { pinned[$1 "=" $2] = 1; next }
    infound && NF == 2 { if (!($1 "=" $2 in pinned)) print $1 "=" $2 }
  ' | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//')
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'not ok %s - %s\n  want rc=%s findings [%s]\n  got  rc=%s findings [%s]\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got"
  printf '%s\n' "$out" | sed 's/^/  | /'
}

new_geometry_case
expect_geometry 0 '' 'the pinned population, in all six spellings, with a doc comment quoting the call'

new_geometry_case
plant_geometry 'crates/view-scratch/src/lib.rs' 1
expect_geometry 1 'crates/view-scratch/src/lib.rs=1 geometry-sites' \
  'a production file outside the pinned set naming a geometry of its own'

new_geometry_case
printf 'let doc = "https://example.invalid/x"; ops.try_resize(width, height)?;\n' \
  >> "$CASE/crates/view/src/startup.rs"
expect_geometry 1 'crates/view/src/startup.rs=6 geometry-sites' \
  'a site behind a string holding a // on the same line'

new_geometry_case
plant_release 'crates/view-harness/src/fixture.rs' 1
expect_geometry 0 '' 'a lock release in a crate that owns no engine attach'

# ---------------------------------------------------------------------------
# the prose width gate: a page wraps at 80 columns, and what cannot wrap is
# exempt by shape rather than by a list of files -- a fence is a sample of a
# file, a row is a row, a heading is one line by construction, a link has
# nowhere to break, and a token longer than the limit cannot be helped by
# wrapping, which is why the token and not the line it stands in is what the
# walk takes out. Counted in characters, by a measure that gives the same
# verdict on an awk that decodes UTF-8 and one that does not
# ---------------------------------------------------------------------------
new_width_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/docs"
  printf '# view\n\nA line inside the limit.\n' > "$CASE/README.md"
  printf '# page\n\nAnother line inside the limit.\n' > "$CASE/docs/page.md"
}

# 81 columns, built rather than written out: a case that counts its own
# width by hand is a case that stops meaning 81 the moment someone edits it.
over() {
  awk -v n="$1" 'BEGIN { line = "x"; while (length(line) < n - 5) { line = line "x" }; printf "%s %s\n", line, "tail" }'
}

expect_width() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "$CHECKER" --prose-width "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | awk '
    /^[^ ].*:[0-9]+: [0-9]+ columns$/ { c = $1; sub(/:$/, "", c); print c; next }
    /^[^ ].*: the width walk cannot read it$/ { c = $1; sub(/:$/, "", c); print c ":unreadable"; next }
    /^STYLE FAIL: no markdown page found/ { print "empty"; next }
  ' | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//')
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'FAIL %s - %s\n  want rc=%s [%s]\n  got  rc=%s [%s]\n%s\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got" "$out"
}

new_width_case
expect_width 0 '' 'a tree whose pages are inside the width'

new_width_case
over 80 >> "$CASE/docs/page.md"
expect_width 0 '' 'a prose line at exactly the width'

new_width_case
over 81 >> "$CASE/docs/page.md"
expect_width 1 'docs/page.md:4' 'a prose line one column over'

new_width_case
{ printf '## '; over 81; } >> "$CASE/docs/page.md"
expect_width 0 '' 'a heading over the width, which carries no newline to wrap at'

new_width_case
{ printf '| '; over 81; } >> "$CASE/docs/page.md"
expect_width 0 '' 'a table row over the width'

new_width_case
{ printf '```\n'; over 81; printf '```\n'; } >> "$CASE/docs/page.md"
expect_width 0 '' 'a fenced sample of a file over the width'

new_width_case
{ printf '```\n'; over 81; } >> "$CASE/docs/page.md"
printf '\n' >> "$CASE/README.md"
over 81 >> "$CASE/README.md"
expect_width 1 'README.md:5' 'a fence left open on one page, which does not silence the next'

new_width_case
printf 'https://example.com/%s\n' "$(over 81 | tr -d ' ')" >> "$CASE/docs/page.md"
expect_width 0 '' 'a line that is one URL'

new_width_case
printf '[![CI](https://example.com/a/very/long/badge/path/%s)](https://example.com/b)\n' \
  "$(over 81 | tr -d ' ')" >> "$CASE/docs/page.md"
expect_width 0 '' 'a badge, which is a link wrapping an image'

new_width_case
printf 'A path this long %s cannot be wrapped under the limit.\n' \
  "$(over 90 | tr -d ' ')" >> "$CASE/docs/page.md"
expect_width 0 '' 'a line carrying a token longer than the limit itself'

new_width_case
{ printf '<!-- generated from SURFACES -->\n'; over 81; } >> "$CASE/docs/page.md"
expect_width 1 'docs/page.md:5' \
  'a block under a generated marker, which the program that writes it wraps'

new_width_case
{ printf '~~~\n'; over 81; printf '~~~\n'; } >> "$CASE/docs/page.md"
expect_width 0 '' 'a tilde-fenced sample of a file, which is a fence CommonMark spells twice'

new_width_case
printf '%s\n' "$(over 90 | tr -d ' ')" >> "$CASE/docs/page.md"
expect_width 0 '' 'a token longer than the limit standing alone on its line'

new_width_case
printf 'This sentence is prose and wraps like prose, and the path %s does not buy it out of the limit.\n' \
  "$(over 90 | tr -d ' ')" >> "$CASE/docs/page.md"
expect_width 1 'docs/page.md:4' \
  'the same token with a sentence beside it, whose prose alone runs past the limit'

new_width_case
{ printf '```\n~~~\n'; over 81; printf '```\n'; } >> "$CASE/docs/page.md"
pad 90 'A line of prose ' ' past the limit' >> "$CASE/docs/page.md"
expect_width 1 'docs/page.md:8' \
  'a backtick block holding a tilde line, which closes neither the block nor the grading after it'

new_width_case
printf 'A rule of box characters %s ends the section\n' \
  '── ── ── ── ── ── ── ── ── ── ──' >> "$CASE/docs/page.md"
expect_width 0 '' \
  'a prose line of 77 columns and 125 bytes, whose runs are too short to buy the exemption a byte measure would need'

new_width_case
pad_wide 81 'A rule of box characters ' ' ends the section' >> "$CASE/docs/page.md"
expect_width 1 'docs/page.md:4' 'a prose line of wide characters one column over'

new_width_case
mkdir "$CASE/docs/adir.md"
expect_width 1 'docs/adir.md:unreadable' \
  'a directory named like a page, which awk skips with a warning and a zero status'

new_width_case
ln -s missing.md "$CASE/docs/dangling.md"
expect_width 1 'docs/dangling.md:unreadable' \
  'a page the walk cannot read, which a discarded status would pass as clean'

new_width_case
rm -f "$CASE/README.md" "$CASE/docs/page.md"
expect_width 1 'empty' 'a tree with no page for the walk to read'

# ---------------------------------------------------------------------------
# the comment rules over scripts/: a comment wraps at the same column a page
# does, cites no review finding and no planning document, and all three rules
# grade the one shebang-selected population the portability legs grade --
# the remote-test fixtures carry no suffix, so a rule spelled over *.sh
# grades a subset of its sibling's. Counted in characters, as the page walk
# counts, so a rule drawn in box characters is judged by what an editor
# shows rather than by what it costs in bytes
# ---------------------------------------------------------------------------
new_script_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/scripts"
  printf '#!/usr/bin/env bash\n# a comment inside the limit\ntrue\n' \
    > "$CASE/scripts/gate.sh"
}

# The two citations the bans below are cased with, assembled rather than
# written out: this file is inside the population those bans grade, and a
# case that spells one plants the drift it exists to catch.
CITE_FINDING='found in'' review'
CITE_PLAN='char''ter'

# A fixture the remote tests exec by path: no suffix, and a script only
# because its first line says so.
plant_fixture() {
  printf '#!/bin/sh\n# %s\nexit 0\n' "$1" > "$CASE/scripts/relay"
  chmod +x "$CASE/scripts/relay"
}

expect_script_comments() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "$CHECKER" --script-comments "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | awk '
    /^[^ ].*:[0-9]+: [0-9]+ columns$/ { c = $1; sub(/:$/, "", c); print c; next }
    /^[^ ].*: the comment rules cannot read it$/ {
      c = $1; sub(/:$/, "", c); print c ":unreadable"; next
    }
    /^STYLE FAIL: no script found/ { print "empty"; next }
    /^STYLE FAIL: review-finding reference/ { print "finding-ban"; next }
    /^STYLE FAIL: planning-/ { print "plan-ban"; next }
    /^[^ :]+:[0-9]+:/ { split($0, f, ":"); print f[1] ":" f[2]; next }
  ' | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//')
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'FAIL %s - %s\n  want rc=%s [%s]\n  got  rc=%s [%s]\n%s\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got" "$out"
}

new_script_case
expect_script_comments 0 '' 'a script whose comments are inside the limit'

new_script_case
pad 80 '# a comment ' ' at the limit' >> "$CASE/scripts/gate.sh"
expect_script_comments 0 '' 'a comment at exactly the width'

new_script_case
pad 85 '# a comment ' ' past the limit' >> "$CASE/scripts/gate.sh"
expect_script_comments 1 'scripts/gate.sh:4' 'a comment five columns over'

new_script_case
printf '# a path this long %s cannot be wrapped under the limit\n' \
  "$(pad 90 '' '' | tr -d ' ')" >> "$CASE/scripts/gate.sh"
expect_script_comments 0 '' 'a comment carrying a token longer than the limit itself'

new_script_case
pad 81 '# a comment ' ' past the limit' >> "$CASE/scripts/gate.sh"
expect_script_comments 1 'scripts/gate.sh:4' 'a comment one column over'

new_script_case
printf '# ── why the gated item is measured, not truncated at the marker ────────────\n' \
  >> "$CASE/scripts/gate.sh"
expect_script_comments 0 '' \
  'a rule drawn in box characters: 77 columns, and 105 bytes a byte measure reddens'

new_script_case
pad 85 'true # a comment ' ' past the limit' >> "$CASE/scripts/gate.sh"
expect_script_comments 0 '' \
  'an over-wide comment beside code, which has nowhere to wrap without moving the code'

new_script_case
pad 85 '# a comment ' ' past the limit' > "$CASE/scripts/notes.txt"
expect_script_comments 0 '' \
  'an over-wide line in a file under scripts/ that no shebang makes a script'

new_script_case
plant_fixture 'a comment inside the limit'
pad 85 '# a comment ' ' past the limit' >> "$CASE/scripts/relay"
expect_script_comments 1 'scripts/relay:4' \
  'an over-wide comment in a suffix-less fixture, which a *.sh population misses'

new_script_case
plant_fixture "the retry loop, $CITE_FINDING to spin on a closed pipe"
expect_script_comments 1 'finding-ban scripts/relay:2' \
  'a review-finding citation in a suffix-less fixture, which the same population reaches'

new_script_case
plant_fixture "the leg the $CITE_PLAN asks for"
expect_script_comments 1 'plan-ban scripts/relay:2' \
  'a planning-document citation in a suffix-less fixture'

new_script_case
ln -s missing.sh "$CASE/scripts/gone.sh"
expect_script_comments 1 'scripts/gone.sh:unreadable' \
  'a script the rules cannot read, which a dropped selection would pass as clean'

new_script_case
rm -f "$CASE/scripts/gate.sh"
expect_script_comments 1 'empty' 'a tree with no script for the walk to read'

printf '\n%s cases, %s failures\n' "$n" "$failures"
[ "$failures" -eq 0 ]
