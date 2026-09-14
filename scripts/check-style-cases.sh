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

# A line of exactly WIDTH characters, opened by PREFIX and closed by
# SUFFIX, padded between them. The widths are computed rather than written
# out: a fixture whose 81st character came from a hand-counted string is one
# editor reflow away from testing 79.
pad() {
  fill=$(($1 - ${#2} - ${#3}))
  printf '%s%s%s\n' "$2" "$(printf "%${fill}s" '' | tr ' ' 'a')" "$3"
}

# The same line padded with a three-byte character rather than an ASCII one:
# a walk counting bytes calls this three times its width, which is how a
# 77-character comment reddened beside a wider one whose long token was
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
# names one guard, and `file:line: N characters` lines naming an over-width
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
    /^STYLE FAIL: a Lua chunk line is over 80 characters$/ {
      print "chunk-width"; next
    }
    /^STYLE FAIL: no view-engine sources found/ { print "lit-missing"; next }
    /^STYLE FAIL: no multi-line string literal found/ {
      print "lit-none"; next
    }
    /^STYLE FAIL: the width check walked [0-9]+ multi-line string literals/ {
      walked = $7; guard = "lit-walk"; next
    }
    /^STYLE FAIL: a line inside a string literal is over 80 characters$/ {
      print "lit-width"; next
    }
    /^ +but grep counts [0-9]+ / {
      if (guard != "") { print guard ":" walked "/" $4; guard = "" }
      next
    }
    /:[0-9]+: [0-9]+ characters$/ {
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
expect 0 '' 'exactly 80 characters in each of the three shapes is inside the width'

# ---------------------------------------------------------------------------
# one character past, in each shape: the finding is the line, and only it
# ---------------------------------------------------------------------------
new_case
plant_chunk 81
plant_literals 60 60
expect 1 "chunk-width $NVIM:2:81" 'a chunk body line one character over'

new_case
plant_chunk 60
plant_literals 81 60
expect 1 "$LIT:3:81 lit-width" 'a literal opening on its own line, one character over'

new_case
plant_chunk 60
plant_literals 60 81
expect 1 "$LIT:7:81 lit-width" 'a literal opened by an assignment, its body one character over'

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
expect 1 "chunk-width $NVIM:2:81" 'a chunk body line of wide characters one character over'

new_case
plant_chunk 60
plant_literals 81 60 pad_wide
expect 1 "$LIT:3:81 lit-width" 'a literal of wide characters one character over'

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
# below is planted in all eight spellings the tree uses -- the attach as a
# method, as an effect variant and as the wire method name, the resize the
# same three ways, the private attach both public ones funnel through, and
# the argv seed a spawn carries -- with a doc comment quoting the call and
# a string holding a // ahead of one.
#
# The rows mirror the checker own GEOMETRY_SITES.
# ---------------------------------------------------------------------------
plant_geometry() {
  mkdir -p "$(dirname "$CASE/$1")"
  : > "$CASE/$1"
  printf '/// A doc comment naming ui_attach and try_resize(, neither a call.\n' >> "$CASE/$1"
  i=0
  while [ "$i" -lt "$2" ]; do
    case $((i % 8)) in
      (0) printf 'ops.ui_attach(width, height, surfaces)?;\n' >> "$CASE/$1" ;;
      (1) printf 'Effect::Rpc(RpcCall::UiAttach { width, height })\n' >> "$CASE/$1" ;;
      (2) printf 'handle.request("nvim_ui_attach", args)?;\n' >> "$CASE/$1" ;;
      (3) printf 'ops.try_resize(width, height)?;\n' >> "$CASE/$1" ;;
      (4) printf 'Effect::Rpc(RpcCall::TryResize { width, height })\n' >> "$CASE/$1" ;;
      (5) printf 'handle.request("nvim_ui_try_resize", args)?;\n' >> "$CASE/$1" ;;
      (6) printf 'self.attach(width, height, surfaces, false)?;\n' >> "$CASE/$1" ;;
      (*) printf 'let cfg = cfg.with_late_attach(width, height);\n' >> "$CASE/$1" ;;
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
  plant_geometry 'crates/view-engine/src/nvim_api.rs' 10
  plant_geometry 'crates/view-engine/src/process.rs' 10
  # the render and the field read, the two spellings that carry the seed
  # without being a call named late_attach: a walk narrowed to
  # `late_attach(` or to `with_late_attach` stops counting them
  printf 'tokens.push(late_attach_cmd(width, height).into_bytes());\n' \
    >> "$CASE/crates/view-engine/src/process.rs"
  printf 'if let Some((width, height)) = cfg.late_attach {\n' \
    >> "$CASE/crates/view-engine/src/process.rs"
  plant_geometry 'crates/view-oracle/src/hang.rs' 4
  plant_geometry 'crates/view-oracle/src/lib.rs' 2
  plant_geometry 'crates/view-oracle/src/reference.rs' 2
  plant_geometry 'crates/view-oracle/src/speculate.rs' 2
  plant_geometry 'crates/view/src/engine_ops.rs' 14
  plant_release 'crates/view/src/main.rs' 1
  # the one file holding both spellings: the guard release and the argv
  # seed the spawn carries
  printf 'let cfg = cfg.with_late_attach(width, height);\n' \
    >> "$CASE/crates/view/src/main.rs"
  plant_geometry 'crates/view/src/native.rs' 1
  plant_geometry 'crates/view/src/recovery.rs' 1
  plant_geometry 'crates/view/src/runtime/executor.rs' 3
  plant_geometry 'crates/view/src/startup.rs' 7
}

# Graded on the rows that differ from the pinned listing, the way the
# tied-spawn cases above are: what a case proves is that the walk saw the
# planted change.
expect_geometry() {
  want_rc="$1"
  want="$2"
  desc="$3"
  git -C "$CASE" add -A
  out=$(bash "${RUN:-$CHECKER}" --geometry-sites "$CASE" 2>&1)
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
expect_geometry 0 '' 'the pinned population, in all eight spellings, with a doc comment quoting the call'

new_geometry_case
plant_geometry 'crates/view-scratch/src/lib.rs' 1
expect_geometry 1 'crates/view-scratch/src/lib.rs=1 geometry-sites' \
  'a production file outside the pinned set naming a geometry of its own'

new_geometry_case
printf 'let doc = "https://example.invalid/x"; ops.try_resize(width, height)?;\n' \
  >> "$CASE/crates/view/src/startup.rs"
expect_geometry 1 'crates/view/src/startup.rs=8 geometry-sites' \
  'a site behind a string holding a // on the same line'

new_geometry_case
plant_release 'crates/view-harness/src/fixture.rs' 1
expect_geometry 0 '' 'a lock release in a crate that owns no engine attach'

# ---------------------------------------------------------------------------
# the prose width gate: a page wraps at 80 characters, and what cannot wrap
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

# 81 characters, built rather than written out: a case that counts its own
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
    /^[^ ].*:[0-9]+: [0-9]+ characters$/ { c = $1; sub(/:$/, "", c); print c; next }
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
expect_width 1 'docs/page.md:4' 'a prose line one character over'

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
  'a prose line of 77 characters and 125 bytes, whose runs are too short to buy the exemption a byte measure would need'

new_width_case
pad_wide 81 'A rule of box characters ' ' ends the section' >> "$CASE/docs/page.md"
expect_width 1 'docs/page.md:4' 'a prose line of wide characters one character over'

new_width_case
mkdir "$CASE/docs/adir.md"
expect_width 1 'docs/adir.md:unreadable' \
  'a directory named like a page, which awk skips with a warning and a zero status'

new_width_case
ln -sn missing.md "$CASE/docs/dangling.md"
expect_width 1 'docs/dangling.md:unreadable' \
  'a page the walk cannot read, which a discarded status would pass as clean'

new_width_case
rm -f "$CASE/README.md" "$CASE/docs/page.md"
expect_width 1 'empty' 'a tree with no page for the walk to read'

# ---------------------------------------------------------------------------
# the comment rules over scripts/: a comment wraps at the same width a page
# does, cites no review finding and no planning document, and all three rules
# grade the one shebang-selected population scripts/lib/script-population.sh
# reads -- the remote-test fixtures carry no suffix, so a rule spelled over
# *.sh grades a subset of its sibling's. Counted in characters, as the page
# walk counts, so a rule drawn in box characters is judged by what an editor
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
    /^[^ ].*:[0-9]+: [0-9]+ characters$/ { c = $1; sub(/:$/, "", c); print c; next }
    /^[^ ].*: the shebang selection cannot read it$/ {
      c = $1; sub(/:$/, "", c); print c ":unreadable"; next
    }
    /^[^ ].*: not a regular file or a symlink, the shebang selection passes it over$/ {
      c = $1; sub(/:$/, "", c); print c ":skipped"; next
    }
    /^STYLE FAIL: no script found/ { print "empty"; next }
    /^STYLE FAIL: scripts\/ under .* holds no script but this one$/ {
      print "self-only"; next
    }
    /^STYLE FAIL: .* exited [0-9]+ instead of grading/ { print "refused"; next }
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
expect_script_comments 1 'scripts/gate.sh:4' 'a comment five characters over'

new_script_case
printf '# a path this long %s cannot be wrapped under the limit\n' \
  "$(pad 90 '' '' | tr -d ' ')" >> "$CASE/scripts/gate.sh"
expect_script_comments 0 '' 'a comment carrying a token longer than the limit itself'

new_script_case
pad 81 '# a comment ' ' past the limit' >> "$CASE/scripts/gate.sh"
expect_script_comments 1 'scripts/gate.sh:4' 'a comment one character over'

new_script_case
printf '# ── why the gated item is measured, not truncated at the marker ────────────\n' \
  >> "$CASE/scripts/gate.sh"
expect_script_comments 0 '' \
  'a rule drawn in box characters: 77 characters, and 105 bytes a byte measure reddens'

# The two limits of counting characters, one case each: the measure is the
# same on every awk and is not what a terminal paints, so the number a
# contributor is shown is stated in the unit it is actually in.
new_script_case
{
  printf '# '
  awk 'BEGIN { while (i++ < 29) printf "\344\270\255 "; printf "\344\270\255" }'
  printf '\n'
} >> "$CASE/scripts/gate.sh"
expect_script_comments 0 '' \
  'a double-width glyph counts one: 61 characters inside the limit, 91 columns painted'

new_script_case
{ printf '# '; awk 'BEGIN { while (i++ < 30) printf "e\314\201 " }'; printf '\n'; } \
  >> "$CASE/scripts/gate.sh"
expect_script_comments 1 'scripts/gate.sh:4' \
  'a combining mark counts its own character: 92 characters reddened, 62 columns painted'

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
ln -sn missing.sh "$CASE/scripts/gone.sh"
expect_script_comments 1 'scripts/gone.sh:unreadable' \
  'a script the rules cannot read, which a dropped selection would pass as clean'

new_script_case
rm -f "$CASE/scripts/gate.sh"
expect_script_comments 1 'empty' 'a tree with no script for the walk to read'

new_script_case
mkfifo "$CASE/scripts/pipe"
expect_script_comments 0 'scripts/pipe:skipped' \
  'a fifo under scripts/, which is readable and could never carry a shebang'

new_script_case
mkdir "$CASE/scripts/sub"
ln -sn sub "$CASE/scripts/dirlink"
expect_script_comments 1 'scripts/dirlink:unreadable' \
  'a symlink to a directory, which is a symlink that was meant to name a file'

new_script_case
{
  printf '#!/usr/bin/env bash\n'
  pad 97 '# a comment ' ' past the limit'
} > "$CASE/scripts/my leg.sh"
expect_script_comments 1 'refused' \
  'a script whose path carries a blank, which a split selection would drop while staying green'

new_script_case
rm -f "$CASE/scripts/gate.sh"
cp "$CHECKER" "$CASE/scripts/$(basename "$CHECKER")"
expect_script_comments 1 'self-only' \
  'a scripts/ holding only the checker, where both bans would report ok having graded nothing'

# The geometry walk's fail-closed trade, pinned so that "fixing" it into a
# comment-stripping read is a red case rather than a silent narrowing:
# `--prod-lines` emits the raw line, and eliding comments would take the two
# spellings that are string literals with them.
new_geometry_case
printf 'let rows = 1; // the row count ui_attach was given\n' \
  >> "$CASE/crates/view/src/native.rs"
expect_geometry 1 'crates/view/src/native.rs=2 geometry-sites' \
  'a spelling named only by a trailing comment, which the walk counts by design'

# ---------------------------------------------------------------------------
# the guarded commands that still carry `|| true`. A grep answers 1 for a
# tree with no hits and 2 for a pattern it could not compile, and the two
# walks below build their patterns from variables, so the status is not
# theirs to read: what stands in for reading it is that an errored filter
# leaves a pinned population unmet. Each case breaks one filter on a copy of
# the checker and requires the run to redden -- a copy, and beside a link to
# the scanner a checker resolves from its own directory, or the case fails
# for want of production lines instead of grading the filter.
# ---------------------------------------------------------------------------
# keyed on the case number, which the new_*_case helpers bump in this shell:
# a counter of its own would be incremented inside the command substitution
# that captures the path, so it would never leave the subshell and every
# copy would land in the same directory as the last one
# -n on every link here, and in scannerless_checker below: a plain `ln -s`
# whose destination is already a symlink to a directory follows it and
# writes the new link INSIDE the target -- for `$dir/lib` that target is the
# graded tree's own scripts/lib/, so a second copy under one case number
# would leave scripts/lib/lib behind and redden every gate that reads the
# population. With -n the second call refuses instead.
broken_checker() {
  dir="$WORK/broken$n"
  mkdir -p "$dir"
  ln -sn "$(cd "$(dirname "$CHECKER")" && pwd)/audit-god-files.sh" \
    "$dir/audit-god-files.sh"
  ln -sn "$(cd "$(dirname "$CHECKER")" && pwd)/lib" "$dir/lib"
  # substituted by index and read out of the environment: the text being
  # broken is itself a regex, and both a sed script and an awk -v value
  # would take a second pass at its backslashes
  old="$1" new="$2" awk '{
    i = index($0, ENVIRON["old"])
    if (i > 0) {
      $0 = substr($0, 1, i - 1) ENVIRON["new"] substr($0, i + length(ENVIRON["old"]))
    }
    print
  }' "$CHECKER" > "$dir/check-style.sh"
  printf '%s\n' "$dir/check-style.sh"
}

new_geometry_case
RUN=$(broken_checker 'grep -E "$GEOMETRY_CALLS"' 'grep -E "[unmatched"')
expect_geometry 1 'crates/view/src/main.rs=1 geometry-sites' \
  'the geometry call filter refusing its pattern, which leaves every call site unfound and main.rs short its argv seed'
RUN=""

new_geometry_case
RUN=$(broken_checker 'release\(' 'release[')
expect_geometry 1 'crates/view/src/main.rs=1 geometry-sites' \
  'the lock release filter refusing its pattern, which leaves one pinned file unfound and main.rs short its release'
RUN=""

# ---------------------------------------------------------------------------
# the condition-notice ownership pin, which only the whole run grades. Its
# stranger filter answered 1 both for a tree with no stranger and for a
# pattern it could not compile, so an errored filter read as a tree that
# owns its notice. Both cases run the whole checker on a scratch root
# carrying the owner and nothing else, so both exit 1 whatever the filter
# does -- what they grade is the condition verdict alone.
# ---------------------------------------------------------------------------
new_condition_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/crates/view-core/src/update"
  git -C "$CASE" init -q
  {
    printf 'fn a(m: &mut Messages) { m.set_native_condition(x); }\n'
    printf 'fn b(m: &mut Messages) { m.set_native_condition(y); }\n'
  } > "$CASE/crates/view-core/src/update/supervision.rs"
  git -C "$CASE" add -A
}

expect_condition() {
  want="$1"
  desc="$2"
  out=$(cd "$CASE" && bash "${RUN:-$CHECKER}" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | awk '
    /^STYLE FAIL: the condition-notice filter exited [0-9]+ instead of grading$/ {
      print "filter-refused"; next
    }
    /^STYLE FAIL: could not read production lines to check condition-notice/ {
      print "no-prod-lines"; next
    }
    /^STYLE FAIL: set_native_condition called outside/ { print "stranger"; next }
    /set_native_condition calls, pinned at/ { print "count"; next }
  ' | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//')
  if [ "$rc" = 1 ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'FAIL %s - %s\n  want rc=1 [%s]\n  got  rc=%s [%s]\n%s\n' \
    "$n" "$desc" "$want" "$rc" "$got" "$out"
}

new_condition_case
expect_condition '' 'the owner making both of its pinned calls, which the pin passes over'

new_condition_case
RUN=$(broken_checker 'grep -v "^$CONDITION_OWNER:"' 'grep -v "[unmatched"')
expect_condition 'filter-refused' \
  'the stranger filter refusing its pattern, which reports no stranger against a met count'
RUN=""

# the checker copied with no scanner beside it: `read_prod_lines` resolves
# the classifier from the checker's own directory, so this is what a
# renamed or missing one looks like to the ownership pin
# the same shape as broken_checker, minus the scanner symlink: what it makes
# is the checker with no classifier beside it
scannerless_checker() {
  dir="$WORK/broken$n"
  mkdir -p "$dir"
  ln -sn "$(cd "$(dirname "$CHECKER")" && pwd)/lib" "$dir/lib"
  cp "$CHECKER" "$dir/check-style.sh"
  printf '%s\n' "$dir/check-style.sh"
}

new_condition_case
printf 'fn c(m: &mut Messages) { m.set_native_condition(z); }\n' \
  > "$CASE/crates/view-core/src/update/mod.rs"
git -C "$CASE" add -A
expect_condition 'stranger' \
  'a second caller outside the owner, which flaps the one notice every pass'

new_condition_case
RUN=$(scannerless_checker)
expect_condition 'no-prod-lines' \
  'the classifier unreachable, which leaves the ownership pin ungraded rather than green'
RUN=""

# ---------------------------------------------------------------------------
# the temp-file trap walk: every script here that makes a temp file removes
# it under a trap, and the one that did not stranded its file for the whole
# length of the slowest scan in the gate.
# ---------------------------------------------------------------------------
new_temp_trap_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/scripts"
}

# the walk reads the shebang population, so a fixture states one: a file the
# selection passes over is a file the trap rule never reaches
write_temp_trap_script() {
  { printf '#!/usr/bin/env bash\n'; cat; } > "$CASE/scripts/a.sh"
}

expect_temp_traps() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "$CHECKER" --temp-traps "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" \
    | awk '/: makes a temp file with no EXIT trap naming it$/ { c = $1; sub(/:$/, "", c); print c }' \
    | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//')
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'FAIL %s - %s\n  want rc=%s [%s]\n  got  rc=%s [%s]\n%s\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got" "$out"
}

new_temp_trap_case
printf 'f=$(mktemp)\ntrap %s EXIT\n' "'rm -f \"\$f\"'" | write_temp_trap_script
expect_temp_traps 0 '' 'a script removing its temp file under a trap'

new_temp_trap_case
printf 'f=$(mktemp)\nrm -f "$f"\n' | write_temp_trap_script
expect_temp_traps 1 'scripts/a.sh' \
  'a script removing its temp file in a straight line, which a signal skips'

new_temp_trap_case
mkdir -p "$CASE/scripts/acceptance"
printf '#!/bin/sh\nf=$(mktemp)\nrm -f "$f"\n' > "$CASE/scripts/acceptance/leg"
expect_temp_traps 1 'scripts/acceptance/leg' \
  'a suffix-less leg under scripts/acceptance/, which a scripts/*.sh glob never reached'

new_temp_trap_case
printf 'f=$(mktemp)\nrm -f "$f"\ntrap - EXIT\n' | write_temp_trap_script
expect_temp_traps 1 'scripts/a.sh' \
  'a trap that clears the handler, which is the spelling that removes nothing'

new_temp_trap_case
printf 'f=$(mktemp)\nchild=$!\ntrap %s EXIT\nrm -f "$f"\n' "'kill \"\$child\"'" \
  | write_temp_trap_script
expect_temp_traps 1 'scripts/a.sh' \
  'a trap that reaps a child and removes no temp file'

# the shape half of this population is written in: the trap names a function
# and the removal is in its body, which is a pairing and has to stay green
new_temp_trap_case
printf 'cleanup() {\n  rm -f "$F"\n}\ntrap cleanup EXIT\nF=$(mktemp)\n' \
  | write_temp_trap_script
expect_temp_traps 0 '' 'the removal written in the function the trap names'

# The other shape half this population is written in: the path is appended
# to a list and the removal walks the list, so the name the handler reads is
# the array and its own loop variable. Green by the relationship that holds,
# which is the append -- a pairing that lowercased the two names instead
# passes the case below.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
ROOTS=()
cleanup() {
  for root in ${ROOTS[@]+"${ROOTS[@]}"}; do
    rm -rf "$root"
  done
}
trap cleanup EXIT
ROOT=$(mktemp -d)
ROOTS+=("$ROOT")
PLANT
expect_temp_traps 0 '' 'a removal written over the array the temp root is appended to'

# The same, written as a string list rather than an array, because the
# append is what the pairing reads and not the type of the thing appended to.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
list=""
cleanup() {
  for d in $list; do
    rm -rf "$d"
  done
}
trap cleanup EXIT
ROOT=$(mktemp -d)
list="$list $ROOT"
PLANT
expect_temp_traps 0 '' 'a removal written over the string list the temp root is appended to'

# The leak a case-insensitive pairing admits: two variables differing only
# by case, one a temp file and one a path that is meant to survive. The temp
# file is stranded and the walk has to say so.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
TMP=/var/cache/keepme
tmp=$(mktemp)
trap 'rm -rf "$TMP"' EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a trap naming an unrelated variable that differs from the temp one by case'

# A brace group inside the handler, which a body ending at the first indented
# brace reads as the end of the function: everything after it, the removal
# included, is dropped and a correct cleanup is reported.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
cleanup() {
  {
    echo done
  } >&2
  rm -f "$F"
}
trap cleanup EXIT
F=$(mktemp)
PLANT
expect_temp_traps 0 '' 'a removal written after a brace group in the handler body'

# The same handler with the removal taken out, so the case above is graded
# on the removal it holds rather than on the walk having stopped reading.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
cleanup() {
  {
    echo done
  } >&2
  echo bye
}
trap cleanup EXIT
F=$(mktemp)
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a handler with a brace group and no removal anywhere in it'

# ---------------------------------------------------------------------------
# the directories the whole run requires: a walk guarded on a directory that
# has moved grades nothing and says nothing, so the run reports on rules it
# never reached. Every literal directory a guard names is required by name.
# ---------------------------------------------------------------------------
new_required_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE"
}

expect_required() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(cd "$CASE" && bash "$CHECKER" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" \
    | sed -n 's/^STYLE FAIL: \(.*\) directory missing$/\1/p' \
    | LC_ALL=C sort | tr '\n' ' ' | sed 's/ *$//')
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ]; then
    printf 'ok %s - %s\n' "$n" "$desc"
    return
  fi
  failures=$((failures + 1))
  printf 'FAIL %s - %s\n  want rc=%s [%s]\n  got  rc=%s [%s]\n%s\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got" "$out"
}

