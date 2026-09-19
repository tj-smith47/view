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

# A helper called above its own definition is "command not found" at rc 127,
# which set -u never sees and the tally never counts, so the case it stands
# in is skipped without a word: a spelling case shipped that way once. The
# order is graded before any case runs, at command position only, since a
# helper's name inside a string or an awk program is not a call.
misordered=$(awk '
  FNR == NR {
    if ($0 ~ /^[a-z_]+\(\) \{/) {
      name = $1
      sub(/\(\).*/, "", name)
      if (!(name in def)) def[name] = FNR
    }
    next
  }
  {
    line = $0
    sub(/^[[:space:]]*/, "", line)
    for (name in def) {
      if (FNR < def[name] && line ~ ("^" name "([[:space:]]|$)")) {
        printf "%s:%d: %s called before its definition at line %d\n", FILENAME, FNR, name, def[name]
      }
    }
  }
' "$0" "$0")
if [ -n "$misordered" ]; then
  printf '%s\n' "$misordered" >&2
  exit 2
fi

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

# shellcheck source=lib/scratch.sh
. "$(dirname "$0")/lib/scratch.sh"
WORK=$(mktemp -d "$(scratch_root)/check-style-cases-XXXXXX")
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
  printf 'FAIL %s - %s\n  want rc=%s [%s]\n  got  rc=%s [%s]\n%s\n%s\n' \
    "$n" "$desc" "$want_rc" "$want" "$rc" "$got" "${note:-}" "$out"
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
# The population is read off the checker rather than restated here: an empty
# root makes it print its own pinned listing beside the finding, which is
# the same listing expect_tied already parses below. Two copies of twenty-two
# path-and-count pairs is a second place to forget, and forgetting it reddens
# five cases at once with a message about the tree.
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

# The checker's own pinned listing, as path-and-count pairs. Harvested once:
# a case tree is built per case, and the checker is a whole tree walk. The
# seed file is what the listing costs: the walk refuses a root with no
# tracked crate source rather than printing a listing about nothing.
mkdir -p "$WORK/seed/crates/seed/src"
git -C "$WORK/seed" init -q
printf 'pub fn nothing() {}\n' > "$WORK/seed/crates/seed/src/lib.rs"
git -C "$WORK/seed" add -A
TIED_POPULATION=$(bash "$CHECKER" --tied-spawns "$WORK/seed" 2>&1 | awk '
  /^pinned:$/ { on = 1; next }
  /^found:$/ { exit }
  on && NF >= 2 { print $1, $2 }
')
if [ -z "$TIED_POPULATION" ]; then
  printf 'TIED-SELF-FAIL: the checker printed no pinned listing to plant from\n'
  exit 2
fi

new_tied_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/crates"
  # the walk asks the god-file scanner which lines are production, and that
  # scanner reads tracked files: a scratch root has to be a repository for
  # the classifier to see anything at all
  git -C "$CASE" init -q
  printf '%s\n' "$TIED_POPULATION" | while read -r tied_path tied_count; do
    plant_spawns "$tied_path" "$tied_count"
  done
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
  plant_geometry 'crates/view/src/vlog.rs' 2
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
# the doc-figure walk: a measurement written into a doc comment is graded by
# nothing and re-recorded by nobody, so a `///` or `//!` line stating one
# fails unless the line names a cell the drift check knows. Three things
# decide whether a figure is a reading, and each is planted below: a decimal
# carrying a unit is one wherever it stands, an integer is one only inside
# the sentence a reading word opened -- any of the verbs a reading is stated
# in, not the word "measured" alone -- and neither is graded where the line
# names a bound or one of the other shapes a chosen value stands beside,
# names a cell id, or sits inside a fenced sample.
#
# The sentence scope is the case that pins the shape. A per-line reading
# state let a figure rustfmt wrapped onto the next line through, and a
# block-wide one graded every constant a block explains beside a measurement
# as a reading -- so the wrapped figure and the constant standing in a block
# whose other sentence measured something are a red case and a green one.
#
# The vocabulary is the drift check's own, read through
# `check-budget-drift.sh --cell-ids`, so each case tree carries the two files
# that check requires. A tree whose budgets file declares no cell is its own
# red case: a walk that read no ids grades every figure as anchored, which
# reads exactly like a tree with nothing to report.
# ---------------------------------------------------------------------------
plant_cell_vocabulary() {
  mkdir -p "$CASE/crates/view-bench" "$CASE/.claude/specs"
  : > "$CASE/.claude/specs/2026-07-17-view-design.md"
  {
    if [ "${1:-with}" = with ]; then
      printf '%s\n' '[[budget]]'
      printf '%s\n' 'scenario = "echo"'
      printf '%s\n' 'metric = "ratio_p50"'
    fi
  } > "$CASE/crates/view-bench/budgets.toml"
}

# Every shape the walk lets through, in one file: a mechanism stated in
# words, a constant the code passes, a figure anchored on a cell id, a figure
# behind the word that makes it a choice, and a fenced sample of what
# something prints.
plant_doc_figures() {
  mkdir -p "$CASE/crates/view-x/src"
  {
    printf '%s\n' '/// Clipping stops paying once most of the frame repaints.'
    printf '%s\n' '///'
    printf '%s\n' '/// The cadence the loop asks for is 150 ms.'
    printf '%s\n' '///'
    printf '%s\n' '/// The gate holds echo.ratio_p50 to 1.25x.'
    printf '%s\n' '///'
    printf '%s\n' '/// A 0.37 us bar is what the row is held to.'
    printf '%s\n' '///'
    printf '%s\n' '/// ```text'
    printf '%s\n' '/// echo/minimal: view p50 0.612ms | nvim p50 0.550ms'
    printf '%s\n' '/// ```'
    printf '%s\n' 'fn clipping_pays() {}'
  } > "$CASE/crates/view-x/src/lib.rs"
}

new_doc_figures_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE"
  plant_cell_vocabulary with
  plant_doc_figures
}

# Graded on the file and line of every reported figure, not on the figure
# itself: what a case proves is that the walk saw the planted line, and a
# case expecting one finding has to fail when a second is reported.
expect_doc_figures() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "${RUN:-$CHECKER}" --doc-figures "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | awk '
    /^STYLE FAIL: a doc comment states a measurement nothing re-takes$/ {
      print "doc-figures"; next
    }
    /^STYLE FAIL: the doc-figure walk read no cell ids to grade against$/ {
      print "no-vocabulary"; next
    }
    /^crates\// { sub(/:$/, "", $1); print $1 }
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

new_doc_figures_case
expect_doc_figures 0 '' 'a tree whose doc comments state mechanisms, constants, anchors, bounds and samples'

new_doc_figures_case
printf '%s\n' '/// A repainted row costs 0.37 us of the frame.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a decimal carrying a unit, with no reading word anywhere near it'

new_doc_figures_case
printf '%s\n' '/// Measured on this host at 6 ns per cell.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'an integer inside the sentence a reading word opened'

new_doc_figures_case
printf '%s\n' '/// Measured on this host and on the one beside it,' \
  '/// at 6 ns per cell.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:14 doc-figures' \
  'the same integer wrapped onto the line after the word that introduced it'

new_doc_figures_case
printf '%s\n' '/// Measured once on this host and never since.' \
  '/// The loop asks for 20 ms between passes.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 0 '' \
  'a constant in the sentence after the one a reading word opened and closed'

new_doc_figures_case
printf '%s\n' '/// Measured at 0.37 us on echo.ratio_p50.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 0 '' 'a reading on a line naming a cell the drift check knows'

new_doc_figures_case
plant_cell_vocabulary without
expect_doc_figures 1 'no-vocabulary' \
  'a tree whose budgets file declares no cell, which would grade every figure as anchored'

# The four spellings a bare `[0-9]` token missed, each of which shipped a
# reading: an instrument writes a delta with its sign and a spread as a
# range, and a token that reads neither passed ten measured figures. The
# range separators become blanks, except the hyphen, which becomes the sign
# of the figure after it so a written `-0.5ms` reads the same way.
new_doc_figures_case
printf '%s\n' '/// The paired delta came out at +1.23% over the window.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a signed decimal carrying a unit'

new_doc_figures_case
printf '%s\n' '/// The tail tracked host load over 0.62ms..92.5ms.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a range written with two dots, whose ends are decimals carrying a unit'

new_doc_figures_case
printf '%s\n' '/// Measured across days, the reading swung +/-20% either way.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a band written around a whole-unit figure, inside a reading sentence'

new_doc_figures_case
printf '%s\n' '/// Measured spread 8-10ms over eleven trials.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a range written with a hyphen, whose ends are whole-unit figures'

# An escape exempts the figures on its own line and never the sentence the
# words on it opened. Skipping the line outright left a reading word beside
# a bound to open no sentence, so the figure rustfmt wrapped below it went
# ungraded -- and the same hole sat behind the cell-id escape.
new_doc_figures_case
printf '%s\n' '/// Not a 5ms bar. Measured pre-attach windows span roughly' \
  '/// 50ms on one platform and more on the other.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:14 doc-figures' \
  'a reading word beside a bound, whose figure is wrapped onto the line below'

new_doc_figures_case
printf '%s\n' '/// Measured on echo.ratio_p50 and on the row beside it,' \
  '/// at 6 ns per cell.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:14 doc-figures' \
  'a reading word on a line naming a cell, whose figure is wrapped onto the line below'

# The same wrap the other way round: rustfmt puts the break wherever the
# width runs out, so the word that makes a figure a reading is as free to
# land after it as before it, and a state that only ran forward graded the
# figure as a constant. What decides it either way is the sentence, so a `.`
# between the two is the green case.
new_doc_figures_case
printf '%s\n' '/// A pre-attach window of 50 ms on one platform and more on' \
  '/// the other, measured across every boot of the day.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a figure on the line before the reading word that grades it, in the same sentence'

new_doc_figures_case
printf '%s\n' '/// The loop asks for 20 ms between passes. Nothing about that' \
  '/// number was measured.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 0 '' \
  'a constant in the sentence before the one a reading word opens'

new_doc_figures_case
printf '%s\n' '/// Measured over 1..=5 ms, depending on the host.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'an inclusive range, whose ends are whole-unit figures inside a reading sentence'

# The finding names the token as the file spells it. A range is cut into two
# figures so each is graded, and a reader sent to `-10` for source text
# `8-10ms` is left to work out which half the file wrote.
new_doc_figures_case
printf '%s\n' '/// Measured spread 8-10ms over eleven trials.' \
  >> "$CASE/crates/view-x/src/lib.rs"
spelled=$(bash "${RUN:-$CHECKER}" --doc-figures "$CASE" 2>&1 |
  sed -n 's|^crates/view-x/src/lib\.rs:13: ||p')
desc='the finding prints the figure as the file spells it'
if [ "$spelled" = 8-10ms ]; then
  printf 'ok %s - %s\n' "$n" "$desc"
else
  failures=$((failures + 1))
  printf 'not ok %s - %s\n  want [8-10ms]\n  got  [%s]\n' "$n" "$desc" "$spelled"
fi

# A whole-unit reading is as real as a fractional one, and `98 ms` shipped in
# a startup chunk's doc because the sentence around it said `spends` rather
# than `measured`. The vocabulary is every verb a reading is stated in, and
# the escape it has to stay clear of is the shape words a value the code
# chose stands beside.
new_doc_figures_case
printf '%s\n' '/// The parse spends 98 ms of it before the text goes out.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what something spends'

new_doc_figures_case
printf '%s\n' '/// A 20,000-entry tree walked in 83 ms on a loaded host.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying how long a walk took'

# Each of these carries a reading verb, so the escape is what passes it. The
# two that shipped first carried none, and passed under the checker that had
# no escape at all -- a green case that would be green either way grades
# nothing.
new_doc_figures_case
printf '%s\n' '/// The scan spends 150 ms, which is the throttle it was given.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 0 '' \
  'a whole-unit reading on a line naming the throttle that chose it'

new_doc_figures_case
printf '%s\n' '/// The retry spends 300 ms, which is the tier it was given.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 0 '' \
  'a whole-unit reading on a line naming the tier that chose it'

# `(s|d)?` gave cap, caps, capd, and the comment claimed `capped` -- the one
# spelling whose stem changes. `tiered` and `bounded` are the same shape.
new_doc_figures_case
printf '%s\n' '/// The scan spends 150 ms, capped there by the loop that arms it.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 0 '' \
  'a whole-unit reading on a line naming the cap that chose it, spelled `capped`'

# The verbs a reading is stated in that the first vocabulary missed. Each of
# these shipped on the tree behind a closure keyed on the verbs already in it.
new_doc_figures_case
printf '%s\n' '/// A cold heavy start pays 13 s to take the notices down.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what a start pays'

new_doc_figures_case
printf '%s\n' '/// The write lands after the keypress, 250 us later.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying where a write lands'

new_doc_figures_case
printf '%s\n' '/// The fold ran 83 ms behind the tick that moved it.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying how long something ran'

# One red case per remaining vocabulary member: a case reverting the member
# alone from the shipped checker has to redden here, or the member is graded
# by nothing and the rule that mints these cases is itself unmet.
new_doc_figures_case
printf '%s\n' '/// The sweep runs 83 ms behind the tick that moved it.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying how long a sweep runs'

new_doc_figures_case
printf '%s\n' '/// The wait needed 83 ms before the retry fired.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what a wait needed'

new_doc_figures_case
printf '%s\n' '/// The retry needs 83 ms before it fires again.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what a retry needs'

new_doc_figures_case
printf '%s\n' '/// Taking 83 ms to settle, the frame then repaints.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what settling is taking'

new_doc_figures_case
printf '%s\n' '/// The tree walks 83 ms of entries before it stops.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what a walk walks'

new_doc_figures_case
printf '%s\n' '/// The cold start paid 83 ms to take the notices down.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what a start paid'

new_doc_figures_case
printf '%s\n' '/// The write landed 250 us after the keypress fired.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying where a write landed'

new_doc_figures_case
printf '%s\n' '/// The probe observed 83 ms before the retry began.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what a probe observed'

new_doc_figures_case
printf '%s\n' '/// The trace recorded 83 ms before the frame settled.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what a trace recorded'

new_doc_figures_case
printf '%s\n' '/// The parse cost 83 ms before the frame settled.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what a parse cost'

new_doc_figures_case
printf '%s\n' '/// The retry took 83 ms before it settled again.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what a retry took'

new_doc_figures_case
printf '%s\n' '/// The sweep clocked 83 ms before the frame settled.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what a sweep clocked'

new_doc_figures_case
printf '%s\n' '/// The parse timed 83 ms before the frame settled.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying what a parse timed'

new_doc_figures_case
printf '%s\n' '/// The frame appears 83 ms after the keypress fires.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence saying when a frame appears'

new_doc_figures_case
printf '%s\n' '/// The reading is 83 ms behind the tick that moved it.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 1 'crates/view-x/src/lib.rs:13 doc-figures' \
  'a whole-unit reading in a sentence naming the noun "reading" itself'

# `ran` as a stem is inside `range`, `transient` and `guarantee`, and every
# one of those would open a sentence the walk then grades. The vocabulary is
# read off a copy whose words each carry a space, so the stem matches the
# word and not the words it sits inside.
new_doc_figures_case
printf '%s\n' '/// The range the loop asks for is 20 ms wide.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 0 '' \
  'a constant in a sentence whose only reading verb is a word that contains one'

# A figure the code computes is neither a reading nor a value written down:
# the only way to keep `2 x` and the `31s` five doubled waits come to was to
# drop the verb that made either a sentence.
new_doc_figures_case
printf '%s\n' '/// Five doubled waits spend the 31 s derived from that base.' \
  >> "$CASE/crates/view-x/src/lib.rs"
expect_doc_figures 0 '' \
  'a whole-unit figure on a line saying the code derives it'

# ---------------------------------------------------------------------------
# the doc-comment width gate: a `///`/`//!` line is held to its crate's
# rustfmt `max_width` (100 where no `rustfmt.toml`/`.rustfmt.toml` exists),
# code lines excluded because rustfmt already wraps those. A table row and a
# fenced sample cannot rewrap without changing what they say, and a run that
# could not fit even alone on its own line -- the marker and indentation
# already spent -- cannot be helped by moving words around it, so each is
# exempt by shape.
# ---------------------------------------------------------------------------
new_doc_width_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/crates/view-x/src"
  : > "$CASE/crates/view-x/src/lib.rs"
}

# Graded on the file and line of every reported line, the same shape
# `expect_doc_figures` uses: a case expecting one finding fails if a second
# is reported, and a case expecting silence fails if the walk narrows itself
# and reports nothing for the wrong reason.
expect_doc_width() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "${RUN:-$CHECKER}" --doc-width "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | awk '
    /^STYLE FAIL: a doc comment line runs past its crate.s rustfmt max_width$/ {
      print "doc-width"; next
    }
    /^STYLE FAIL: no crate sources found/ { print "no-crates"; next }
    /^crates\// { sub(/:$/, "", $1); print $1 }
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

new_doc_width_case
printf '%s\n' '/// A line of ordinary prose that stays well inside the limit.' \
  > "$CASE/crates/view-x/src/lib.rs"
expect_doc_width 0 '' 'a doc comment inside the limit'

new_doc_width_case
printf '%s\n' \
  '/// A line of ordinary prose padded out with filler words until it runs well past the hundred column limit rustfmt holds every other line in this crate to.' \
  > "$CASE/crates/view-x/src/lib.rs"
expect_doc_width 1 'crates/view-x/src/lib.rs:1 doc-width' \
  'a doc comment line over the default 100-column limit'

# A markdown table row is one row of the table it stands in and cannot
# rewrap without breaking the table, so it is exempt regardless of width.
new_doc_width_case
printf '%s\n' \
  '/// | a very long left column that pushes this row well past the hundred column limit | yes |' \
  > "$CASE/crates/view-x/src/lib.rs"
expect_doc_width 0 '' 'a table row over the limit'

# A line starting with a literal `|` that is ordinary prose, not a table row,
# carries no second `|` and no separator-row shape, so it is not exempt.
new_doc_width_case
printf '%s\n' \
  '/// | a filter piped through several stages that runs well past the hundred column limit rustfmt holds every other line in this crate to.' \
  > "$CASE/crates/view-x/src/lib.rs"
expect_doc_width 1 'crates/view-x/src/lib.rs:1 doc-width' \
  'a `|`-led prose line over the limit that is not a table row'

# The separator row shape (`|-`) carries no second `|` at all and is still a
# genuine table row.
new_doc_width_case
awk 'BEGIN { s = "|"; while (length(s) < 101) { s = s "-" }; print "/// " s }' \
  > "$CASE/crates/view-x/src/lib.rs"
expect_doc_width 0 '' 'a table separator row over the limit'

# A fenced sample is a quote of what something prints or parses, not prose:
# splitting the line would document a line break the program never emits.
new_doc_width_case
printf '%s\n' '/// ```text' \
  '/// echo/minimal: view p50 0.612ms p99 0.941ms max 1.203ms | nvim p50 0.550ms p99 0.713ms' \
  '/// ```' \
  > "$CASE/crates/view-x/src/lib.rs"
expect_doc_width 0 '' 'a fenced sample over the limit'

# A markdown link whose target alone, plus the marker and indentation this
# line already pays for, cannot fit under the limit has nowhere to break:
# moving it to a line of its own still leaves it over.
new_doc_width_case
printf '%s\n' \
  '    /// ([`SurfaceConflicts::forget_engine`](crate::native::surfaces::SurfaceConflicts::forget_engine)' \
  '    /// carries the per-field reasoning).' \
  > "$CASE/crates/view-x/src/lib.rs"
expect_doc_width 0 '' \
  'a markdown link that cannot fit on a line of its own even with its indentation alone'

# A crate-level rustfmt.toml overrides the workspace root's own max_width
# (there is none at this scratch tree's root, so the default of 100 is what
# it overrides): a line that fits under 100 but not under the crate's own
# 60 has to redden, or the crate-level read is dead code.
new_doc_width_case
printf '%s\n' 'max_width = 60' > "$CASE/crates/view-x/rustfmt.toml"
printf '%s\n' \
  "/// A line under the default hundred characters but over this crate's own." \
  > "$CASE/crates/view-x/src/lib.rs"
expect_doc_width 1 'crates/view-x/src/lib.rs:1 doc-width' \
  "a doc comment under the default 100 but over the crate's own rustfmt max_width"

new_doc_width_case
rm -rf "$CASE/crates"
expect_doc_width 1 'no-crates' \
  'a tree with no crate sources, which would grade every line as inside the limit'

new_width_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/docs"
  # each seed line is inside the width and over the short floor, so that the
  # line a case appends below it grades what the case is about rather than the
  # seam between the two
  printf '# view\n\nA line of prose that sits inside the limit and past the short floor.\n' \
    > "$CASE/README.md"
  printf '# page\n\nAnother line of prose inside the limit and past the short floor too.\n' \
    > "$CASE/docs/page.md"
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
    /^[^ ].*:[0-9]+: [0-9]+ characters, and the paragraph runs on$/ {
      c = $1; sub(/:$/, "", c); print c ":ragged"; next
    }
    /^[^ ].*:[0-9]+: [0-9]+ characters$/ { c = $1; sub(/:$/, "", c); print c; next }
    /^[^ ].*:[0-9]+: an inline code span runs on to line [0-9]+$/ {
      c = $1; sub(/:$/, "", c); print c ":span"; next
    }
    /^[^ ].*:[0-9]+: a list marker sits mid-line in prose$/ {
      c = $1; sub(/:$/, "", c); print c ":marker"; next
    }
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

# The convention pages wrap at the same width as the docs, and nothing
# measured them until this walk read the directory they sit in: both lines
# the last review found there were written by appending a sentence to a
# paragraph rather than re-wrapping it, which is the shape no reader catches.
new_width_case
mkdir -p "$CASE/.claude/rules"
{ printf '# shell\n\n'; pad 100 'A rule written as prose ' ' and never re-wrapped'; } \
  > "$CASE/.claude/rules/shell.md"
expect_width 1 '.claude/rules/shell.md:3' 'a prose line over the width on a convention page'

new_width_case
mkdir -p "$CASE/.claude/rules"
printf '# shell\n\nA line inside the limit.\n' > "$CASE/.claude/rules/shell.md"
expect_width 0 '' 'a convention page inside the width'

# The other half of that page edit: the sentence that went over the width was
# re-wrapped and the line before it was left short, which the width walk reads
# as clean because it grades the maximum alone.
new_width_case
{
  printf '# page\n\n'
  printf 'This sentence fills most of a line and then the paragraph goes on.\n'
  printf 'A short seam.\n'
  printf 'The paragraph runs on past the seam and would have fitted beside it.\n'
} > "$CASE/docs/page.md"
expect_width 1 'docs/page.md:4:ragged' \
  'a short line mid-paragraph, which a walk grading the maximum reads as clean'

# The three shapes a line is short for a reason, each of which a rule reading
# "under the floor" alone would redden: the line that ends its paragraph, the
# line whose next word could not have fitted beside it, and the line a list
# item or a table row follows.
new_width_case
{
  printf '# page\n\n'
  printf 'A short seam.\n\n'
  printf 'A new paragraph starts under it.\n'
} > "$CASE/docs/page.md"
expect_width 0 '' 'a short line ending its paragraph'

new_width_case
{
  printf '# page\n\n'
  printf 'A short seam.\n'
  printf '%s beside it.\n' "$(over 90 | tr -d ' ')"
} > "$CASE/docs/page.md"
expect_width 0 '' 'a short line whose next word is longer than what is left of the limit'

new_width_case
{
  printf '# page\n\n'
  printf 'A short seam:\n'
  printf -- '- an item under it\n'
  printf 'Another short seam:\n'
  printf '| a | row |\n'
} > "$CASE/docs/page.md"
expect_width 0 '' 'a short line a list item and a table row follow'

# The fourth shape: a marker comment on its own line. The generated block in
# docs/surface-ownership.md sits under one, and a rule reading the marker as a
# short prose line asks for the paragraph to be pulled up into the comment --
# where the test that writes that block would put it back at the next run.
new_width_case
{
  printf '# page\n\n'
  printf '<!-- generated from the policy table -->\n'
  printf 'The generated paragraph runs on under the marker and reads as prose.\n'
} > "$CASE/docs/page.md"
expect_width 0 '' 'a short marker comment the generated paragraph follows'

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
over 81 | sed 's/^/| /; s/$/ |/' >> "$CASE/docs/page.md"
expect_width 0 '' 'a table row over the width'

# A line starting with a literal `|` that is ordinary prose, not a table row,
# carries no second `|` and no separator-row shape, so it is not exempt.
new_width_case
{ printf '| '; over 81; } >> "$CASE/docs/page.md"
expect_width 1 'docs/page.md:4' \
  'a `|`-led prose line over the width that is not a table row'

# The separator row shape (`|-`) carries no second `|` at all and is still a
# genuine table row.
new_width_case
awk 'BEGIN { s = "|-"; while (length(s) < 82) { s = s "-" }; print s }' \
  >> "$CASE/docs/page.md"
expect_width 0 '' 'a table separator row over the width'

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

# A wrap that lands inside an inline code span leaves the page reading the
# same and the span gone: `crates/view-harness/src/bin/bench.rs` looks its
# fixtures up in docs/benchmarking.md by the span they are written as, and a
# split one is a page whose next edit reddens a test rather than the page.
new_width_case
{
  printf '# page\n\n'
  printf 'A sentence before the call `nvim_buf_set_lines(0, 0, -1, false,\n'
  printf '{})` and then more prose, which carries the paragraph on.\n'
} > "$CASE/docs/page.md"
expect_width 1 'docs/page.md:3:span' 'an inline code span a wrap broke across two lines'

new_width_case
{
  printf '# page\n\n'
  printf 'A sentence before the call `nvim_buf_set_lines(0, 0)` and then\n'
  printf 'more prose after it, which carries the paragraph on past here.\n'
} > "$CASE/docs/page.md"
expect_width 0 '' 'the same span whole on its line'

# A span longer than the limit has nowhere to go, exactly like the run the
# width walk subtracts: reddening it asks for a line over the limit.
new_width_case
{
  printf '# page\n\n'
  printf 'A sentence before `%s\n' "$(over 60 | tr -d ' ')"
  printf '%s` and then more prose to carry it on.\n' "$(over 30 | tr -d ' ')"
} > "$CASE/docs/page.md"
expect_width 0 '' 'a code span longer than the limit itself, which no wrap can hold on one line'

# The other side of the same rule: the word a short line is graded against is
# the whole span and the punctuation behind it, because that is what a re-wrap
# would have to move up. Measured to the first blank instead, the span's own
# first word fits and the seam reddens -- with nothing to do about it but
# break the span the case above forbids breaking.
new_width_case
{
  printf '# page\n\n'
  pad 58 'A sentence that stops ' ' short here.'
  printf '%s and the paragraph runs on past the seam.\n' '`rm -rf /tmp/x and more`,'
} > "$CASE/docs/page.md"
expect_width 0 '' 'a short seam whose next line opens with a span too wide to move up beside it'

# The span state belongs to the paragraph: a code span cannot cross a blank
# line and cannot cross a fence. Carried past either, the stray backtick in
# the first paragraph pairs with the opening tick of the real span below it,
# the run that accumulates takes the over-long exemption, and the genuine
# split span is never reported -- silently, which is the direction that
# matters.
new_width_case
{
  printf '# page\n\n'
  printf 'A paragraph that uses a ` as punctuation and then carries on past\n'
  printf 'the seam so that the line above it is not short either.\n\n'
  printf 'A second paragraph of prose that runs on and stops here at a seam.\n\n'
  printf 'A sentence before the call `nvim_buf_set_lines(0, 0, -1, false,\n'
  printf '{})` and then more prose, which carries the paragraph on.\n'
} > "$CASE/docs/page.md"
expect_width 1 'docs/page.md:8:span' \
  'a split span two paragraphs under a stray backtick, which a carried state hides'

new_width_case
{
  printf '# page\n\n'
  printf 'A paragraph that uses a ` as punctuation and then carries on past\n'
  printf 'the seam so that the line above it is not short either.\n\n'
  printf '```\n'
  printf 'a sample line holding one ` of its own\n'
  printf '```\n\n'
  printf 'A sentence before the call `nvim_buf_set_lines(0, 0, -1, false,\n'
  printf '{})` and then more prose, which carries the paragraph on.\n'
} > "$CASE/docs/page.md"
expect_width 1 'docs/page.md:10:span' \
  'the same split span under a fence, whose sample ticks are not the page prose'

# A bullet the wrap pulled up onto the line above it stops being an item of
# its list, and nothing else in this walk sees it: a list opener is exempt
# from the ragged rule, the merged line is inside the width, and a word-stream
# comparison reads the `-` either way. Three of these were spent to rejoin
# split code spans on pages this walk grades.
new_width_case
{
  printf '# page\n\n'
  printf -- '- A first item whose sentence ends here. - A second item that was\n'
  printf '  pulled up onto the line above it by a re-wrap.\n'
} > "$CASE/docs/page.md"
expect_width 1 'docs/page.md:3:marker' \
  'a bullet a re-wrap pulled into the item above it'

new_width_case
{
  printf '# page\n\n'
  printf -- '- A first item whose sentence ends here.\n'
  printf -- '- A second item that keeps the line it opens, as an item of the list.\n'
} > "$CASE/docs/page.md"
expect_width 0 '' 'the same two items, each opening its own line'

# The line an item is merged into is as often a continuation of the item above
# as it is that item's opener, and all three bullets the sweep spent sat on a
# continuation: graded by the opener arm alone, the rule finds none of them.
new_width_case
{
  printf '# page\n\n'
  printf -- '- A first item whose sentence opens the list and runs on to a\n'
  printf '  second line of its own, where it ends. - A second item that a\n'
  printf '  re-wrap pulled up onto that continuation line.\n'
} > "$CASE/docs/page.md"
expect_width 1 'docs/page.md:4:marker' \
  'a bullet a re-wrap pulled onto a continuation line of the item above it'

# The marker spellings beside the `-` the three shipped merges were written
# with: a list is as readily starred, plussed or numbered, and a rule that
# reads one of the four grades a quarter of the population.
new_width_case
{
  printf '# page\n\n'
  printf -- '* A starred item that opens its own list and runs on to a second\n'
  printf '  line of its own, where it ends. * A second starred item pulled up.\n\n'
  printf -- '+ A plus item that opens its own list and runs on to a second line\n'
  printf '  of its own, where it ends. + A second plus item pulled up onto it.\n\n'
  printf -- '1. An ordered item that opens its own list and runs on to a second\n'
  printf '   line of its own, where it ends. 2. A second ordered item pulled up.\n'
} > "$CASE/docs/page.md"
expect_width 1 'docs/page.md:10:marker docs/page.md:4:marker docs/page.md:7:marker' \
  'a starred, a plussed and a numbered item, each merged onto a continuation'

# The marker rule is anchored on the end of a sentence because prose writes a
# bare `-` and a bare `+` mid-line far more often as arithmetic than as a
# bullet: the pages this walk grades carry a dozen of these and not one of
# them is a list.
new_width_case
{
  printf '# page\n\n'
  printf 'The row is measured to `hi_vcol - lo_vcol + 1`, computed once for\n'
  printf 'each row the block covers rather than for the block as a whole.\n'
} > "$CASE/docs/page.md"
expect_width 0 '' 'arithmetic written mid-line in prose, which is no list marker'

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

# `note` is empty when a precondition the case states held, and carries what
# went wrong when it did not: the seed cases below grade what the planted
# spelling actually made under TMPDIR as well as what the checker said about
# it, and one verdict per case is what the count at the end reads.
expect_temp_traps() {
  want_rc="$1"
  want="$2"
  desc="$3"
  note="${4:-}"
  out=$(bash "$CHECKER" --temp-traps "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" \
    | awk '/: makes a temp file with no EXIT trap removing it/ { c = $1; sub(/:$/, "", c); print c }' \
    | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//')
  if [ "$rc" = "$want_rc" ] && [ "$got" = "$want" ] && [ -z "$note" ]; then
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

# The append side of the same fail-open: a variable that accumulates a message
# rather than a path is a self-referential assignment naming the seed, so the
# harvest reads it as a holder, and a handler that prints it names the holder
# without removing anything. The temp root is never removed.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
log=""
cleanup() { echo "$log"; }
trap cleanup EXIT
ROOT=$(mktemp -d)
log="$log made $ROOT"
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a handler printing an accumulator that names the temp root but removes nothing'

# A brace group written at the handler header own indentation, which a body
# ending at a brace by indentation reads as the function close: the removal
# below it is dropped and a correct cleanup is reported.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
cleanup() {
{
echo done
}
  rm -rf "$ROOT"
}
trap cleanup EXIT
ROOT=$(mktemp -d)
PLANT
expect_temp_traps 0 '' 'a removal written after an unindented brace group in the handler body'

# A here-doc body carrying a `}` in column one, which every top-level handler
# in this population would read as its own close.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
cleanup() {
  cat > "$HOME/x.json" <<JSON
{
  "a": 1
}
JSON
  rm -rf "$ROOT"
}
trap cleanup EXIT
ROOT=$(mktemp -d)
PLANT
expect_temp_traps 0 '' 'a removal written after a here-doc body holding a brace in column one'

# The same brace, under a tag the writer quoted, and unpaired this time: a
# `<<` is read where the shell reads one and a quoted word is skipped
# everywhere else, so the tag has to be read out of its quotes or the body
# below it is code. Unpaired because a body whose braces balance reads the
# same either way and grades nothing -- this one closes the handler on the
# `}` in column one and the removal under it is never read.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
cleanup() {
  cat > "$HOME/x.json" <<'JSON'
}
JSON
  rm -rf "$ROOT"
}
trap cleanup EXIT
ROOT=$(mktemp -d)
PLANT
expect_temp_traps 0 '' 'a removal written after a here-doc body under a quoted tag'

# The removal one call deep, which is what half of these handlers write once
# the cleanup takes the root as an argument. The trap name is resolved a
# level; a name inside that level is resolved the same way or a correct
# script is refused.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
cleanup() { cleanup_root "$X"; }
cleanup_root() { rm -rf "$1"; }
trap cleanup EXIT
X=$(mktemp -d)
PLANT
expect_temp_traps 0 '' 'a removal one function call deep, reached through the call'

# The same shape graded on the removal rather than on the walk having
# followed the call: comment the callee's `rm` out and nothing is removed.
# The names differ from the case above because two scripts carrying one body
# is what `no_two_scripts_define_the_same_function_body` refuses, fixture
# text included.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
cleanup() { cleanup_root "$ROOT"; }
cleanup_root() {
  # rm -rf "$1"
  echo bye
}
trap cleanup EXIT
ROOT=$(mktemp -d)
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a commented-out removal one call deep, which removes nothing'

# The comment side of the fail-open Z2 closed: a handler that names a removal
# it does not run. Removal lines are read after the comment strip, so a `#`-led
# `rm` pairs nothing.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
cleanup() { # rm -rf "$X" used to live here
  echo bye
}
trap cleanup EXIT
X=$(mktemp -d)
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a commented-out removal in the handler, which removes nothing'

# A `<<` inside a quoted argument is not the here-doc operator, so the line
# after it is still the handler body and the removal there is read.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
cleanup() {
  printf '%s' "<<x"
  rm -rf "$X"
}
trap cleanup EXIT
X=$(mktemp -d)
PLANT
expect_temp_traps 0 '' 'a removal after a here-doc tag written inside a quoted argument'

# The operator that never terminates: everything below it is here-doc text
# the handler never runs, the `rm` included, so the root leaks and the walk
# has to say so rather than read the text as code. The trap is armed above
# the handler, which is where half of these scripts write it and the only
# ordering under which the swallow is visible -- a trap written below one
# takes the swallow too and the file reddens for having no trap at all.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat <<TAG
  rm -rf "$X"
}
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a removal swallowed by a here-doc that never terminates'

# The same swallow through the two spellings the reader nearly lost: a
# backslash before the tag quotes it the way single quotes do, and a blank
# between the operator and its word is one the shell allows. Read as no
# operator at all, the text under either is scanned as commands and the `rm`
# there pairs a root the handler never removes, so each has to answer what
# the bare spelling above answers.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat <<\TAG
  rm -rf "$X"
}
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a removal swallowed by a backslash-quoted here-doc tag that never terminates'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat << TAG
  rm -rf "$X"
}
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a removal swallowed by a blank-separated here-doc tag that never terminates'

