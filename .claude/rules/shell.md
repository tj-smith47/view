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

The second row's shape has a spelling a line-at-a-time scan can see, and it
is banned on its own: a `case` whose word runs onto the next line. Written
that way the pattern below it closes on a paren the substitution counts as
its own, which took the whole style case matrix out of the 3.2 leg. Every
`case` in this population puts its `in` on the header line, so a `case` with
no `in` beside it is the tell, and the construct scan refuses it.

Four legs in `scripts/check-budget-drift-cases.sh` enforce it over every
file under `scripts/` whose shebang names bash or `sh` -- the remote-test
fixtures carry no suffix -- so a script is graded without anyone
remembering to add it here: one greps the construct list, one greps the
split `case` header, one parses each
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
one, which it must let through. The split `case` scan is graded the same
way, by a planted header whose word runs onto the next line.

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

## A `--prod-lines` pin counts a spelling written in a trailing comment

The three pins in `scripts/check-style.sh` that ask
`scripts/audit-god-files.sh --prod-lines` which lines are production code
(`check_tied_spawns`, `check_geometry_sites`, the condition-notice
ownership check) read the raw line the scanner emits. A whole-line comment
never reaches them -- the classifier drops it -- but a spelling written
after a `//` on a line of code does, and it counts as a site.

That is the trade, and it is deliberate. The scanner's only eliding
machinery removes string *contents*, and two of the geometry spellings are
string literals (`"nvim_ui_attach"`, `"nvim_ui_try_resize"`): an
elide-based emit would blind the walk to the wire names in the one file
that sends them. Cutting comments while keeping strings means a second awk
pass over every consumer, against a defect whose failure mode is a loud
gate naming the file. So the walks over-count rather than narrow, and the
answer to a red row whose code holds no geometry is to reword the comment
or to move the row.

Pinned by a case at the end of `scripts/check-style-cases.sh` that plants a
trailing comment naming a spelling and requires the count to rise, so the
next attempt to "fix" the over-count into a comment-stripping read is a red
case rather than a silent narrowing.

## A `mktemp` is paired with an EXIT trap in the same script

A straight-line `rm` covers the ordinary path and nothing else. The gates
here scan a whole tree for seconds at a time, and a Ctrl-C or a `set -e`
abort inside that window leaves the file under `${TMPDIR:-/tmp}` for good --
which is exactly what `check-style.sh`'s `read_prod_lines` did while it was
the one temp file in `scripts/` without a trap.

The trap is what removes the file; a straight-line `rm` beside it is fine
and stays for the ordinary path. A trap naming a variable needs that
variable to still exist when it fires: a `local` is gone by then and `set -u`
aborts the exit path on it, so the name the trap reads is a script global.

`check_temp_traps` in `scripts/check-style.sh` walks the shebang population
below for a `mktemp`, and the pairing it requires is an armed `EXIT` trap
whose handler names one of the variables a `mktemp` path went into --
resolved through the function the handler names, since half of these scripts
write the removal in a `cleanup()`. Two spellings pass a walk that only asks
for the word `trap`: `trap - EXIT`, which clears the handler rather than
arming one, and a trap that reaps a child and removes nothing. Both are
cased, red, at the end of `scripts/check-style-cases.sh`, beside the
function-body pairing that has to stay green. Keyed on the script rather
than on the statement, because the removal legitimately sits far from the
`mktemp`. The name a handler may pair on is the variable the `mktemp`
assigned or a list that variable is appended to (`ROOTS+=("$ROOT")`,
`list="$list $ROOT"`), and it is matched case-sensitively: a removal written
over an array of roots reads `$root` where the `mktemp` named `ROOT`, and
the append is the relationship that actually holds between the two. Pairing
by lowercased name instead reads `TMP=/var/cache/keepme` as the removal for
`tmp=$(mktemp)`, which passes a leak in silence -- that pair is cased, red.
An assignment that is not an append is not a holder: `other=$ROOT/sub` names
a path inside the temp root and removing it removes nothing of the root. A
handler body ends at a brace on the function header's own indentation, never
at the first indented one, because a `{ ...; } >&2` group inside a `cleanup`
otherwise ends the body early and the removal below it is never read. The
population
and not `scripts/*.sh`: that glob reaches neither `scripts/acceptance/` nor a
file with no suffix, and the first script it missed was making a temp
directory a Ctrl-C stranded.

## A path a walk is guarded on is required by name

`scripts/check-style.sh` guards each walk on the directory or page it reads,
so a walk is never handed a root that is not there. A guard with no `else` is
fail-open: the run passes having graded nothing, and reports on rules it
never reached. The run therefore names every guarded directory in one
`for required in ...` list and every guarded file in `for required_file in
...` beside it, and fails closed on each; a pin at the end of
`scripts/check-style-cases.sh` walks the `[ -d ]` and `[ -f ]` guards in the
checker under test and reddens on the first one missing from either list --
so the next walk added behind an `if [ -d x ]` or an `if [ -f x.md ]` trips
there rather than shipping silent.