new_required_case
mkdir -p "$CASE/crates" "$CASE/compat" "$CASE/corpus" "$CASE/docs"
: > "$CASE/README.md"
expect_required 1 'scripts/ scripts/acceptance/' \
  'a run from a root whose scripts/ has moved, which the walks alone pass silently'

new_required_case
mkdir -p "$CASE/scripts/acceptance" "$CASE/compat" "$CASE/corpus" "$CASE/docs"
: > "$CASE/README.md"
expect_required 1 'crates/' \
  'the same verdict for the sibling directory the run has always failed closed on'

# ---------------------------------------------------------------------------
# a mode handler reached by a relative path: the handlers cd into the root
# they grade, and a scanner resolved after that cd is resolved against the
# wrong directory.
# ---------------------------------------------------------------------------
relative_checker() {
  dir="$WORK/relative$n"
  mkdir -p "$dir"
  # the checker named the way a caller standing beside its tree names it,
  # from a directory that is not the root being graded
  {
    printf '#!/usr/bin/env bash\n'
    printf 'cd %s || exit 2\n' "$(dirname "$(dirname "$CHECKER")")"
    printf 'exec bash %s/%s "$@"\n' \
      "$(basename "$(dirname "$CHECKER")")" "$(basename "$CHECKER")"
  } > "$dir/run.sh"
  printf '%s\n' "$dir/run.sh"
}

