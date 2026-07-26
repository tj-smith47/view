#!/usr/bin/env bash
# Gate: no production .rs file exceeds the god-file ceiling of production CODE
# lines. Oversized files must split into cohesive modules, and any inline
# #[cfg(test)] block that stays must move to a sibling tests.rs / *_tests.rs:
# inline test blocks in large files inflate cargo-llvm-cov coverage (test lines
# count as covered production lines), while separate test files are excluded.
#
# Production lines = non-blank, non-comment CODE lines that are not test code.
# Test code does not count toward the ceiling at all: a pure test file is never
# a god file, and an inline test region is subtracted from its host file.
#
# Two modes, matching the retired check-loc.sh contract it supersedes:
#   audit-god-files.sh            tree-wide, authoritative (task loc / ci)
#   audit-god-files.sh FILE...    per-file fast path (the post-edit-rs.sh hook)
# The per-file path classifies a file by its own path and #![cfg(test)] only,
# skipping the cross-file `mod NAME;` resolution the tree-wide pass does; the
# tree-wide gate at commit time is the authority, this is only fast feedback.
#
# ── what counts as test code ───────────────────────────────────────────────
# Three sources, all derived rather than name-matched, so a test file called
# helpers.rs is classified from the declaration that gates it, not a filename:
#   1. Cargo test/bench targets under crates/*/tests/ and crates/*/benches/.
#   2. A file carrying the whole-file inner attribute #![cfg(test)].
#   3. A file reached from a #[cfg(test)] mod NAME; declaration, transitively.
#
# Inside a file that is not wholly test code, a test region is a test-only
# #[cfg(…)] attribute plus the item it gates. Test-only means the predicate
# cannot hold outside `cargo test`:
#   #[cfg(test)]                                → test-only
#   #[cfg(all(test, unix))]                     → test-only  (all ⇒ test)
#   #[cfg(any(windows, test))]                  → NOT: builds on windows
#   #[cfg(feature = "…")]                       → NOT: no bare test predicate
# A scan that only matches the literal #[cfg(test)] misses #[cfg(all(test, …))],
# which view uses to gate unix-only fixtures off the Windows build.
#
# ── why the gated item is measured, not truncated at the marker ────────────
# Truncating production at the first #[cfg(test)] is wrong for a module-dir
# mod.rs, where #[cfg(test)] mod tests; sits mid-file with a pub use re-export
# block below it. So the gated ITEM is measured and subtracted — a `mod tests;`
# declaration costs its statement, an inline `mod tests { … }` costs its whole
# brace body — and scanning resumes afterward.
set -euo pipefail