# The closing half of the same two spellings: a body that does terminate
# leaves the removal below it code, so a reader that opened the body and
# never closed it reddens a file that is right.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat <<\TAG
  not a removal
TAG
  rm -rf "$X"
}
PLANT
expect_temp_traps 0 '' \
  'a removal below a backslash-quoted here-doc that terminates on its own tag'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat << TAG
  not a removal
TAG
  rm -rf "$X"
}
PLANT
expect_temp_traps 0 '' \
  'a removal below a blank-separated here-doc that terminates on its own tag'

# The two spellings a tag scan of letters alone reads wrong, in the swallowing
# direction: `<< -TAG` names the tag `-TAG` -- the `-` is the operator only
# where it touches it -- and a quoted tag runs to its closing quote, so
# `<<'EOF-1'` is not closed by the plain `EOF` line inside its body. Read as
# no tag and as `EOF`, the `rm` under each is scanned as a command the
# handler never runs.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat << -TAG
  rm -rf "$X"
}
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a removal swallowed by a blank-separated tag whose own first character is a dash'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat <<'EOF-1'
EOF
  rm -rf "$X"
}
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a removal swallowed by a quoted tag carrying a non-word character'

# The closing half of both, so neither case above can be answered by a reader
# that opens a body and never closes it: the dash tag closes on its own line,
# and the quoted one closes on the whole tag rather than on the prefix.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat << -TAG
  not a removal