new_geometry_case
RUN=$(relative_checker)
expect_geometry 0 '' 'the walk reached by a relative path, from a directory that is not the root'
RUN=""

# ---------------------------------------------------------------------------
# three pins over the tree this file ships in rather than over a scratch
# root: what each grades is a property of the committed sources themselves,
# which no fixture can stand in for.
# ---------------------------------------------------------------------------
TREE="$(cd "$(dirname "$0")/.." && pwd)"
# shellcheck source=lib/script-population.sh
. "$TREE/scripts/lib/script-population.sh"

new_pin_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE"
}

expect_pin() {
  if [ -z "$2" ]; then
    printf 'ok %s - %s\n' "$n" "$1"
    return
  fi
  failures=$((failures + 1))
  printf 'FAIL %s - %s\n' "$n" "$1"
  printf '%s\n' "$2" | sed 's/^/  | /'
}

# The unit word over the pages and the gate scripts. A line whose
# `columns` is what a glyph paints is the two-limit example these pages are
# written around, and stays. A line whose unit word carries the limit is a
# number wearing a unit it is not in -- the mismatch a contributor hits when
# a message reddening a comment at 80 characters cites a rule written in the
# cells a glyph paints. The discriminator is that painting verb on the same
# line, so every line kept says what it is about. The pattern here spells its
# first character bracketed: same language, and the pin grades this file by
# the rule it states.
#
# Scope is the four paths named below and nothing wider: the rules pages,
# plus the three files that state the limit. A discriminator this shape is a
# proximity heuristic rather than the property, and the rest of the shebang
# population carries 15 legitimate plural mentions of the word with no
# painting verb beside them -- a table's cells, a terminal's width -- so
# widening the walk would redden lines that are right.
new_pin_case
UNIT_WORD='\b[c]olumns\b'
unit=$(cd "$TREE" && grep -rnE "$UNIT_WORD" \
  .claude/rules scripts/check-style.sh scripts/check-style-cases.sh \
  scripts/lib/script-population.sh | grep -vE 'paint|fill') || true
