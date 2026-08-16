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
set -euo pipefail

SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
REPO_ROOT=$(cd -- "$SCRIPT_DIR/../.." && pwd)

RTT_ACCEPTANCE_BIN=${RTT_ACCEPTANCE_BIN:-$REPO_ROOT/target/release/rtt-acceptance}
TAPS_VIEW_BIN=${TAPS_VIEW_BIN:-$REPO_ROOT/target/taps/release/view}
NVIM_BIN=${NVIM_BIN:-nvim}

[ -x "$RTT_ACCEPTANCE_BIN" ] || {
    printf 'FAIL: no rtt-acceptance binary at %s (cargo build --release -p view-harness --bin rtt-acceptance, or set RTT_ACCEPTANCE_BIN)\n' \
        "$RTT_ACCEPTANCE_BIN" >&2
    exit 1
}
[ -x "$TAPS_VIEW_BIN" ] || {
    printf 'FAIL: no bench-taps view binary at %s (cargo build --release -p view --features bench-taps --target-dir target/taps, or set TAPS_VIEW_BIN)\n' \
        "$TAPS_VIEW_BIN" >&2
    exit 1
}
command -v "$NVIM_BIN" >/dev/null 2>&1 || {
    printf 'FAIL: no nvim on PATH as %s (set NVIM_BIN)\n' "$NVIM_BIN" >&2
    exit 1
}

printf 'view acceptance: RTT injection (%s, %s)\n' \
    "${RTT_ACCEPTANCE_BIN#"$REPO_ROOT/"}" "$(nvim --version | head -1)"

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
