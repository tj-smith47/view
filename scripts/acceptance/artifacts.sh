#!/usr/bin/env bash
#
# The binaries an acceptance run measures, guaranteed to be the tree's own.
#
# Sourced, never executed. Every script here drives a compiled binary out of
# `target/`, and a path under `target/` is not evidence of anything on its
# own: it is whatever the last build that touched it left, including a build
# from source that has since been edited away. A run that trusts one is
# capable of reporting a defect that was fixed hours earlier, which is worse
# than a run that fails -- it sends a reviewer looking for a race in code
# that never ran.
#
# So the default path is BUILT here rather than checked for existence, on
# every run, which cargo answers in a fraction of a second when nothing has
# changed. A path the caller pinned is not this script's to build -- it may
# not even come from this tree -- but it is still refused when the tree has
# moved past it, because the same stale artifact produces the same false
# verdict either way.
#
# Callers must define `REPO_ROOT` before sourcing.

# A fractional-seconds clock every leg's wait loops time out against.
#
# BSD `date` (macOS, an established validation host for this repo) has no
# `%N` and answers `<epoch>.N`, which awk reads as the integer seconds --
# so every `elapsed` returns 0.00 and no wait loop ever expires. A sweep
# that hangs on a defect instead of failing on it is worse than one with
# whole-second resolution, which is what the fallback here gives.
now() {
    local t
    t=$(date +%s.%N)
    case "$t" in
    *N*) date +%s ;;
    *) printf '%s\n' "$t" ;;
    esac
}

# A directory for the pane dumps a failing run keeps, with the ones earlier
# failures kept reaped first.
#
# Nothing removes a kept dump afterwards -- deliberately, since it is the
# evidence -- so on a host whose TMPDIR is a small tmpfs a run of failures
# fills it and the next run has nowhere to write. A day is long enough for
# whoever the dump was kept for to have read it.
dump_dir() {
    local prefix="$1" parent="${TMPDIR:-/tmp}"
    find "$parent" -maxdepth 1 -name "$prefix-*" -type d -mtime +1 \
        -exec rm -rf {} + 2>/dev/null || true
    mktemp -d "$parent/$prefix-XXXXXX"
}

# Makes sure `path` is a binary this tree's source produced.
#
#   ensure_artifact "$VIEW_BIN" "$REPO_ROOT/target/release/view" \
#       cargo build --release -p view
#
# `default` is the path the caller uses when nothing was pinned; everything
# after it is the build command that produces it.
ensure_artifact() {
    local path="$1" default="$2"
    shift 2
    if [ "$path" = "$default" ]; then
        printf 'building %s\n' "${path#"$REPO_ROOT/"}" >&2
        (cd -- "$REPO_ROOT" && "$@" >&2) || {
            printf 'FAIL: %s could not be built (%s)\n' "${path#"$REPO_ROOT/"}" "$*" >&2
            return 1
        }
    else
        [ -x "$path" ] || {
            printf 'FAIL: no binary at %s (%s, or unset the override)\n' "$path" "$*" >&2
            return 1
        }
        local newer
        newer=$(find "$REPO_ROOT/crates" -name '*.rs' -newer "$path" -print -quit 2>/dev/null || true)
        if [ -n "$newer" ]; then
            printf 'FAIL: %s predates %s, so it is not the tree it would be measuring\n' \
                "$path" "${newer#"$REPO_ROOT/"}" >&2
            printf '      rebuild it (%s) or point at one that is current\n' "$*" >&2
            return 1
        fi
    fi
    [ -x "$path" ] || {
        printf 'FAIL: %s is not an executable file after the build that owns it\n' "$path" >&2
        return 1
    }
}