-TAG
  rm -rf "$X"
}
PLANT
expect_temp_traps 0 '' \
  'a removal below a dash-first tag that terminates on its own line'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat <<'EOF-1'
EOF
  not a removal
EOF-1
  rm -rf "$X"
}
PLANT
expect_temp_traps 0 '' \
  'a removal below a quoted tag that terminates on the whole tag'

# The pair a single `-` prefix on the queued tag cannot tell apart: `<<--TAG`
# is the tab-stripping operator with the tag `-TAG`, and `<< -TAG` above is
# the plain operator with the same tag. Read as one, the tab-indented
# terminator here never closes its body and the removal below it is swallowed.
new_temp_trap_case
{
  printf 'trap cleanup EXIT\nX=$(mktemp -d)\ncleanup() {\n'
  printf '  cat <<--TAG\n  not a removal\n\t-TAG\n  rm -rf "$X"\n}\n'
} | write_temp_trap_script
expect_temp_traps 0 '' \
  'a removal below a tab-stripping operator whose tag itself starts with a dash'

# The rest of that class: a tag is a shell word, so it runs to the blank or
# operator that ends one and every other character goes into it. Read to the
# first character no variable name holds, `cat <<EOF.1` is the tag `EOF`, the
# plain `EOF` line below closes the body it never opened, and the `rm` the shell
# reads as data is read here as a removal.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat <<EOF.1
EOF
  rm -rf "$X"
}
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a removal swallowed by an unquoted tag carrying a character no name holds'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat <<EOF.1
EOF
  not a removal
