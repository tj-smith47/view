#!/usr/bin/env bash
#
# The A/B build half of a paired measurement: one binary per revision, each
# from its own exported tree into its own target directory, and never two
# copies of the same binary reported as a difference.
#
#   bash scripts/ab-build.sh <before-ref> <after-ref> [-- extra cargo args]
#   VIEW_BIN=~/.cache/view-ab/before/target/release/view task bench -- ...
#
# Two target directories, not one. A pair built from two source trees at
# different paths through a single target dir came out byte-identical once:
# cargo's dep-info still named the first tree's files, all of them fresh, so
# the second build compiled nothing and the pair measured the same binary
# twice -- a null result that reads exactly like a change with no effect.
#
# Hence also the `cmp` at the end, which is the whole reason to run this
# rather than two builds by hand: the refusal is what a paired measurement
# cannot be trusted without.
set -euo pipefail

REPO_ROOT=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)
OUT=${AB_OUT:-$HOME/.cache/view-ab}
PACKAGE=${AB_PACKAGE:-view}
BINARY=${AB_BINARY:-view}

if [ "$#" -lt 2 ]; then
    printf 'usage: bash scripts/ab-build.sh <before-ref> <after-ref> [-- extra cargo args]\n' >&2
    exit 2
fi
before_ref=$1
after_ref=$2
shift 2
[ "${1:-}" = -- ] && shift

for ref in "$before_ref" "$after_ref"; do
    git -C "$REPO_ROOT" rev-parse --verify --quiet "$ref^{tree}" >/dev/null || {
        printf 'FAIL: %s names nothing in this repo\n' "$ref" >&2
        exit 1
    }
done

build() {
    local side="$1" ref="$2" tree target
    tree=$OUT/$side/src
    target=$OUT/$side/target
    shift 2
    printf '%s: %s (%s)\n' "$side" "$ref" "$(git -C "$REPO_ROOT" rev-parse --short "$ref")" >&2
    rm -rf "${OUT:?}/$side"
    mkdir -p "$tree" "$target"
    git -C "$REPO_ROOT" archive --format=tar "$ref" | tar -x -C "$tree"
    # `git archive` stamps every file with the revision's own date, which is
    # older than a warm target directory: cargo compares mtimes and would
    # decide there is nothing left to build
    find "$tree" -name '*.rs' -exec touch {} +
    (
        cd -- "$tree"
        CARGO_TARGET_DIR=$target cargo build --release -p "$PACKAGE" "$@" >&2
    )
}

build before "$before_ref" "$@"
build after "$after_ref" "$@"

a=$OUT/before/target/release/$BINARY
b=$OUT/after/target/release/$BINARY
for path in "$a" "$b"; do
    [ -x "$path" ] || {
        printf 'FAIL: no binary at %s after the build that owns it\n' "$path" >&2
        exit 1
    }
done
if cmp -s "$a" "$b"; then
    printf 'FAIL: %s and %s are byte-identical, so the pair would measure one binary twice\n' \
        "$a" "$b" >&2
    printf '      (one shared target dir does this; so does a pair of refs with no code between them)\n' >&2
    exit 1
fi

printf 'before: %s\n' "$a"
printf 'after:  %s\n' "$b"
