#!/usr/bin/env bash
#
# RTT-injection acceptance: the echo_speculated row, driven through the
# committed stub-ssh double (scripts/test-fixtures/fake-ssh) plus the
# delay-relay fixture (scripts/test-fixtures/delay-relay) at four injected
# SSH round-trip tiers, asserting every tier's speculated_ratio_p50 stays
# under budgets.toml's bound for that row.
#
# The four tiers, the delay-relay design, and the arithmetic behind the
# printed "unspeculated equivalent" column are all owned by
# crates/view-harness/src/bin/rtt_acceptance.rs and
# crates/view-bench/src/scenarios/echo_speculated_rtt.rs; this script is a
# thin, env-var-overridable wrapper the same shape as supervision.sh's,
# not a second copy of that logic.
#
# Class-scoped: the row it asserts against is armed on controlled-linux, so
# anywhere else this leg announces a skip and exits 0 rather than inventing
# a bar an uncontrolled host cannot honestly measure against.
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)
# shellcheck source=scripts/acceptance/artifacts.sh
. "$SCRIPT_DIR/artifacts.sh"

# the bound this leg asserts against (budgets.toml's echo_speculated /
# speculated_ratio_p50 row) is armed on controlled-linux alone, so there is
# nothing for it to measure against on any other host; past this the
# resolved class it leaves in CLASS is the one the binary is handed, or a
# controlled host would hit the same missing-row refusal an uncontrolled
# one does
skip_unless_class remote-rtt controlled-linux

RTT_ACCEPTANCE_BIN=${RTT_ACCEPTANCE_BIN:-$TARGET_ROOT/release/rtt-acceptance}
TAPS_VIEW_BIN=${TAPS_VIEW_BIN:-$TARGET_ROOT/taps/release/view}
NVIM_BIN=${NVIM_BIN:-nvim}

ensure_artifact "$RTT_ACCEPTANCE_BIN" "$TARGET_ROOT/release/rtt-acceptance" \
    cargo build --release -p view-harness --bin rtt-acceptance || exit 1
ensure_artifact "$TAPS_VIEW_BIN" "$TARGET_ROOT/taps/release/view" \
    cargo build --release -p view --features bench-taps --target-dir "$TARGET_ROOT/taps" || exit 1
command -v "$NVIM_BIN" >/dev/null 2>&1 || {
    printf 'FAIL: no nvim on PATH as %s (set NVIM_BIN)\n' "$NVIM_BIN" >&2
    exit 1
}

printf 'view acceptance: RTT injection (%s, %s)\n' \
    "${RTT_ACCEPTANCE_BIN#"$REPO_ROOT/"}" "$(nvim --version | head -1)"

# the resolved class is supplied only when the caller named none: clap
# refuses `--class` twice, so passing it unconditionally would turn an
# explicit one into a usage error. Matched argument by argument rather than
# over the joined list, which cannot tell a flag from a value that contains
# one.
named_class=
for arg in "$@"; do
    case $arg in
    --class | --class=*) named_class=1 ;;
    esac
done
[ -n "$named_class" ] || set -- --class "$CLASS" "$@"

# the exit code is echoed as this script's own final log line, on every
# path (pass, budget breach, refusal), matching `task bench`'s wrapper
# convention so a tee'd gate log is self-describing without a reader
# having to interpret rtt-acceptance's own verdict text; errexit is off
# for the call itself so a non-zero exit reaches `code` instead of
# aborting the script before the line below can echo it
set +e
"$RTT_ACCEPTANCE_BIN" --taps-view-bin "$TAPS_VIEW_BIN" --nvim-bin "$NVIM_BIN" "$@"
code=$?
set -e
echo "remote-rtt wrapper exit: $code"
exit "$code"