EOF.1
  rm -rf "$X"
}
PLANT
expect_temp_traps 0 '' \
  'a removal below an unquoted dotted tag that terminates on the whole tag'

# A quote inside the tag word ends nothing for the shell: `cat <<EO"F"` is
# the tag `EOF` after quote removal, so the plain `EOF` line below closes the
# body and the removal under it is code. Read with the quotes still in the
# tag, the body never terminates, the rest of the file goes unread, and a
# handler that is right reddens.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  cat <<EO"F"
not a removal
EOF
  rm -rf "$X"
}
PLANT
expect_temp_traps 0 '' \
  'a removal below a tag whose word carries a quoted run the shell removes'

# The fail-open the same swallow used to open in the other direction: a
# handler that removes nothing, a tag inside a quoted argument, and a removal
# in a function the trap never calls. Read as the operator, the tag swallows
# the rest of the file and the handler pairs on that removal.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  echo "see <<TAG in the docs"
}
elsewhere() {
  rm -rf "$X"
}
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a handler removing nothing, which does not pair on a removal in a function it never calls'

# The same fail-open through the call rather than through the tag: the
# callee name sits inside a string the handler prints, so the handler runs
# no removal and the root leaks. A call is read where the shell reads one --
# outside every quote -- and nowhere else.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
SROOT=$(mktemp -d)
wipe_s() {
  rm -rf "$SROOT"
}
cleanup() {
  echo "run; wipe_s now"
}
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a handler naming its callee inside a string, which runs no removal'

# The other side of that boundary, so the case above cannot be answered by a
# walk that reads no call at all: the same two functions with the name in
# command position.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
CROOT=$(mktemp -d)
wipe_c() {
  rm -rf "$CROOT"
}
cleanup() {
  wipe_c
}
PLANT
expect_temp_traps 0 '' 'a handler whose callee removes, named in command position'

# The same boundary on the removal rather than on the call: an `rm` written
# inside a string is a word the handler prints. Read as a removal it pairs a
# root nothing ever removes, which is the fail-open the quote boundary exists
# to close, reached through the other slot the reader feeds.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
trap cleanup EXIT
X=$(mktemp -d)
cleanup() {
  echo "rm -rf $X"
}
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a handler printing an rm written inside a string, which removes nothing'

# The trap line is a call site of its own: the command a trap runs is a
# string the shell re-parses, so a quote-led word there is a name and the
# path beside it is an argument. Refused, a script that removes its root one
# call deep from the trap line reddens.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
wipe_q() {
  rm -rf "$1"
}
QROOT=$(mktemp -d)
trap 'wipe_q "$QROOT"' EXIT
PLANT
expect_temp_traps 0 '' 'the inline call on a trap line, whose callee removes what it is handed'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
wipe_q() {
  : "$1"
}
QROOT=$(mktemp -d)
trap 'wipe_q "$QROOT"' EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'the same inline call whose callee removes nothing'

# A trap command string written in double quotes escapes the quotes it wants
# inside it, and the string ends at the quote the escape does not cover: cut
# at the first `\"` instead, the command reads `rm -rf \`, the name it
# removes never reaches the pairing, and a script that removes its root
# reddens.
new_temp_trap_case
printf 'X=$(mktemp -d)\ntrap "rm -rf \\"$X\\"" EXIT\n' | write_temp_trap_script
expect_temp_traps 0 '' 'a trap whose command string escapes the quotes around its path'

# The other direction on the same shape: an escaped quote inside the command
# opens a string there, so an `rm` written inside it is a word the trap
# prints and pairs nothing.
new_temp_trap_case
printf 'X=$(mktemp -d)\ntrap "echo \\"rm -rf $X\\"" EXIT\n' | write_temp_trap_script
expect_temp_traps 1 'scripts/a.sh' \
  'a trap printing an rm inside its own escaped quotes, which removes nothing'

# What the shell expands and what it does not, on a trap command string. A
# single-quoted run in the re-parsed command is literal text: the first two
# below remove a path whose own name is `$X` and strand the temp root, which
# real bash confirms. The third is right to be clean -- the name is bare in the
# re-parsed command and expands when the trap fires -- and the fourth is the
# shape scripts/acceptance/artifacts.sh writes: the name is expanded into the
# string before the trap is armed, so the quotes the re-parse reads sit around
# a path rather than around a name.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
trap "rm -rf '\$X'" EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a trap whose escaped name sits in single quotes, which removes a path named for it'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf '$X'
}
trap cleanup EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'the same single-quoted removal written in a handler body'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf $'$X'
}
trap cleanup EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'the same removal written in an ANSI-C run, which expands no name either'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
trap "rm -rf \$X" EXIT
PLANT
expect_temp_traps 0 '' \
  'a trap whose name is bare in the re-parsed command, which expands when it fires'

# The same escape one file-position over, where nothing re-parses it: a
# handler body is script text the shell reads directly, so `\$X` there is a
# literal `$X` and the removal strands the root. Real bash leaves both of
# these roots behind. The trap line above stays green because the shell drops
# the backslash while it builds the string the trap re-parses.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "\$X"
}
trap cleanup EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a handler body removing an escaped name inside double quotes'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf \$X
}
trap cleanup EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a handler body removing an escaped name with no quotes around it'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
trap "rm -rf '$X'" EXIT
PLANT
expect_temp_traps 0 '' \
  'a trap whose path is expanded into the string before the trap is armed'

# A removal reaches the name only where the operand is its expansion. The four
# below each delete a path the temp root still holds -- bash expands the first
# to a backslash and a path, and the other three to a sibling, a suffixed name
# and a file inside the root -- while a walk reading the name out of the middle
# of the operand pairs the leak with a removal that never touched it. The last
# of them ran under real bash with TMPDIR pointed at a scratch directory and
# left its root behind.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "\\$X"
}
trap cleanup EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a handler body whose removal carries a literal backslash before the name'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "pre$X"
}
trap cleanup EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a handler body removing a name the operand prefixes, which is another path'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "$X.bak"
}
trap cleanup EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a handler body removing a suffixed name, which leaves the root it names'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "$X/sub"
}
trap cleanup EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a handler body removing a path inside the root, which removes none of it'

# The shapes that are the expansion of the name and stay green: a path under
# the root removes the root when the slash is all that follows it, the braced
# spelling is the same name, and a handler written on one line carries the
# list separator behind its operand -- which is the line
# scripts/capture-terminal-probe.sh writes.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "$X"/
}
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a removal written as the root with a trailing slash'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "${X}"
}
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a removal written in the braced spelling of the name'