expect_pin 'the plural unit word used only for what a glyph paints, never for the limit' "$unit"

# Each broken checker copy lands in a directory no earlier copy took. The
# counter that keyed the directory was bumped inside the command
# substitution that captures the path, so the increment never left the
# subshell: every copy landed on the last one, `ln` reported `Already
# exists` on stderr twice a run, and two copies held at once in one case
# would have been the same file read twice. Keyed on the case number
# instead, which the parent bumps.
new_pin_case
taken=$(ls -d "$WORK/broken$n" 2>/dev/null) || taken=""
copy=$(broken_checker 'release\(' 'release[' 2> "$WORK/copy.err")
isolation=""
if [ -n "$taken" ]; then
  isolation="$taken already held a copy before this case asked for one"
fi
if [ "$copy" != "$WORK/broken$n/check-style.sh" ]; then
  isolation=$(printf '%sthe copy landed at %s\n' "${isolation:+$isolation
}" "$copy")
fi
if [ -s "$WORK/copy.err" ]; then
  isolation=$(printf '%s%s\n' "${isolation:+$isolation
}" "$(cat "$WORK/copy.err")")
fi
expect_pin 'a broken checker copy taking a directory of its own, silently' "$isolation"

# A second copy asked for under one case number, which is what an added call
# with no new_*_case above it makes. A link made without -n whose
# destination is already a link to a directory follows it and lands inside
# the target -- here the graded tree's own scripts/lib/, whose next
# population read then cannot open scripts/lib/lib. That read is what this
# asserts, because all three gates over the population make it.
new_pin_case
GRADED="$(cd "$(dirname "$CHECKER")/.." && pwd)"
broken_checker 'release\(' 'release[' > /dev/null 2> "$WORK/reuse.err"
broken_checker 'release\(' 'release[' > /dev/null 2>> "$WORK/reuse.err"
reuse=""
if ! script_population_read "$GRADED" > /dev/null; then
  reuse="the population read the three gates share refuses the tree"
