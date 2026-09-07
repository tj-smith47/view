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

Four shapes break it, and only the first and the last are constructs you
can name:

| shape | 3.2 says | write instead |
|---|---|---|
| `declare -A m=([k]=v)` | `k: unbound variable` under `set -u` | a `case`, or newline-joined strings matched with `grep -Fqx` |
| `case "$x" in *.*) … ;; esac` inside `$( )` or `<( )` | `syntax error near unexpected token` | `case "$x" in (*.*) … ;; esac` — the leading paren keeps the count |
| an apostrophe in a comment inside `$( )` or `<( )` | ``bad substitution: no closing `)' `` | reword the comment; 3.2 reads the quote, not the `#` |
| `${x//a/b}` on anything longer than a word | nothing — it rescans the string per match and runs unbounded | `sed`/`tr` for a rewrite; `[[ $x == *[![:space:]]* ]]` (or its negation) for an emptiness test |

The last row's class is the every-match substitution and nothing wider.
`${x//a/b}`, `${x//a}` and `${!x//a/b}` are in it — the indirect spelling
rescans whatever the name holds, so it costs what the direct one costs, and
the construct grep matches both. `${x/#a/b}`, `${x/%a/b}` and `${x/a/b}`
are out: each rebuilds the string once, which is a sibling in spelling and
not in cost, and a rule with no ground to ban them does not. Whichever
spelling a rewrite uses, a rewrite of a path-sized word is not what the row
is about: the cost the row names is a rescan of something long, and moving
a word-sized encode into a process instead cost the `loc` gate four seconds
against its one and a half.

The last one is the only shape that fails as a hang rather than a message.
The drift check's five emptiness tests were written as "delete every blank
and see what is left" and the largest of them spun in bash itself, state
R with no child, on a 12 kB variable the shipped baselines build — still
running at 283 s under an alarm, where the glob test answered in 0 s.

The same ban covers `mapfile`, `readarray`, `[[ -v x ]]`, `${x,,}`,
`${x^^}`, `\|&`, `&>>` and `;;&`.

Three legs in `scripts/check-budget-drift-cases.sh` enforce it over every
file under `scripts/` whose shebang names bash or `sh` -- the remote-test
fixtures carry no suffix -- so a script is graded without anyone
remembering to add it here: one greps the construct list, one parses each
script under `/bin/bash` when that is a pre-4 bash — which is the only leg
that sees the two paren-counting shapes, and it runs on the host the
contract is about — and one runs the drift check over the shipped tree,
read-only, inside `perl -e 'alarm 120; exec @ARGV'` and under that same
stock `/bin/bash`, because a cost this size is invisible to both of the
reading legs and to every case tree, whose planted files are orders of
magnitude smaller than the baselines that ship. The timed leg runs whatever
that interpreter turns out to be: an overrun is a cost regression under any
bash, and the pattern substitution is the first cause to check rather than
the only one.

The construct list is itself graded, by three planted spellings: the direct
and the indirect substitution, each of which it must refuse, and an anchored
one, which it must let through.

The population is the directory rather than the
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