The same shape in one line, `[ -d docs ] && targets="$targets docs"`, is safe
where the population writes it and unsafe in one position. Mid-body under
`set -e` it does what it reads as: the failing command is the test, which
precedes the final `&&`, so errexit is suppressed and the run continues with
`targets` unset. As the last command of a function or of the script it is a
bug -- the list's status is then the function's, and a missing directory
returns 1 to a caller reading that as a failure. Nothing in `scripts/` is in
that position. Write it `if [ -d docs ]; then ... fi` anyway, because which
of the two a reader is looking at depends on what follows the line rather
than on the line.

## A symbolic link a script makes is written `ln -sn`

`ln -s` whose destination already exists as a symlink to a directory does
not fail. It follows the link and creates the new one *inside* the target,
which is how a checker-copy helper in `scripts/check-style-cases.sh` --
whose `$dir/lib` points at the graded tree's own `scripts/lib/` -- wrote
`scripts/lib/lib` into the tree the gates read when two copies shared one
case number. The next population read then could not open it and all three
gates over that population reddened, on a tree nobody had edited. The
sibling `ln` beside it, pointing at a file, failed loudly in the same
situation; this one succeeded silently. `-n` is in both BSD and GNU `ln`
and refuses rather than writing through, and it is a no-op where the
destination cannot pre-exist, so the whole population uses it rather than
the sites where someone spotted the hazard. Pinned by a case at the end of
`scripts/check-style-cases.sh` that walks the population for the flagless
spelling at the start of a command.

## A comment wraps at 80 characters

`scripts/check-style.sh` grades every comment line under `scripts/` at the
width its markdown pages are held to, and a contributor meets the rule for
the first time when it reddens a line. What the message says has to be what
an editor shows, so the walks count characters and not bytes: a rule drawn
in box characters is 77 characters and 105 bytes, and a gate measuring bytes
reddens it while passing a wider line whose long token it subtracts whole.

Counted without a UTF-8 awk, because the same verdict has to come back from
gawk, mawk and BSD awk: under `LC_ALL=C` a character is a lead byte plus its
continuation bytes, so the walks drop the continuations
(`gsub(/[\200-\277]/, "")`) and take the length of what is left. The
`LC_ALL=C` is not decoration -- gawk in a UTF-8 locale refuses that range as
a collation character and the walk dies rather than grading.

A character is not a terminal column, and the measure has two stated limits
because of it. A double-width glyph counts one and paints two, so a comment
of 61 characters can fill 91 columns and pass. A decomposed sequence counts
its marks, so `e` followed by a combining acute counts two and paints one,
and a comment of 92 characters reddens at 62 columns painted. Both are the
price of one verdict on every awk, both are cased beside the width cases,
and both are why every message here says `characters`: a number carrying a
unit it is not in is worse than no number at all. Nothing shipped sits near
either limit -- the only non-ASCII in the graded population is box drawing,
which is East-Asian-Width *ambiguous* and paints one column in a Latin
locale.

What cannot wrap is exempt by shape rather than by a list of files:

| shape | why it is exempt |
|---|---|
| a run longer than the limit (a path, a captured declaration) | it has nowhere to break, so the line is measured with it taken out and the prose beside it still wraps |
| a comment sharing its line with code | wrapping it would move the code |
| a page's fenced block, table row, heading, or lone link | a sample is a sample, a row is a row, and a heading carries no newline to wrap at |

The population is every file under `scripts/` whose first line names bash or
`sh`, and it is read in one place: `scripts/lib/script-population.sh`, which
`check-style.sh`, `check-portability.sh` and `check-budget-drift-cases.sh`
all source. That is why the remote-test fixtures carrying no suffix are
graded by each of them. Four rules here read that one list -- the width
walk, the two citation bans and the temp-file trap walk -- because a rule
spelled over `*.sh` grades a subset of its sibling's, and the difference is
where a finding sits unread.

Four shapes decide what the list holds, and each of them is there because a
short list reads exactly like a tree with nothing to report:

| shape | what the list does |
|---|---|
| a file the rules cannot open | red verdict naming it -- a `grep -l` or an `xargs awk` would drop it and grade the survivors |
| a symlink | listed (`find scripts \( -type f -o -type l \)`), so a dangling one and one pointing at a directory are both that red verdict |
| a fifo, socket or device node | named and passed over: it is readable, it could never carry a shebang, and reddening a gate for it is a false red nobody can act on |
| a path carrying a blank | selected by a loop rather than by `xargs`, so it stays in the population; the bans' `grep` and the walk's `xargs` then refuse it loudly rather than grading a list one file short |

Net of `check-style.sh` itself the population must still hold something. The
two citation bans skip that one file -- it spells the banned phrases in
order to define them -- and a `scripts/` holding nothing else leaves both of
them reporting `ok` having graded nothing, which is the shape the rule above
bans. That is its own red verdict, and it names the root it was run against.