fi
# removed before the verdict is reported, so a red case leaves the tree it
# grades as it found it
if [ -e "$GRADED/scripts/lib/lib" ]; then
  reuse=$(printf '%sthe reused directory wrote a link into %s\n' \
    "${reuse:+$reuse
}" "$GRADED/scripts/lib")
  rm -f "$GRADED/scripts/lib/lib"
fi
expect_pin 'a checker directory reused, which writes no link into the graded tree' "$reuse"

# The same property over every link the population makes, because the two
# helpers above are not the last place one gets written and the failure is
# silent where it happens.
new_pin_case
if ! script_population_read "$TREE" > /dev/null || [ -z "$SCRIPT_POPULATION" ]; then
  deref="the population read answered nothing, so no link was graded"
else
  # anchored at the start of a command, so prose naming the flagless
  # spelling is not a finding
  deref=$(cd "$TREE" && grep -nE '^[[:space:]]*ln[[:space:]]+-s([^n]|$)' \
    $SCRIPT_POPULATION) || deref=""
fi
expect_pin 'every link the population makes written -n, so none writes through one' "$deref"

# A file with no suffix carrying a shebang, counted by every consumer of
# scripts/lib/script-population.sh. Two of them take a scan root and are run
# against the fixture; the third, check-budget-drift-cases.sh, grades the
# tree it ships in and takes none, so what stands for it is the helper
# answering the same file -- that call is the whole of its population.
new_pin_case
mkdir -p "$CASE/scripts" "$CASE/.claude/hooks"
: > "$CASE/Taskfile.yml"
# the banned spelling is assembled rather than written out, so planting a
# real one in the fixture does not put one in this file's own scanned lines
banned='stat'
{
  printf '#!/bin/sh\n'
  pad 85 '# a comment ' ' past the limit'
  printf '%s -c %%Y f\n' "$banned"
} > "$CASE/scripts/leg"
missed=""
note() { missed=$(printf '%s%s\n' "${missed:+$missed
}" "$1"); }
out=$(bash "$CHECKER" --script-comments "$CASE" 2>&1)
case "$out" in
  *"scripts/leg:2: "*) ;;
  *) note 'check-style.sh did not grade the comment in scripts/leg' ;;