# The four parameter-expansion forms whose value is $X when it is set --
# :?, :-, ? and - -- with and without a message, quoted or not. Each is the
# root's own expansion, unlike #, % and / below, which read out a different
# string and stay refused.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "${X:?}"
}
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a removal written as ${X:?}, quoted, with no message'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf ${X:?unset}
}
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a removal written as ${X:?msg}, unquoted, with a message'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "${X:-}"
}
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a removal written as ${X:-}, quoted, with no fallback'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "${X?unset}"
}
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a removal written as ${X?msg}, quoted, unset test only'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "${X-}"
}
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a removal written as ${X-}, quoted, unset test only'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "${X:?}"/
}
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a removal written as ${X:?} with a trailing path slash'

# #, % and / read out a different string than $X, so a removal written with
# any of them still leaves the root and stays refused.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "${X#/tmp}"
}
trap cleanup EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' 'a removal written as ${X#pattern}, a different string'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "${X%/sub}"
}
trap cleanup EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' 'a removal written as ${X%pattern}, a different string'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() {
  rm -rf "${X/sub/pre}"
}
trap cleanup EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' 'a removal written as ${X/pattern/repl}, a different string'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
cleanup() { rm -rf "$X"; }
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a handler written on one line, whose operand ends at a semicolon'

# The same operand rule on the trap line, where the name was expanded into the
# string before the trap was armed: the quotes there sit around a path, and
# what they hold is graded as the operand it is.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
trap "rm -rf '$X/sub'" EXIT
PLANT
expect_temp_traps 1 'scripts/a.sh' \
  'a trap whose expanded path names a file inside the root rather than the root'

# The root's own spelling: scripts/acceptance/artifacts.sh:244 writes
# rm -rf "${SELFCHECK_TMP:-}" in its abort handler.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
X=$(mktemp -d)
trap 'rm -rf "${X:-}"' EXIT
PLANT
expect_temp_traps 0 '' \
  'a trap whose armed command removes ${X:-}, the root'\''s own spelling'

# The wire between the harvest and the removal walk carries no byte a script
# can write: the two halves of a line arrive as two records, and a handler
# line holding the ASCII field separator the harvest once wrote between them
# still pairs. Split on that byte instead, the line is cut where the script
# wrote it, the name it removes is lost, and a right script reddens.
new_temp_trap_case
printf 'X=$(mktemp -d)\ncleanup() {\n  echo "A\034B"; rm -rf "$X"\n}\ntrap cleanup EXIT\n' \
  | write_temp_trap_script
expect_temp_traps 0 '' 'a removal on a handler line carrying an ASCII field separator byte'

# A callee defined in a file the script sources is the same removal written
# one file over. Resolved one level, beside the script or under scripts/lib/;
# a path none of those resolves is named in the verdict rather than refused
# in silence.
new_temp_trap_case
printf '%s\n' '#!/usr/bin/env bash' 'sourced_d() { rm -rf "$1"; }' > "$CASE/scripts/lib_d.sh"
write_temp_trap_script <<'PLANT'
HERE=$(dirname "$0")
. "$HERE/lib_d.sh"
DROOT=$(mktemp -d)
cleanup() {
  sourced_d "$DROOT"
}
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a callee defined in a file the script sources'

new_temp_trap_case
write_temp_trap_script <<'PLANT'
HERE=$(dirname "$0")
. "$HERE/nowhere.sh"
EROOT=$(mktemp -d)
cleanup() {
  sourced_e "$EROOT"
}
trap cleanup EXIT
PLANT
probe=$(bash "$CHECKER" --temp-traps "$CASE" 2>&1)
named=""
case "$probe" in
  (*'could not read what it sources at scripts/a.sh:3: "$HERE/nowhere.sh"'*) ;;
  (*) named="the verdict did not name the source line it could not read" ;;
esac
expect_temp_traps 1 'scripts/a.sh' \
  'a source the walk cannot resolve, named in the verdict rather than refused in silence' \
  "$named"

# The shape every source line in this population writes, which a walk cutting
# the operand at its first blank never resolves: the `$( )` holds two blanks
# of its own. Green here, because the callee the sourced file defines is what
# the handler removes through.
new_temp_trap_case
printf '%s\n' '#!/usr/bin/env bash' 'sourced_g() { rm -rf "$1"; }' > "$CASE/scripts/lib_g.sh"
write_temp_trap_script <<'PLANT'
source "$(dirname "$0")/lib_g.sh"
GROOT=$(mktemp -d)
cleanup() {
  sourced_g "$GROOT"
}
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a callee in a file sourced through a command substitution in the path'

# The same shape unresolvable, so the verdict is read as well as the
# resolution: what it names is the operand the line writes and never a
# fragment of it, which is what a reader has to go and look at.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
source "$(dirname "$0")/nowhere_h.sh"
HROOT=$(mktemp -d)
cleanup() {
  sourced_h "$HROOT"
}
trap cleanup EXIT
PLANT
probe=$(bash "$CHECKER" --temp-traps "$CASE" 2>&1)
named=""
case "$probe" in
  (*'could not read what it sources at scripts/a.sh:2: "$(dirname "$0")/nowhere_h.sh"'*) ;;
  (*) named="the verdict named a fragment of the operand rather than the operand" ;;
esac
expect_temp_traps 1 'scripts/a.sh' \
  'an unresolvable substitution path named in the verdict as the line writes it' \
  "$named"

# A quoted operand may hold a blank of its own, which is neither the blank that
# ends the operand nor one inside its `$( )`. Read to the first blank outside
# the substitution, the verdict names the fragment `"$(dirname "$0")/nope` --
# a path nobody can go and look at -- and the resolution tries it.
new_temp_trap_case
write_temp_trap_script <<'PLANT'
source "$(dirname "$0")/nope with blank.sh"
BROOT=$(mktemp -d)
cleanup() {
  sourced_b "$BROOT"
}
trap cleanup EXIT
PLANT
probe=$(bash "$CHECKER" --temp-traps "$CASE" 2>&1)
named=""
case "$probe" in
  (*'could not read what it sources at scripts/a.sh:2: "$(dirname "$0")/nope with blank.sh"'*) ;;
  (*) named="the verdict cut the operand at the blank inside its quotes" ;;
esac
expect_temp_traps 1 'scripts/a.sh' \
  'a quoted source operand holding a blank, named whole in the verdict' \
  "$named"

# The same operand resolvable, so the case above cannot be answered by a walk
# that reads the whole operand and resolves none of it: the callee the sourced
# file defines is what this handler removes through.
new_temp_trap_case
printf '%s\n' '#!/usr/bin/env bash' 'sourced_k() { rm -rf "$1"; }' \
  > "$CASE/scripts/lib k.sh"
write_temp_trap_script <<'PLANT'
source "$(dirname "$0")/lib k.sh"
KROOT=$(mktemp -d)
cleanup() {
  sourced_k "$KROOT"
}
trap cleanup EXIT
PLANT
expect_temp_traps 0 '' 'a callee in a sourced file whose path holds a blank'

# The wire between the sources awk and the shell carries no byte a script can
# write either: the line number, the path and the operand arrive as three
# records of their own. Split on the shared separator instead, an operand
# holding that byte is cut where the script wrote it, and the verdict names a
# path the walk never tried.
new_temp_trap_case
printf 'LIB=lib\n. "$LIB/x\034y.sh"\nLROOT=$(mktemp -d)\ncleanup() {\n  :\n}\ntrap cleanup EXIT\n' \
  | write_temp_trap_script
probe=$(bash "$CHECKER" --temp-traps "$CASE" 2>&1)
named=""
# the byte itself is not spelled here: the wildcard stands where the script
# wrote it, and a walk that split the record on it loses the tail of the
# operand rather than moving it
case "$probe" in
  (*'could not read what it sources at scripts/a.sh:3: "$LIB/x'*'y.sh"'*) ;;
  (*) named="the verdict cut the operand at the separator byte" ;;
esac
expect_temp_traps 1 'scripts/a.sh' \
  'a source operand carrying the separator byte, named whole in the verdict' \
  "$named"

# The seed class, one case per quoting shape. Green is "this spelling seeds":
# the handler removes `$N`, so a shape the harvest reads answers 0 and a shape
# it passes over leaves no name to pair and answers 1. Which shape makes a file
# is what the verdicts have to follow: the shape whose single quotes sit inside
# the double ones still runs mktemp, because they quote nothing there.
temp_trap_seed_case() {
  new_temp_trap_case
  printf 'trap %s EXIT\n%s\n' "'rm -f \"\$N\"'" "$1" | write_temp_trap_script
  expect_temp_traps "$2" "$3" "$4"
}
temp_trap_seed_case 'N=$(mktemp)' 0 '' \
  'a bare command substitution seeding the temp name'
temp_trap_seed_case 'N="$(mktemp)"' 0 '' \
  'a double-quoted command substitution seeding the temp name'
temp_trap_seed_case "N=\"'\$(mktemp)'\"" 0 '' \
  'single quotes inside the double ones, which still runs mktemp, seeding it'
temp_trap_seed_case "N='\$(mktemp)'" 1 'scripts/a.sh' \
  'a single-quoted spelling, which makes no file and seeds nothing'
temp_trap_seed_case "N=\$'\$(mktemp)'" 1 'scripts/a.sh' \
  'an ANSI-C quoted spelling, which makes no file and seeds nothing'

# The prefixed shape, which makes a file and still seeds nothing: the name
# holds `pre/tmp/tmp.XXXX` and not the temp path, so a removal over it removes
# nothing that was made. Excluded by name rather than by accident, and
# fail-closed -- a file whose `mktemp` reaches no recognised name is reported
# rather than passed, which is the verdict this case asks for. The fixture is
# run first, so what the spelling makes is graded and not assumed.
new_temp_trap_case
mkdir -p "$CASE/tmp"
printf '#!/usr/bin/env bash\nN="pre$(mktemp)"\nprintf %%s "$N"\n' > "$CASE/seed.sh"
made=$(TMPDIR="$CASE/tmp" bash "$CASE/seed.sh")
note=""
if [ ! -f "${made#pre}" ]; then
  note="the prefixed spelling made no file, so the exclusion below grades nothing"
fi
printf 'trap %s EXIT\nN="pre$(mktemp)"\n' "'rm -f \"\$N\"'" | write_temp_trap_script
expect_temp_traps 1 'scripts/a.sh' \
  'a prefixed command substitution, which makes a file whose name is not the temp path' \
  "$note"