ROOT="${1:-}"
# A single existing directory arg selects tree-wide mode against that root; any
# other args are treated as files for the per-file fast path.
if [[ -n "$ROOT" && -d "$ROOT" && $# -eq 1 ]]; then
    cd "$ROOT"
    set --
else
    cd "$(git rev-parse --show-toplevel 2>/dev/null || pwd)"
fi

GOD_FILE_LIMIT=1000

# Pinned ceilings, never blank cheques: a file may shrink freely, but a commit
# that grows a pinned file fails, so each entry is a ratchet that only moves
# down and then out. EXEMPT — a size that is a deliberate design outcome.
# DEBT — a real violation predating this guard, pinned and printed until split.
# Format: "<path>:<pinned ceiling>:<why>".
GOD_FILE_EXEMPT=()
GOD_FILE_DEBT=()

# Per-file production CODE line count. Emits "<count>\t<path>" per input file.
# The gated test region's extent is found by brace depth over code with string
# and comment content elided first (strip_code), so every brace, semicolon and
# // the scanner sees is real code — a naive trailing-// strip cuts the line at
# the // inside "https://…" and swallows the rest of the file, and an
# indent-anchored close ends early on a } at column 0 inside a raw string.
count_prod_lines() {
    awk '
        function strip_code(l,   out, i, n, c, k, endm, m) {
            out = ""; i = 1; n = length(l)
            while (i <= n) {
                if (in_raw) {
                    endm = "\""
                    for (k = 0; k < raw_hashes; k++) endm = endm "#"
                    k = index(substr(l, i), endm)
                    if (k == 0) return out
                    i = i + k - 1 + length(endm); in_raw = 0; continue
                }
                if (in_str) {
                    while (i <= n) {
                        c = substr(l, i, 1)
                        if (c == "\\") { i += 2; continue }
                        if (c == "\"") { i++; in_str = 0; break }
                        i++
                    }
                    if (in_str) return out
                    continue
                }
                if (in_bcomment) {
                    k = index(substr(l, i), "*/")
                    if (k == 0) return out
                    i = i + k + 1; in_bcomment = 0; continue
                }
                c = substr(l, i, 1)
                if (c == "/" && substr(l, i + 1, 1) == "/") return out
                if (c == "/" && substr(l, i + 1, 1) == "*") { in_bcomment = 1; i += 2; continue }
                if (c == "r") {
                    m = 0
                    while (substr(l, i + 1 + m, 1) == "#") m++
                    if (substr(l, i + 1 + m, 1) == "\"") {
                        in_raw = 1; raw_hashes = m; i = i + m + 2; continue
                    }
                }
                if (c == "\"") { in_str = 1; i++; continue }
                if (c == "'"'"'") {
                    if (substr(l, i + 1, 1) == "\\") {
                        k = index(substr(l, i + 2), "'"'"'")
                        if (k > 0) { i = i + 2 + k; continue }
                    } else if (substr(l, i + 2, 1) == "'"'"'") { i += 3; continue }
                }
                out = out c
                i++
            }
            return out
        }

        function count_char(s, ch,   i, n, t) {
            t = 0; n = length(s)
            for (i = 1; i <= n; i++) if (substr(s, i, 1) == ch) t++
            return t
        }

        # `not(...)` anywhere makes the predicate PRODUCTION-only: a
        # `#[cfg(not(test))]` item ships in the real binary, so subtracting it
        # would hide unlimited production code behind one attribute.
        function is_test_only_cfg(l) {
            if (l !~ /^[[:space:]]*#\[cfg\(/) return 0
            if (l ~ /\(any\(/) return 0
            if (l ~ /not[[:space:]]*\(/) return 0
            return (l ~ /(^|[(,[:space:]])test([),]|$)/)
        }
        function flush() { if (cur != "") printf("%d\t%s\n", prod, cur) }

        FNR == 1 {
            flush()
            cur = FILENAME; prod = 0; state = "idle"
            depth = 0; opened = 0
            in_raw = 0; raw_hashes = 0; in_str = 0; in_bcomment = 0
        }

        { line = $0; code = strip_code(line) }

        state == "region" {
            depth += count_char(code, "{") - count_char(code, "}")
            if (index(code, "{") > 0) opened = 1
            if (opened) {
                if (depth <= 0) state = "idle"
            } else if (code ~ /;[[:space:]]*$/) {
                state = "idle"
            }
            next
        }

        {
            # tested against the ELIDED line: a `#[cfg(test)]` sitting inside
            # a block comment or a raw string is not an attribute, and
            # starting a region there swallows the production code after it
            if (is_test_only_cfg(code)) {
                state = "region"; depth = 0; opened = 0
                depth += count_char(code, "{") - count_char(code, "}")
                if (index(code, "{") > 0) opened = 1
                if (opened && depth <= 0) state = "idle"
                next
            }
            # Production count is CODE lines only: strip_code leaves nothing but
            # whitespace for a blank or comment-only line, so those never count.
            if (code ~ /[^[:space:]]/) prod++
        }

        END { flush() }
    ' "$@"
}

# ── per-file fast path ─────────────────────────────────────────────────────
if [[ $# -gt 0 ]]; then
    fail=0
    for f in "$@"; do
        if [[ ! -f "$f" ]]; then
            # a typo'd path used to exit 0, so a caller could silently check
            # nothing at all and read it as a pass
            echo "audit-god-files: not a file: $f" >&2
            fail=1
            continue
        fi
        # only the two structural exclusions the tree-wide pass also applies.
        # Filename skips (*_tests.rs and friends) used to live here and made
        # the hook pass a production module the authoritative gate fails.
        case "$f" in
            *crates/*/tests/* | *crates/*/benches/*) continue ;;
        esac
        grep -qE '^[[:space:]]*#!\[cfg\(test\)\]' -- "$f" && continue
        count="$(count_prod_lines "$f" | cut -f1)"
        if [[ "${count:-0}" -gt "$GOD_FILE_LIMIT" ]]; then
            echo "GOD FILE: $f has $count production code lines (ceiling $GOD_FILE_LIMIT)."
            echo "  Split it into cohesive modules; move any inline #[cfg(test)] mod tests"
            echo "  to a sibling tests.rs in the same commit."
            fail=1
        fi
    done
    exit $fail
fi

# ── tree-wide authoritative audit ──────────────────────────────────────────
# --others --exclude-standard alongside the tracked set: a god file written but
# not yet git-added is exactly what this guard exists to catch. -f drops index
# entries with no file on disk (a split deletes foo.rs in the worktree before
# that deletion is staged), which would otherwise fail every downstream reader.
mapfile -t ALL_RS < <(
    git ls-files --cached --others --exclude-standard -- 'crates/**/*.rs' 'crates/*.rs' 2>/dev/null |
        sort -u |
        while IFS= read -r _f; do [[ -f "$_f" ]] && printf '%s\n' "$_f"; done
)
if [[ ${#ALL_RS[@]} -eq 0 ]]; then
    echo "audit-god-files: no crates/**/*.rs tracked; nothing to scan." >&2
    exit 1
fi

# Resolve `mod NAME;` declared in DECL_FILE to the file that provides it, into
# the global RESOLVED (empty when the module has no file). rustc looks in the
# declaring module's directory: lib.rs/main.rs/mod.rs declare into their OWN
# directory, src/foo.rs into the sibling src/foo/.
resolve_mod_file() {
    local decl_file="$1" name="$2" dir base cand_dir c
    dir="${decl_file%/*}"
    base="${decl_file##*/}"
    base="${base%.rs}"
    case "$base" in
        mod | lib | main) cand_dir="$dir" ;;
        *) cand_dir="$dir/$base" ;;
    esac
    RESOLVED=""
    for c in "$cand_dir/$name.rs" "$cand_dir/$name/mod.rs"; do
        if [[ -f "$c" ]]; then
            RESOLVED="$c"
            return 0
        fi
    done
}

# Every `mod NAME;` in the tree in one awk pass as "<file>\t<gated>\t<name>",
# gated=1 when a test-only cfg attribute precedes (or shares the line with) it.
collect_mod_decls() {
    awk '
        # anchored, matching the counter above: unanchored, a doc comment that
        # merely MENTIONS `#[cfg(test)]` above a `mod NAME;` marked that
        # module test-only and excluded its whole file from the census.
        # `not(...)` is production-only, as in the counter.
        function is_test_only_cfg(l) {
            if (l !~ /^[[:space:]]*#!?\[cfg\(/) return 0
            if (l ~ /\(any\(/) return 0
            if (l ~ /not[[:space:]]*\(/) return 0
            return (l ~ /(^|[(,[:space:]])test([),]|$)/)
        }
        FNR == 1 { pend = 0 }
        { line = $0 }
        is_test_only_cfg(line) { pend = 1 }
        match(line, /^[[:space:]]*(pub[[:space:]]*(\([^)]*\)[[:space:]]*)?)?mod[[:space:]]+[A-Za-z_][A-Za-z_0-9]*[[:space:]]*;/) {
            name = substr(line, RSTART, RLENGTH)
            sub(/[[:space:]]*;[[:space:]]*$/, "", name)
            sub(/^.*[[:space:]]/, "", name)
            printf("%s\t%d\t%s\n", FILENAME, pend, name)
            pend = 0
            next
        }
        /^[[:space:]]*(#|\/\/)/ { next }
        /^[[:space:]]*$/        { next }
        { pend = 0 }
    ' "$@"
}

declare -A GATED_DECLS=()
declare -A ALL_DECLS=()
declare -A IS_TEST_FILE=()
WORKLIST=()

while IFS=$'\t' read -r file gated name; do
    [[ -n "$file" ]] || continue
    ALL_DECLS["$file"]+="$name "
    [[ "$gated" == "1" ]] && GATED_DECLS["$file"]+="$name "
done < <(collect_mod_decls "${ALL_RS[@]}")

declare -A INNER_CFG_TEST=()
while IFS= read -r f; do
    [[ -n "$f" ]] && INNER_CFG_TEST["$f"]=1
done < <(grep -rlE '^[[:space:]]*#!\[cfg\(test\)\]' -- "${ALL_RS[@]}" 2>/dev/null || true)

for f in "${ALL_RS[@]}"; do
    case "$f" in
        crates/*/tests/* | crates/*/benches/*)
            IS_TEST_FILE["$f"]=1
            WORKLIST+=("$f")
            continue
            ;;
    esac
    if [[ -n "${INNER_CFG_TEST[$f]:-}" ]]; then
        IS_TEST_FILE["$f"]=1
        WORKLIST+=("$f")
        continue
    fi
    for name in ${GATED_DECLS[$f]:-}; do
        resolve_mod_file "$f" "$name"
        if [[ -n "$RESOLVED" && -z "${IS_TEST_FILE[$RESOLVED]:-}" ]]; then
            IS_TEST_FILE["$RESOLVED"]=1
            WORKLIST+=("$RESOLVED")
        fi
    done
done

# Transitive closure: a submodule of a test-only module is itself test-only.
wl_head=0
while [[ $wl_head -lt ${#WORKLIST[@]} ]]; do
    f="${WORKLIST[$wl_head]}"
    wl_head=$((wl_head + 1))
    for name in ${ALL_DECLS[$f]:-}; do
        resolve_mod_file "$f" "$name"
        if [[ -n "$RESOLVED" && -z "${IS_TEST_FILE[$RESOLVED]:-}" ]]; then
            IS_TEST_FILE["$RESOLVED"]=1
            WORKLIST+=("$RESOLVED")
        fi
    done
done

PROD_FILES=()
for f in "${ALL_RS[@]}"; do
    [[ -n "${IS_TEST_FILE[$f]:-}" ]] || PROD_FILES+=("$f")
done

declare -A PIN_CEILING=()
declare -A PIN_REASON=()
declare -A PIN_LIST=()
for listname in GOD_FILE_EXEMPT GOD_FILE_DEBT; do
    declare -n _list="$listname"
    for e in ${_list[@]+"${_list[@]}"}; do
        _path="${e%%:*}"
        _rest="${e#*:}"
        PIN_CEILING["$_path"]="${_rest%%:*}"
        PIN_REASON["$_path"]="${_rest#*:}"
        PIN_LIST["$_path"]="$listname"
    done
    unset -n _list
done

violations=""
exempt_seen=""
debt_seen=""
largest_count=0
largest_path=""
declare -A MEASURED=()

while IFS=$'\t' read -r count path; do
    [[ -n "$path" ]] || continue
    MEASURED["$path"]="$count"

    matched=""
    ceiling="${PIN_CEILING[$path]:-}"
    if [[ -n "$ceiling" ]]; then
        reason="${PIN_REASON[$path]}"
        listname="${PIN_LIST[$path]}"
        matched=1
        if [[ "$listname" == GOD_FILE_EXEMPT ]]; then
            exempt_seen+="  $path: $count prod lines (pinned $ceiling) — $reason"$'\n'
            kind="exempt"
        else
            debt_seen+="  $path: $count prod lines (pinned $ceiling) — $reason"$'\n'
            kind="recorded debt"
        fi
        if [[ "$count" -gt "$ceiling" ]]; then
            violations+="$path: $count production lines — $kind at a PINNED ceiling of $ceiling and it GREW; a pinned entry may only fall, so split the file instead of raising the pin"$'\n'
        fi
    fi
    [[ -z "$matched" ]] || continue

    if [[ "$count" -gt "$largest_count" ]]; then
        largest_count="$count"
        largest_path="$path"
    fi
    if [[ "$count" -gt "$GOD_FILE_LIMIT" ]]; then
        violations+="$path: $count production lines (ceiling $GOD_FILE_LIMIT)"$'\n'
    fi
done < <(count_prod_lines "${PROD_FILES[@]}" | sort -rn)

# Both registers are ratchets, not parking spots: a pin may only fall, and a
# stale one has to leave. Without this a pinned file could be split, shrink
# below its pin, or be deleted outright and the entry would sit in the list
# forever, quietly exempting a path that no longer needs exempting.
for path in "${!PIN_CEILING[@]}"; do
    ceiling="${PIN_CEILING[$path]}"
    listname="${PIN_LIST[$path]}"
    if [[ -z "${MEASURED[$path]:-}" ]]; then
        violations+="$path: pinned at $ceiling in $listname, but no such production file was scanned — the pin is stale (split, renamed, deleted, or now classified as test code); remove the entry"$'\n'
        continue
    fi
    count="${MEASURED[$path]}"
    if [[ "$count" -le "$GOD_FILE_LIMIT" ]]; then
        violations+="$path: $count production lines is within the ${GOD_FILE_LIMIT} ceiling, so its $listname pin of $ceiling is stale; remove the entry"$'\n'
    elif [[ "$count" -lt "$ceiling" ]]; then
        violations+="$path: $count production lines sits below its $listname pin of $ceiling; a pin may only fall, so lower it to $count and lock the gain in"$'\n'
    fi
done

if [[ -n "$violations" ]]; then
    echo "GOD FILE — production code over the ${GOD_FILE_LIMIT}-line ceiling."
    echo
    printf '%s' "$violations"
    echo
    echo "Production lines are non-blank, non-comment code minus test code: files"
    echo "under crates/*/tests|benches, files carrying #![cfg(test)], files reached"
    echo "through a #[cfg(test)] mod NAME; declaration, and inline test-only cfg"
    echo "regions are all excluded. A pure test file is never a god file."
    echo
    echo "Fix: split the production code into cohesive modules. If the file still"
    echo "holds an inline #[cfg(test)] mod tests { … }, move it to a sibling"
    echo "tests.rs in the same commit — that keeps the split reviewable and leaves"
    echo "one concern per file."
    echo
    echo "Recording the file instead of splitting it is a last resort and needs the"
    echo "user's agreement: add it to GOD_FILE_DEBT in this script with a pinned"
    echo "ceiling and a written reason. GOD_FILE_EXEMPT is only for a size that is a"
    echo "deliberate design outcome. Neither pin may ever be raised."
    exit 1
fi

if [[ -n "$largest_path" ]]; then
    largest_note="largest is $largest_path at $largest_count production lines"
else
    largest_note="no unpinned production file was measured"
fi
echo "audit-god-files: ${#PROD_FILES[@]} production files scanned (${#IS_TEST_FILE[@]} test files excluded); $largest_note, ceiling $GOD_FILE_LIMIT."
if [[ -n "$exempt_seen" ]]; then
    echo "audit-god-files: ${#GOD_FILE_EXEMPT[@]} named exemption(s) — deliberate, not going to shrink:"
    printf '%s' "$exempt_seen"
fi
if [[ -n "$debt_seen" ]]; then
    echo "audit-god-files: ${#GOD_FILE_DEBT[@]} recorded debt file(s) — OVER the ceiling and still owed a split:"
    printf '%s' "$debt_seen"
fi
