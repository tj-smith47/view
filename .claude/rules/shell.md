---
paths: ["scripts/**", "Taskfile.yml"]
---
# Shell conventions

## Every script under `scripts/` is written to bash 3.2

`Taskfile.yml` runs its scripts as `bash scripts/…`, so whichever bash is
first on `PATH` decides whether a gate runs at all. macOS ships
`/bin/bash` 3.2.57, and a contributor there gets a gate that dies mid-run
with a message reading as a script bug — CI's macos leg only passes because
the runner image puts a newer bash ahead of it. So the whole population is
written to 3.2, not just the scripts whose header says so.

Three shapes break it, and only the first is a construct you can name:

| shape | 3.2 says | write instead |
|---|---|---|
| `declare -A m=([k]=v)` | `k: unbound variable` under `set -u` | a `case`, or newline-joined strings matched with `grep -Fqx` |
| `case "$x" in *.*) … ;; esac` inside `$( )` or `<( )` | `syntax error near unexpected token` | `case "$x" in (*.*) … ;; esac` — the leading paren keeps the count |
| an apostrophe in a comment inside `$( )` or `<( )` | ``bad substitution: no closing `)' `` | reword the comment; 3.2 reads the quote, not the `#` |

The same ban covers `mapfile`, `readarray`, `[[ -v x ]]`, `${x,,}`,
`${x^^}`, `\|&`, `&>>` and `;;&`.

Two cases in `scripts/check-budget-drift-cases.sh` enforce it over every
`scripts/**/*.sh` in the tree, so a script is graded without anyone
remembering to add it here: one greps the construct list, and one parses
each script under `/bin/bash` when that is a pre-4 bash — which is the
only leg that sees the two paren-counting shapes, and it runs on the host
the contract is about. The population is the directory rather than the
scripts `Taskfile.yml` names, because the release path runs
`scripts/package-bundle.sh` from a workflow and `scripts/mbp-build-leg.sh`
runs on the macOS host the contract is about — five scripts no task names,
and a broken one there fails where nobody is watching for it.

## A pipeline stage names the file it reads

`grep`/`awk`/`sed` with no file argument reads stdin, so a stage whose file
list came out empty does not fail — it sits there having graded nothing. A
list built by a `grep` over another file is checked for emptiness before it
is passed on, and where two legs read the same list the check sits above
both: a leg that reports `ok` having graded nothing disagrees with its own
sibling about what an empty list means.