# ---------------------------------------------------------------------------
# the root a temp file is made under: a `mktemp` with no template answers
# under TMPDIR, which is /tmp wherever nothing set that. Four cases, each
# red when the one rule it names is taken out of the walk.
# ---------------------------------------------------------------------------
expect_temp_roots() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "$CHECKER" --temp-roots "$CASE" 2>&1)
  rc=$?
  # both verdicts read: the walk refuses a call that names no root and a
  # template rooted at the shared tmpfs, and a harvest taking only the first
  # can never grade the second
  got=$(printf '%s\n' "$out" \
    | awk '/: makes a temp file with no template saying where it goes/ ||
           /: roots a temp file at TMPDIR, which is the shared tmpfs/ {
        c = $1; sub(/:[0-9]*:$/, "", c); print c
      }' \
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
printf 'f=$(mktemp)\n' | write_temp_trap_script
expect_temp_roots 1 'scripts/a.sh' 'a mktemp handed nothing at all'

# the same call with only its options, which is the spelling this population
# actually shipped: an option is not a root.
new_temp_trap_case
printf 'f=$(mktemp -d)\n' | write_temp_trap_script
expect_temp_roots 1 'scripts/a.sh' 'a mktemp handed an option and no template'

# the operand reading, which is the whole of what makes the case above red:
# a walk that flagged every mktemp would redden this one too.
new_temp_trap_case
printf 'f=$(mktemp -d "$(scratch_root)/a-XXXXXX")\n' | write_temp_trap_script
expect_temp_roots 0 '' 'a mktemp handed a template under a named root'

# the single-quote reading: the case files plant whole scripts through
# `printf` and a spelling written there is a fixture, not a call.
new_temp_trap_case
printf "printf 'f=\$(mktemp)\\\\n'\n" | write_temp_trap_script
expect_temp_roots 0 '' 'a bare mktemp written inside a single-quoted string'

# the here-doc reading, which is where the rest of those fixtures live.
new_temp_trap_case
printf 'cat <<%s\nf=$(mktemp)\nPLANT\n' "'PLANT'" | write_temp_trap_script
expect_temp_roots 0 '' 'a bare mktemp written in a here-doc body'

# the join the shell makes on a trailing backslash: read as far as the
# backslash, the call is handed its options and a backslash, and the
# backslash reads as the root the rule asks for.
new_temp_trap_case
printf 'f=$(mktemp \\\n  -d)\n' | write_temp_trap_script
expect_temp_roots 1 'scripts/a.sh' 'a mktemp whose options continue onto the next line'

# the long spelling of the same option, which a walk reading only `-x` takes
# for an operand because the second dash is not a letter.
new_temp_trap_case
printf 'f=$(mktemp --directory)\n' | write_temp_trap_script
expect_temp_roots 1 'scripts/a.sh' 'a mktemp handed a long option and no template'

# the shared tmpfs written longhand, which is a root and still the tmpfs the
# bare call lands on.
new_temp_trap_case
printf 'f=$(mktemp -d "${TMPDIR:-/tmp}/a-XXXXXX")\n' | write_temp_trap_script
expect_temp_roots 1 'scripts/a.sh' 'a template rooted at TMPDIR outside scripts/acceptance'

# the one directory that spelling is allowed in, and the whole of why the
# case above is keyed on the path rather than on the operand alone.
new_temp_trap_case
mkdir -p "$CASE/scripts/acceptance"
printf '#!/usr/bin/env bash\nf=$(mktemp -d "${TMPDIR:-/tmp}/view-acc-XXXXXX")\n' \
  > "$CASE/scripts/acceptance/leg"
expect_temp_roots 0 '' 'the same template under scripts/acceptance, where a session root is chosen by hand'

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
  # both required arms read: the directory verdict says `x/ directory missing`
  # and the file one says `README.md missing`, and a harvest that takes only
  # the first can never grade the second
  got=$(printf '%s\n' "$out" \
    | sed -n -e 's/^STYLE FAIL: \(.*\) directory missing$/\1/p' \
      -e 's/^STYLE FAIL: \([^ ]*\) missing$/\1/p' \
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
mkdir -p "$CASE/crates" "$CASE/compat" "$CASE/corpus" "$CASE/docs" "$CASE/.claude/rules"
: > "$CASE/README.md"
expect_required 1 'scripts/ scripts/acceptance/' \
  'a run from a root whose scripts/ has moved, which the walks alone pass silently'

new_required_case
mkdir -p "$CASE/scripts/acceptance" "$CASE/compat" "$CASE/corpus" "$CASE/docs" \
  "$CASE/.claude/rules"
: > "$CASE/README.md"
expect_required 1 'crates/' \
  'the same verdict for the sibling directory the run has always failed closed on'

# The file arm, which the two cases above cannot reach: each plants a README.md
# so that the three walks behind `[ -f README.md ]` are the only ones the arm
# guards, and a root with every directory and no page is what grades it.
new_required_case
mkdir -p "$CASE/crates" "$CASE/scripts/acceptance" "$CASE/compat" "$CASE/corpus" \
  "$CASE/docs" "$CASE/.claude/rules"
expect_required 1 'README.md' \
  'a run from a root whose README.md has moved, which the three walks behind it pass silently'

# The directory the width walk added to the guarded set, which is required
# by the same rule as the six beside it: the walk over it is behind a guard,
# and a guard with no else reads as a pass.
new_required_case
mkdir -p "$CASE/crates" "$CASE/scripts/acceptance" "$CASE/compat" "$CASE/corpus" \
  "$CASE/docs"
: > "$CASE/README.md"
expect_required 1 '.claude/rules/' \
  'a run from a root whose convention pages have moved, which the width walk passes silently'

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
#
# The `ln` word wherever it sits, rather than after a list of the operators
# a command can follow: that list left out `!`, `time`, `exec` and a brace
# group, which are command starts too, and chasing it is endless. A word
# boundary is what `ln=3`, `ln.sh` and `xln -s` need, and the shared reader
# is what a comment and a here-doc body need. The flag run is read as words
# with no `n` in any of them, so `-s -n` written apart is as green as `-sn`,
# `--no-dereference` is green for the same reason, and the run has to end at
# an operand, which is what makes a trailing `-n` visible. `--` and
# `--symbolic` are flag words like any other.
#
# The over-read, on purpose: a string literal is code to the reader, so a
# link assembled in one is graded. The alternative blinds the walk to every
# generator in the tree, and the population carries no such string.
FLAGLESS_LN='(^|[^A-Za-z0-9_])ln[[:space:]]+(--?[A-MO-Za-mo-z-]*[[:space:]]+)*--?[A-MO-Za-mo-z-]*s[A-MO-Za-mo-z-]*([[:space:]]+--?[A-MO-Za-mo-z-]*)*[[:space:]]+[^-[:space:]]'
flagless_links() {
  awk -v SQ="'" -v LN="$FLAGLESS_LN" "$SCRIPT_CODE_AWK"'
    { script_code_scan($0); if (CODE ~ LN) { print FILENAME ":" FNR ": " $0 } }
  ' "$@"
}

new_pin_case
if ! script_population_read "$TREE" > /dev/null || [ -z "$SCRIPT_POPULATION" ]; then
  deref="the population read answered nothing, so no link was graded"
else
  deref=$(cd "$TREE" && flagless_links $SCRIPT_POPULATION)
fi
expect_pin 'every link the population makes written -n, so none writes through one' "$deref"

# planted, because the walk above is worth what it catches and the tree it
# reads carries no flagless link to catch. The spelling is assembled through
# a variable so planting one here does not make this file a hit.
new_pin_case
flagless='ln -s'
{
  printf '#!/bin/sh\n'
  printf 'mkdir -p "$d" && %s "$a" "$b"\n' "$flagless"
} > "$CASE/chained.sh"
missed=""
if [ -z "$(flagless_links "$CASE/chained.sh")" ]; then
  missed="a link written after && went uncaught"
fi
expect_pin 'the link walk reads ln wherever a command starts, not only at the start of a line' "$missed"

# The mirror: the flags written apart are the same link as `-sn`, and a walk
# that reddens them teaches the population to write the spelling that
# follows a link.
new_pin_case
apart='ln -s -n'
{
  printf '#!/bin/sh\n'
  printf '%s "$a" "$b"\n' "$apart"
} > "$CASE/apart.sh"
caught=$(flagless_links "$CASE/apart.sh")
expect_pin 'the link walk passes -s -n, the same link with its flags written apart' "$caught"

# The positions the operator list left out, `if` among them, and the two flag
# spellings it could not reach. Every line here is a link written without -n,
# so every line is a finding; the string-literal line is the over-read the
# walk takes on purpose, and it is asserted rather than tolerated.
new_pin_case
starts="$CASE/starts.sh"
sym="ln --symbolic"
{
  printf '#!/bin/sh\n'
  printf '! %s "$a" "$b"\n' "$flagless"
  printf 'if %s "$a" "$b"; then :; fi\n' "$flagless"
  printf 'time %s "$a" "$b"\n' "$flagless"
  printf 'exec %s "$a" "$b"\n' "$flagless"
  printf '{ %s "$a" "$b"; }\n' "$flagless"
  printf '%s -- "$a" "$b"\n' "$flagless"
  printf '%s "$a" "$b"\n' "$sym"
  printf 'msg="%s a b"\n' "$flagless"
} > "$starts"
found=$(flagless_links "$starts" | wc -l | tr -d ' ')
missed=""
if [ "$found" != "8" ]; then
  missed=$(printf '8 links planted, %s read back\n%s\n' "$found" "$(flagless_links "$starts")")
fi
expect_pin 'the link walk reads ln after !, if, time, exec and a brace, through -- and --symbolic, and inside a string' "$missed"

# The other side of the same boundary: prose and a here-doc body are not
# commands, so the walk holds its tongue there whatever punctuation sits in
# front of the words. The comment carries a `;`, which is what the operator
# list used to redden.
new_pin_case
quiet="$CASE/quiet.sh"
{
  printf '#!/bin/sh\n'
  printf '# see foo; %s a b for why\n' "$flagless"
  printf 'cat <<EOF\n'
  printf '%s a b\n' "$flagless"
  printf 'EOF\n'
} > "$quiet"
expect_pin 'the link walk grades no comment and no here-doc body' "$(flagless_links "$quiet")"

# The same boundary read the other way: a `<<TAG` inside a quoted argument is
# text and opens no body, so the lines under it are commands still. Read as an
# operator it blinds the walk from there to the end of that file, which is the
# direction no walk over this population may fail in -- and the two spellings
# planted here are the two this file and its drift sibling write themselves,
# one in an argument and one in a `printf` format.
new_pin_case
quoted="$CASE/quoted-tag.sh"
apos="'"
{
  printf '#!/bin/sh\n'
  printf 'msg=%scat <<EOF%s\n' "$apos" "$apos"
  printf '%s "$a" "$b"\n' "$flagless"
  printf 'printf %scat <<EOF\\n%s\n' "$apos" "$apos"
  printf '%s "$c" "$d"\n' "$flagless"
} > "$quoted"
found=$(flagless_links "$quoted" | wc -l | tr -d ' ')
missed=""
if [ "$found" != "2" ]; then
  missed=$(printf '2 links planted under a quoted tag spelling, %s read back\n%s\n' \
    "$found" "$(flagless_links "$quoted")")
fi
expect_pin 'the link walk reads the lines under a tag spelling written in a string' "$missed"

# The two line shapes where the reader's quote pass and the shared tokenizer
# disagree, which is what tells the halves of that boundary apart: a string
# closing on the line that carries `cat <<EOF` is an opener to anything
# reading the raw line and none to a reader that knows the quote opened two
# lines up, and a left shift whose operand is a name is a tag to a regex and
# none to the tokenizer's arithmetic branch. Either read swallows the link
# under it. Without these two, a matrix reddens only when the tokenizer and
# the feed it is handed are wrong together, and grades neither alone.
new_pin_case
disagree="$CASE/disagreeing-feeds.sh"
{
  printf '#!/bin/sh\n'
  printf 'msg=%sa string that opens here\n' "$apos"
  printf 'cat <<EOF%s\n' "$apos"
  printf '%s "$a" "$b"\n' "$flagless"
  printf 'if (( 1 << n )); then :; fi\n'
  printf '%s "$c" "$d"\n' "$flagless"
} > "$disagree"
found=$(flagless_links "$disagree" | wc -l | tr -d ' ')
missed=""
if [ "$found" != "2" ]; then
  missed=$(printf '2 links planted under the two disagreeing feeds, %s read back\n%s\n' \
    "$found" "$(flagless_links "$disagree")")
fi
expect_pin 'the link walk reads the lines under a tag only a raw line or a regex opens' "$missed"

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
# Every guard spelling rather than the two the checker happens to write today:
# `[ -e ]`, `[ -r ]`, `[ -s ]`, a `[[ ... ]]` and a bracket-free `test -f` all
# guard a walk the same way and each is fail-open the moment it is written, so
# the harvest that has to see the next one is the one that sees them all. The
# negation is a guard too and is the spelling the checker under test actually
# writes -- `[ ! -d "$required" ]` three times -- and it reads nothing here
# only because those three name a variable.
#
# `test` is an English word, so it is read where a command starts and nowhere
# else, through the list the split-`case` scan shares: `cargo test -p
# view-core` is an argument and not a guard on a directory named view-core.
# The line is read as code, so a guard written in a comment or in a here-doc
# body is neither.
#
# A `[ -d x ] || mkdir x` is left out: the run makes the path, so requiring it
# to exist beforehand demands what the script is there to create. The whole
# line is passed over, which is as wide as a create-if-missing is written.
TEST_GUARD="$SCRIPT_COMMAND_START"'test[[:space:]]+!?[[:space:]]*-[a-zA-Z][[:space:]]+[A-Za-z0-9_/.-]+'
# bracket literals spelled `[[]` and `[]]` rather than backslashed: this value
# reaches awk through -v, which reads an escape sequence out of it first
BRACKET_GUARD='[[][[]?[[:space:]]+!?[[:space:]]*-[a-zA-Z][[:space:]]+[A-Za-z0-9_/.-]+[[:space:]]+[]][]]?'
guarded_paths() {
  awk -v SQ="'" -v TG="$TEST_GUARD" -v BG="$BRACKET_GUARD" "$SCRIPT_CODE_AWK"'
    # the bracket form ends in its own closing bracket, so the path is the
    # word before the last; the bracket-free form ends at the path
    function nth_last(t, k,   w, n) {
      n = split(t, w, /[[:space:]]+/)
      return w[n - k]
    }
    {
      script_code_scan($0)
      line = CODE
      if (line ~ /[|][|][[:space:]]*mkdir/) { next }
      s = line
      while (match(s, BG)) {
        print nth_last(substr(s, RSTART, RLENGTH), 1)
        s = substr(s, RSTART + RLENGTH)
      }
      s = line
      while (match(s, TG)) {
        print nth_last(substr(s, RSTART, RLENGTH), 0)
        s = substr(s, RSTART + RLENGTH)
      }
      if (line ~ /^for dir in .*; do$/) {
        t = line
        sub(/^for dir in /, "", t)
        sub(/; do$/, "", t)
        n = split(t, w, /[[:space:]]+/)
        for (i = 1; i <= n; i++) { print w[i] }
      }
    }
  ' "$1" | LC_ALL=C sort -u
}
guarded=$(guarded_paths "$CHECKER")
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

# The harvest over one planted file per spelling, because the checker writes
# two of them today and the pin is worth what it would see in the next one.
# The negated spellings are there because a negation is how this checker
# writes its own three guards, the `|| mkdir` because a path the run creates
# is not a path the run requires, the brace group because a guard written
# inside one is still a guard, and the commented line because the harvest
# reads code. `cargo test -p` sits there too: a walk is guarded by `test` in
# command position and by nothing else.
new_pin_case
{
  printf '#!/usr/bin/env bash\n'
  printf 'if [ -e alpha ]; then :; fi\n'
  printf 'if [ -f bravo ]; then :; fi\n'
  printf 'if [ -d charlie ]; then :; fi\n'
  printf 'if [ -r delta ]; then :; fi\n'
  printf 'if [ -s echoed ]; then :; fi\n'
  printf 'if [[ -d foxtrot ]]; then :; fi\n'
  printf 'if test -f golf; then :; fi\n'
  printf 'for dir in hotel india; do\n'
  printf '  :\n'
  printf 'done\n'
  printf 'cargo test -p juliet\n'
  printf 'if [ ! -d kilo ]; then :; fi\n'
  printf '[ -d lima ] || mkdir lima\n'
  printf 'test ! -e mike && :\n'
  printf 'if [[ ! -f november ]]; then :; fi\n'
  printf '{ test -f oscar; }\n'
  printf '# [ -d papa ]\n'
  printf 'if false; then :\nelif test -f papaya; then :\nfi\n'
  printf 'while test -f quebec; do break; done\n'
  printf 'until test -f romeo; do break; done\n'
} > "$CASE/guards.sh"
spellings=$(guarded_paths "$CASE/guards.sh" | tr '\n' ' ' | sed 's/ *$//')
missed=""
if [ "$spellings" != 'alpha bravo charlie delta echoed foxtrot golf hotel india kilo mike november oscar papaya quebec romeo' ]; then
  missed="the guard harvest answered [$spellings]"
fi
expect_pin 'every guard spelling harvested, the negated ones among them, after each of elif, while and until, with a create-if-missing, a comment and a cargo test -p read as none' "$missed"

# The other half of the same guard: `[ -d docs ] && targets="$targets docs"`
# is safe mid-body and safe last in a loop body, where only the status moves.
# Written last in a function or a command substitution the status becomes the
# caller verdict, and a missing directory then reads as a failure. The shell
# page says the population writes one such line and that its `|| true`
# defuses it, which is a claim about the tree rather than about the shell, so
# it is walked rather than asserted: the list line, the line that closes over
# it, and whether either carries the `|| true`.
# where the list may start is the one boundary, not a fourth spelling of it:
# anchored at the line start this walk read nothing of `x=1; [ -d docs ] &&
# ...` and nothing of the same list inside a `{ ...; }` group, both of which
# carry the verdict the same way
guarded_list_tails() {
  awk -v SQ="'" -v GL="$SCRIPT_COMMAND_START"'(!|[[][[]?|test)[[:space:]]' "$SCRIPT_CODE_AWK"'
    function verdict(l) {
      return (l ~ /[|][|][[:space:]]*(true|:)/) ? "defused" : "live"
    }
    FNR == 1 { pend = "" }
    {
      script_code_scan($0)
      if (CODE ~ /^[[:space:]]*$/) { next }
      if (pend != "" && CODE ~ /^[[:space:]]*([})]|done[)]|fi[)]|esac[)])/) {
        print FILENAME ":" pline ": " verdict(pend " " CODE)
      }
      pend = ""
      if (CODE ~ GL && CODE ~ /&&/) { pend = CODE; pline = FNR }
    }
  ' "$@"
}

