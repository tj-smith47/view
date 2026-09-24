#!/usr/bin/env bash
# Case matrix for scripts/audit-god-files.sh's single-file/tree-wide
# agreement: the per-file fast path (run from the Edit hook, one file, no
# git listing) has to answer the same test-only question the tree-wide
# `--counts` pass answers (a full mod-graph walk over the tracked tree), or
# a file wholly reached through a parent's `#[cfg(test)] mod tests;` reads
# as a god file on save and never in the gate that actually blocks a commit.
# Each case builds a throwaway git repo under the scratch root so tree-wide
# mode's `git ls-files` has something to list, and runs the checker in both
# modes from inside it.
#
#   bash scripts/audit-god-files-cases.sh
#   bash scripts/audit-god-files-cases.sh --checker /path/to/copy
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
if [ -z "$CHECKER" ]; then
    CHECKER="$(cd "$(dirname "$0")" && pwd)/audit-god-files.sh"
fi
if [ ! -f "$CHECKER" ]; then
    printf 'checker not found: %s\n' "$CHECKER" >&2
    exit 2
fi

printf 'checker under test: %s\n' "$CHECKER"

# shellcheck source=lib/scratch.sh
. "$(dirname "$0")/lib/scratch.sh"
WORK=$(mktemp -d "$(scratch_root)/audit-god-files-cases-XXXXXX")
trap 'rm -rf "$WORK"' EXIT

n=0
failures=0

# A throwaway git repo, since tree-wide mode reads `git ls-files` and a
# directory with nothing committed lists nothing to measure.
new_case() {
    n=$((n + 1))
    CASE="$WORK/case$n"
    mkdir -p "$CASE"
    (cd "$CASE" && git init -q && git config user.email t@t.t && git config user.name t)
}

commit_case() {
    (cd "$CASE" && git add -A && git commit -q -m case)
}

# N lines of trivial production code, so a body's own line count is exact
# and every line is real code to the elider (no comment, no blank).
prod_lines() {
    local count="$1" i
    for ((i = 0; i < count; i++)); do
        printf 'let _x%d = %d;\n' "$i" "$i"
    done
}

expect() {
    local want_rc="$1" want_grep="$2" desc="$3" out rc
    shift 3
    out=$(cd "$CASE" && bash "$CHECKER" "$@" 2>&1)
    rc=$?
    local want_grepped="present"
    [ -z "$want_grep" ] && want_grepped="absent"
    local grepped="absent"
    if [ -n "$want_grep" ]; then
        case "$out" in
            *"$want_grep"*) grepped="present" ;;
        esac
    fi
    if [ "$rc" = "$want_rc" ] && [ "$grepped" = "$want_grepped" ]; then
        printf 'ok %s - %s\n' "$n" "$desc"
        return
    fi
    failures=$((failures + 1))
    printf 'not ok %s - %s\n  want rc=%s grep(%s)=%s\n  got  rc=%s grep=%s\n' \
        "$n" "$desc" "$want_rc" "$want_grep" "$want_grepped" "$rc" "$grepped"
    printf '%s\n' "$out" | sed 's/^/  | /'
}

# A #[cfg(test)] mod tests; child, over the ceiling on its own, must pass
# single-file mode whatever its length: it is wholly test code, reached only
# through its parent's declaration, and the fast path used to miss that
# because it never read any file but the one it was handed.
new_case
mkdir -p "$CASE/crates/x/src/paint"
{
    echo 'pub(crate) mod frames;'
    echo '#[cfg(test)]'
    echo 'mod tests;'
} > "$CASE/crates/x/src/paint/panes.rs"
echo 'pub fn frames() {}' > "$CASE/crates/x/src/paint/frames.rs"
mkdir -p "$CASE/crates/x/src/paint/panes"
prod_lines 1200 > "$CASE/crates/x/src/paint/panes/tests.rs"
commit_case
expect 0 '' 'a #[cfg(test)] mod tests; child over the ceiling passes single-file mode' \
    "$CASE/crates/x/src/paint/panes/tests.rs"

# A plain production file over the ceiling still fails single-file mode --
# the fix above must not turn the fast path into a rubber stamp.
new_case
mkdir -p "$CASE/crates/x/src"
prod_lines 1200 > "$CASE/crates/x/src/big.rs"
commit_case
expect 1 'GOD FILE' 'a production file over the ceiling still fails single-file mode' \
    "$CASE/crates/x/src/big.rs"

# An inline #[cfg(test)] mod tests { ... } block is counted up to its own
# boundary, and only up to it, in both modes: the host file's production
# count stays under the ceiling although its total line count (production
# plus the inline test body) is well past it.
new_case
mkdir -p "$CASE/crates/x/src"
{
    prod_lines 950
    echo '#[cfg(test)]'
    echo 'mod tests {'
    prod_lines 300
    echo '}'
} > "$CASE/crates/x/src/inline.rs"
commit_case
expect 0 '' 'an inline #[cfg(test)] mod tests block is excluded up to its own boundary (single-file)' \
    "$CASE/crates/x/src/inline.rs"

counts_out=$(cd "$CASE" && bash "$CHECKER" --counts . 2>&1)
n=$((n + 1))
case "$counts_out" in
    *$'\t'"crates/x/src/inline.rs")
        line=$(printf '%s\n' "$counts_out" | grep -F "crates/x/src/inline.rs")
        prod_count="${line%%$'\t'*}"
        if [ "$prod_count" = "950" ]; then
            printf 'ok %s - tree-wide --counts reports the same 950-line boundary for the inline block\n' "$n"
        else
            failures=$((failures + 1))
            printf 'not ok %s - tree-wide --counts reports the same 950-line boundary for the inline block\n  want 950\n  got  %s\n' \
                "$n" "$prod_count"
        fi
        ;;
    *)
        failures=$((failures + 1))
        printf 'not ok %s - tree-wide --counts reports the same 950-line boundary for the inline block\n  crates/x/src/inline.rs missing from --counts output\n' "$n"
        printf '%s\n' "$counts_out" | sed 's/^/  | /'
        ;;
esac

[ "$failures" -eq 0 ] || {
    printf '%d/%d cases failed\n' "$failures" "$n"
    exit 1
}
printf 'audit-god-files-cases.sh: %d cases ok\n' "$n"