esac
out=$(bash "$TREE/scripts/check-portability.sh" "$CASE" 2>&1)
case "$out" in
  *scripts/leg*) ;;
  *) note 'check-portability.sh did not scan scripts/leg' ;;
esac
script_population_read "$CASE" > /dev/null
if [ "$SCRIPT_POPULATION" != "scripts/leg" ]; then
  note "the helper selected [$SCRIPT_POPULATION] rather than scripts/leg"
fi
expect_pin 'a suffix-less shebang file reached by the comment rules, the userland scan and the helper the drift matrix reads' "$missed"

# Every path the run guards a walk on is a path the run requires by name,
# a page behind `[ -f ]` as much as a directory behind `[ -d ]`. Walking the
# guards rather than checking the two the last review found: the next walk
# added behind either is fail-open the moment it is written, and the guard
# is what a reader adds without thinking about the else.
new_pin_case
guarded=$( {
  grep -oE '\[ -[df] [A-Za-z0-9_/.-]+ \]' "$CHECKER" | awk '{ print $3 }'
  sed -n 's/^for dir in \(.*\); do$/\1/p' "$CHECKER" | tr ' ' '\n'
} | LC_ALL=C sort -u)
# `-f` beside `-d`, and both required lists read: the fail-open a guard with
# no else leaves is the same one whether the walk is guarded on a directory
# or on a page, and a pin that greps only `-d` can never see the second.
required=$( {
  sed -n 's/^for required in \(.*\); do$/\1/p' "$CHECKER"
  sed -n 's/^for required_file in \(.*\); do$/\1/p' "$CHECKER"
} | tr ' ' '\n' | LC_ALL=C sort -u)
# Defined out here rather than written inline below, and delimited by
# blanks rather than by newlines: the closing paren of a case pattern
# inside a command substitution is one of the two shapes bash 3.2
# miscounts, and there it ended the substitution rather than the pattern,
# so the whole file stopped parsing on the host the 3.2 contract is about.
# Blanks are safe here because a guarded directory is matched out of the
# checker as [A-Za-z0-9_/.-]+ and can hold none.
required_holds() { case " $2 " in (*" $1 "*) return 0 ;; esac; return 1; }
required_line=$(printf '%s\n' "$required" | tr '\n' ' ')
unrequired=$(printf '%s\n' "$guarded" \
  | while IFS= read -r d; do
    [ -n "$d" ] || continue
    required_holds "$d" "$required_line" ||
      printf '%s is guarded on but not required\n' "$d"
  done)