new_pin_case
if ! script_population_read "$TREE" > /dev/null || [ -z "$SCRIPT_POPULATION" ]; then
  tails="the population read answered nothing, so no list was graded"
else
  tails=$(cd "$TREE" && guarded_list_tails $SCRIPT_POPULATION)
fi
undefused=$(printf '%s\n' "$tails" | grep ': live$') || true
named=$(printf '%s\n' "$tails" | sed -n 's/^\(.*\):[0-9]*: defused$/\1/p' \
  | LC_ALL=C sort -u | tr '\n' ' ' | sed 's/ *$//')
drifted="$undefused"
if [ "$named" != 'scripts/check-budget-drift-cases.sh' ]; then
  drifted=$(printf '%sthe defused instances are [%s], where the page names one\n' \
    "${drifted:+$drifted
}" "$named")
fi
expect_pin 'the one guarded list written last in a substitution is the one the page names, and it is defused' "$drifted"

# planted, because the walk above is worth what it catches and the tree it
# reads carries only the defused instance
new_pin_case
{
  printf '#!/bin/sh\n'
  printf 'pick() {\n'
  printf '  [ -d docs ] && targets="$targets docs"\n'
  printf '}\n'
} > "$CASE/tail.sh"
missed=""
case "$(guarded_list_tails "$CASE/tail.sh")" in
  *': live') ;;
  *) missed="a guarded list written last in a function went unread" ;;
esac
expect_pin 'the guarded-list walk reads a list written last in a function with no || true' "$missed"

# The two positions a line anchor passed over, which are the positions the
# shared list exists to name: the list written after a `;` and the same list
# inside a brace group. Both carry the caller verdict exactly as the
# line-start spelling does.
new_pin_case
{
  printf '#!/bin/sh\n'
  printf 'pick() {\n'
  printf '  x=1; [ -d docs ] && targets="$targets docs"\n'
  printf '}\n'
  printf 'grab() {\n'
  printf '  { [ -d docs ] && targets="$targets docs"; }\n'
  printf '}\n'
} > "$CASE/tail-mid.sh"
missed=""
found=$(guarded_list_tails "$CASE/tail-mid.sh" | grep -c ': live$') || found=0
if [ "$found" != "2" ]; then
  missed=$(printf '2 guarded lists planted off the line start, %s read back\n%s\n' \
    "$found" "$(guarded_list_tails "$CASE/tail-mid.sh")")
fi
expect_pin 'the guarded-list walk reads a list written after a semicolon and inside a brace group' "$missed"

# A value two scans share sits in one file, and the next spelling of it is
# what the scan after those two invents: where a command starts had been
# drawn three different ways before it was written down, a fourth was written
# beside the third the same week, and the ASCII field separator was then
# written twice over, under two names and in two spellings. Walked rather
# than asserted, over the assignment that is how each of those was written --
# an anchored group, the punctuation a command can follow, the words it can
# follow, or the separator byte in any of its three spellings -- with the
# definitions themselves and every line that reads one passed over. The
# ceiling: a value written straight into an `awk -v` rather than into a
# variable is not read here, and nothing but this file says so.
shared_constant_spellings() {
  awk -v SQ="'" -v SEP="$SCRIPT_FIELD_SEP" "$SCRIPT_CODE_AWK"'
    {
      script_code_scan($0)
      if (CODE ~ /^SCRIPT_COMMAND_START=/ || CODE ~ /^SCRIPT_FIELD_SEP=/) { next }
      if (index(CODE, "$SCRIPT_COMMAND_START") > 0) { next }
      if (index(CODE, "$SCRIPT_FIELD_SEP") > 0) { next }
      if (CODE !~ /^[[:space:]]*[A-Za-z_][A-Za-z0-9_]*=/) { next }
      if (index(CODE, "(^|[;") > 0 ||
          index(CODE, "^[[:space:]]*(") > 0 ||
          index(CODE, "(if|then|do") > 0 ||
          index(CODE, "\\034") > 0 || index(CODE, "\\x1c") > 0 ||
          index(CODE, SEP) > 0) {
        print FILENAME ":" FNR ": " $0
      }
    }
  ' "$@"
}

new_pin_case
if ! script_population_read "$TREE" > /dev/null || [ -z "$SCRIPT_POPULATION" ]; then
  spellings="the population read answered nothing, so no list was graded"
else
  spellings=$(cd "$TREE" && shared_constant_spellings $SCRIPT_POPULATION)
fi
expect_pin 'one spelling of where a command starts and one of the field separator, read by every walk that needs either' "$spellings"

# planted, because the walk above is worth what it catches and the tree it
# reads carries none: the private list deleted from this file and the private
# separator deleted from the userland scan, each written back in the shape it
# was written in, plus the two spellings of that byte nobody has written yet.
new_pin_case
cat > "$CASE/private.sh" <<'PRIV'
#!/bin/sh
GUARDED_LIST='^[[:space:]]*(!|[[][[]?|test)[[:space:]]'
printf '%s\n' "$GUARDED_LIST"
PRIV
cat > "$CASE/private-sep.sh" <<'PSEP'
#!/bin/sh
SEP=$'\034'
printf '%s' "$SEP"
PSEP
cat > "$CASE/private-hex.sh" <<'PHEX'
#!/bin/sh
SEP=$(printf '\x1c')
printf '%s' "$SEP"
PHEX
# the byte itself rather than a spelling of it, written by printf so that this
# file carries no control character of its own
printf '#!/bin/sh\nSEP="\034"\nprintf %s "$SEP"\n' "'%s'" > "$CASE/private-byte.sh"
missed=""
for planted in private.sh private-sep.sh private-hex.sh private-byte.sh; do
  if [ -z "$(shared_constant_spellings "$CASE/$planted")" ]; then
    missed=$(printf '%s%s\n' "${missed:+$missed
}" "the value planted in $planted went unread")
  fi
done
expect_pin 'the spelling walk reads a private list and a private separator written beside the shared ones' "$missed"

# A backslash inside a bracket expression is undefined in POSIX awk, and the
# three awks this tree runs on read it differently, so the tree writes none:
# `[ \t]` is spelled `[[:space:]]` and an escaped quote is read by index.
# Walked rather than asserted, because the construct is one keystroke from
# being written again and no awk of the three says a word about it.
#
# A `[` where a command starts is the test builtin and opens no bracket
# expression, so the same list that names that position names it here. The
# backslash counted is one a regex engine reads as an escape (`\t`, `\n`, a
# backslash, either bracket, and a digit); a `\"` is the shell quoting its
# own argument and the regex engine never sees it.
#
# The digit escape is on the list because an octal byte class is the one
# spelling of a byte range that reads as a bracket expression everywhere and
# is defined nowhere -- `[\200-\277]`, the continuation-byte range the
# character measure used to carry, which is built out of two literal bytes by
# sprintf instead. Measured over this tree's scripts, it reddens that one
# site and nothing else.
#
# An escaped bracket is on that list because the set without it passed the
# spelling the doc-figure walk shipped -- `[`*~()>\[\],;:"]`, which gawk and
# mawk strip the punctuation for and busybox awk strips none of -- while the
# rule's own sentence bans every backslash. The list is enumerated and not
# "any backslash" because this walk reads shell text and cannot tell an awk
# regex from a shell glob: a `case` pattern's `[!\ ]` carries a backslash no
# regex engine ever sees.
bracket_backslash_sites() {
  awk -v SQ="'" -v CS="$SCRIPT_COMMAND_START" "$SCRIPT_CODE_AWK"'
    function undefined_bracket(s,   i, n, c, nxt, inb, first, hit) {
      n = length(s); inb = 0; first = 0; hit = 0
      for (i = 1; i <= n; i++) {
        c = substr(s, i, 1)
        if (!inb) {
          if (c == "\\") { i++; continue }
          if (c != "[") { continue }
          if (substr(s, 1, i) ~ CS "[[]$") { continue }
          inb = 1; first = 1; hit = 0
          continue
        }
        if (c == "^" && first) { continue }
        if (c == "]" && !first) {
          if (hit) { return 1 }
          inb = 0; first = 0
          continue
        }
        if (c == "\\") {
          nxt = substr(s, i + 1, 1)
          if (nxt == "\\" || nxt == "t" || nxt == "n" ||
              nxt == "[" || nxt == "]" || nxt ~ /^[0-9]$/) { hit = 1 }
          i++
        }
        first = 0
      }
      return 0
    }
    {
      script_code_scan($0)
      if (undefined_bracket(CODE)) { print FILENAME ":" FNR ": " $0 }
    }
  ' "$@"
}

new_pin_case
if ! script_population_read "$TREE" > /dev/null || [ -z "$SCRIPT_POPULATION" ]; then
  brackets="the population read answered nothing, so no bracket expression was graded"
else
  brackets=$(cd "$TREE" && bracket_backslash_sites $SCRIPT_POPULATION)
fi
expect_pin 'no backslash inside a bracket expression, which POSIX awk leaves undefined' "$brackets"

# planted, both sides: the three spellings the tree used to write -- the two
# awk regexes and the octal byte range the character measure carried -- and
# the shell test whose brackets are a command rather than a bracket
# expression.
new_pin_case
cat > "$CASE/bracket.sh" <<'BRK'
#!/bin/sh
awk '{ n = split($0, w, /[ \t]+/); print n }' "$1"
BRK
cat > "$CASE/bracket-quote.sh" <<'BRQ'
#!/bin/sh
awk '$0 ~ /[^\\]";$/ { print }' "$1"
BRQ
cat > "$CASE/bracket-square.sh" <<'BRS'
#!/bin/sh
awk '{ gsub(/[`*~()>\[\],;:"]/, "", $0); print }' "$1"
BRS
cat > "$CASE/bracket-octal.sh" <<'BRO'
#!/bin/sh
awk '{ t = $0; gsub(/[\200-\277]/, "", t); print length(t) }' "$1"
BRO
cat > "$CASE/bracket-test.sh" <<'BRT'
#!/bin/sh
if [ -n "$1" ] && [ "$1" != $'\n' ]; then
  printf '%s\n' "$1"
fi
BRT
missed=""
for planted in bracket.sh bracket-quote.sh bracket-square.sh bracket-octal.sh; do
  if [ -z "$(bracket_backslash_sites "$CASE/$planted")" ]; then
    missed=$(printf '%s%s\n' "${missed:+$missed
}" "the backslash planted in $planted went unread")
  fi
done
if [ -n "$(bracket_backslash_sites "$CASE/bracket-test.sh")" ]; then
  missed=$(printf '%s%s\n' "${missed:+$missed
}" "a shell test in command position was read as a bracket expression")
fi
expect_pin 'the bracket walk reads all four undefined spellings and reads no shell test as one' "$missed"

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

# A bad revision used to let git's own "fatal: bad revision" reach stderr and
# still exit 0 with no findings, indistinguishable from a clean tree.
new_pin_case
REWRAP="$(cd "$(dirname "$0")" && pwd)/check-rewrap-structure.sh"
out=$(cd "$TREE" && bash "$REWRAP" nosuchrev HEAD 2>&1) && rc=0 || rc=$?
bad_rev=""
if [ "$rc" != 2 ]; then
  bad_rev="a nonsense revision exited $rc rather than 2"
fi
case "$out" in
  (*nosuchrev*) ;;
  (*)
    bad_rev=$(printf '%sthe bad revision was never named: [%s]\n' \
      "${bad_rev:+$bad_rev
}" "$out")
    ;;
esac
expect_pin 'a nonsense revision to check-rewrap-structure.sh exits 2 naming it, rather than 0 with no findings' "$bad_rev"

# The helper-order guard at the top of this file, graded on a copy of the
# file itself: a case that calls a helper above its definition ran as rc 127
# and was counted by nothing, so the copy plants exactly that call and the
# guard has to refuse the whole run naming it before a single case starts.
new_pin_case
awk 'NR == 61 { print "expect_pin \"planted\" \"\"" } { print }' "$0" > "$CASE/cases.sh"
out=$(bash "$CASE/cases.sh" 2>&1) && rc=0 || rc=$?
bad_order=""
if [ "$rc" != 2 ]; then
  bad_order="a helper called above its definition exited $rc rather than 2"
fi
case "$out" in
  (*"expect_pin called before its definition"*) ;;
  (*)
    bad_order=$(printf '%sthe misordered helper was never named: [%s]\n' \
      "${bad_order:+$bad_order
}" "$(printf '%s' "$out" | head -3)")
    ;;
esac
expect_pin 'a helper called above its definition refuses the run with rc 2 naming it, rather than skipping the case at rc 127' "$bad_order"

# ---------------------------------------------------------------------------
# The stance walk over README.md and docs/: one red per shape the rule names
# and one green per exemption it licenses. Every exemption is read off the
# line, so a case plants the shape and never a filename the walk knows.
# ---------------------------------------------------------------------------
new_frames_case() {
  n=$((n + 1))
  CASE="$WORK/case$n"
  mkdir -p "$CASE/docs"
  printf '# view\n\nA line of prose that says what is true and stops.\n' \
    > "$CASE/README.md"
  printf '# page\n\nAnother line that says what is true and stops.\n' \
    > "$CASE/docs/page.md"
}

expect_frames() {
  want_rc="$1"
  want="$2"
  desc="$3"
  out=$(bash "$CHECKER" --prose-frames "$CASE" 2>&1)
  rc=$?
  got=$(printf '%s\n' "$out" | awk '
    /^(frame|tell|fairness|joiner) [^ ]+:[0-9]+: / {
      loc = $2; sub(/:$/, "", loc); print $1 " " loc; next
    }
    /^STYLE FAIL: no markdown page found to grade for the stance$/ {
      print "empty"; next
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

new_frames_case
expect_frames 0 '' 'a tree whose pages say what is true and stop'

new_frames_case
printf 'The number is a reading, not a guess.\n' >> "$CASE/docs/page.md"
expect_frames 1 'frame docs/page.md:4' 'a contrast frame on a page'

new_frames_case
printf 'The answer the run gives is the honest one.\n' >> "$CASE/docs/page.md"
expect_frames 1 'tell docs/page.md:4' 'a tell word on a page'

new_frames_case
printf 'Both readings are taken in the same run.\n' >> "$CASE/docs/page.md"
expect_frames 1 'fairness docs/page.md:4' 'a condition of fairness on a page'

new_frames_case
printf 'The key reaches the engine -- and the glyph is already drawn.\n' \
  >> "$CASE/docs/page.md"
expect_frames 1 'joiner docs/page.md:4' 'a dash joining two clauses on a page'

# The same four shapes on README.md, because the page a person reads first
# is in the population and a walk handed one directory would say nothing.
new_frames_case
printf 'The launch is a moment, not a segment.\n' >> "$CASE/README.md"
expect_frames 1 'frame README.md:4' 'a contrast frame on the README'

# A fenced block is a sample of a file or of a session: what it holds is
# quoted rather than written.
new_frames_case
{
  printf '```\n'
  printf 'The number is a reading, not a guess, and the honest one.\n'
  printf '```\n'
} >> "$CASE/docs/page.md"
expect_frames 0 '' 'a fenced sample holding every shape'

new_frames_case
printf '| id | note |\n| a | a reading, not a guess |\n' >> "$CASE/docs/page.md"
expect_frames 0 '' 'a table row, which is a cell of data'

new_frames_case
printf '> The number is a reading, not a guess.\n' >> "$CASE/docs/page.md"
expect_frames 0 '' 'a blockquote, which is quoted upstream text'

new_frames_case
printf 'A column named `fair` sits beside one named `honest` in the table.\n' \
  >> "$CASE/docs/page.md"
expect_frames 0 '' 'tell words inside backticked spans, which hold code'

new_frames_case
printf 'E5108: Error executing lua: not a function, instead of a table.\n' \
  >> "$CASE/docs/page.md"
expect_frames 0 '' 'an nvim message quoted as nvim writes it'

# The transport denial .claude/rules/bench.md requires of the
# speculated-echo paragraph, and nothing wider.
new_frames_case
printf 'That is the reading, not a network; the network case is the leg.\n' \
  >> "$CASE/docs/page.md"
expect_frames 0 '' 'the transport denial on a line that names the reading'

new_frames_case
printf 'The cache is warm, not a network away from the picker it serves.\n' \
  >> "$CASE/docs/page.md"
expect_frames 1 'frame docs/page.md:4' \
  'the same words on a line naming neither the reading nor a local one'

new_frames_case
printf 'The gate reports whether or not a page trips but the run goes on.\n' \
  >> "$CASE/docs/page.md"
expect_frames 0 '' 'the idiom, which is one word to a reader'

# A span the line leaves open is reported by the split-span rule; this walk
# takes the rest of the line with it rather than reading a half-quoted
# sample as prose.
new_frames_case
printf 'A page whose population is graded by the walk over `docs/`.\n' \
  >> "$CASE/docs/page.md"
expect_frames 0 '' 'a closed span on an ordinary line'

new_frames_case
rm -f "$CASE/README.md"
rm -rf "$CASE/docs"
expect_frames 1 'empty' \
  'a tree with no page to grade, which would report ok having graded nothing'
printf '\n%s cases, %s failures\n' "$n" "$failures"
[ "$failures" -eq 0 ]