if [ -z "$required" ]; then
  unrequired=$(printf '%s\nthe run requires no directory at all\n' "$unrequired")
fi
expect_pin 'every path a walk is guarded on named in one of the run required lists' "$unrequired"

# The wrapped-opening carve-out, derived by the walk and printed rather than
# written into the header by hand. A re-wrapped signature is what used to
# leave that sentence stale in silence; here it has to move the printed
# count and redden the row for the file it was re-wrapped in.
new_geometry_case
wrapped_of() {
  bash "$CHECKER" --geometry-sites "$CASE" 2>&1 \
    | sed -n 's/^geometry: [0-9]* counted lines, \([0-9]*\) of them wrapped openings in \([0-9]*\) files$/\1 \2/p'
}
git -C "$CASE" add -A
before=$(wrapped_of)
printf 'fn ui_attach(\n' >> "$CASE/crates/view/src/native.rs"
git -C "$CASE" add -A
after=$(wrapped_of)
out=$(bash "$CHECKER" --geometry-sites "$CASE" 2>&1) && rc=0 || rc=$?
rewrap=""
if [ "$before" != "0 0" ]; then
  rewrap="the unwrapped tree printed [$before] rather than no wrapped opening"
fi
if [ "$after" != "1 1" ]; then
  rewrap=$(printf '%sthe re-wrapped tree printed [%s] rather than one in one file\n' \
    "${rewrap:+$rewrap
}" "$after")
fi
row=$(printf '%s\n' "$out" \
  | awk '$1 == "crates/view/src/native.rs" && $2 == "2" { f = 1 } END { if (f) print "found" }')
if [ "$rc" != 1 ] || [ "$row" != found ]; then
  rewrap=$(printf '%sthe row for the re-wrapped file did not redden (rc %s)\n' \
    "${rewrap:+$rewrap
}" "$rc")
fi
expect_pin 'a re-wrapped signature moving the printed wrapped-opening count and reddening its row' "$rewrap"

printf '\n%s cases, %s failures\n' "$n" "$failures"
[ "$failures" -eq 0 ]
