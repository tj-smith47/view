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

Three shapes break it. The first and the last are constructs you can name.
The middle one is not a construct at all but a defect in 3.2's own reader:
it finds the end of a `$( )` or `<( )` by counting parens with quote
awareness and no knowledge of comments, so any `)` it counts that the
writer did not mean ends the substitution early, and any quote it counts
swallows the rest.

| shape | 3.2 says | write instead |
|---|---|---|
| `declare -A m=([k]=v)` | `k: unbound variable` under `set -u` | a `case`, or newline-joined strings fed to `grep -Fqx` by here-string (`grep -Fqx -- "$x" <<<"$list"`) and never by a pipe — a quiet `grep` exits at its first match, SIGPIPEs the producer, and under `pipefail` the hit comes back a miss (`crates/view-oracle/tests/shell_guards.rs:402` refuses the pipe) |
| a paren or a quote inside `$( )` or `<( )` that the reader counts and the writer did not mean: a `case` pattern with no leading paren, or a `)` or an apostrophe in a comment | `syntax error near unexpected token`, or ``bad substitution: no closing `)' `` | give every `case` pattern its leading paren (`case "$x" in (*.*) … ;; esac`) and reword the comment. Keep the `case` word on its header line too — that is not a third instance but the proxy a line scanner can see, and it is how the first one is caught |
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

The middle row names two instances, and each has a gate of its own. The
first -- a `case` pattern with no leading paren -- is visible only to a
parser, so what a scan bans in its place is the proxy: a `case` whose word
runs onto the next line. That spelling took the whole style case matrix out
of the 3.2 leg, and every `case` in this population puts its `in` on the
header line, so a `case` with no `in` beside it is the tell. The missing
paren is the defect and the newline is not: a split header whose patterns
carry their parens extracts cleanly, and a one-line header whose patterns
do not breaks exactly as the split one does.

The second instance -- a comment inside a multi-line `$( )` or `<( )` whose
own parens do not balance, or which carries an odd number of quotes -- is
read directly, because the reader is inside a substitution there and reads
the comment as code. Balanced parens in such a comment are left alone: the
count comes back and the substitution ends where it was written to, which is
why the shipped comments at `check-budget-drift.sh:337-340` are not
findings.

Five legs in `scripts/check-budget-drift-cases.sh` enforce it over every file
under `scripts/` whose shebang names bash or `sh` -- the remote-test fixtures
carry no suffix -- so a script is graded without anyone remembering to add it
here: one greps the construct list, one reads the `case` word in command
position, one walks every comment sitting inside a `$( )` or a `<( )`, one
parses each script under `/bin/bash` when that is a pre-4 bash — which is the
only leg that sees the leading-paren-less `case` pattern, and it runs on the
host the contract is about — and one runs the drift check over the shipped tree,
read-only, inside `perl -e 'alarm 120; exec @ARGV'` and under that same stock
`/bin/bash`, because a cost this size is invisible to both of the reading legs
and to every case tree, whose planted files are orders of magnitude smaller than
the baselines that ship. The timed leg runs whatever that interpreter turns out
to be: an overrun is a cost regression under any bash, and the pattern
substitution is the first cause to check rather than the only one.

The same care applies to the awk these gates are written in, where the
undefined construct is a backslash inside a bracket expression: `/[ \t]+/`
and `/[^\\]";$/` are both left undefined by POSIX, and the three awks this
tree has been run under are free to disagree. `[ \t]` is spelled
`[[:space:]]` and an escaped quote is read by `substr` and `index`, so no
bracket expression here holds a backslash at all; a pin at the end of
`scripts/check-style-cases.sh` walks the population and reddens on the next
one, reading a `[` where a command starts as the `test` builtin rather than
as a bracket expression.

Both reading legs take their line from one reader in
`scripts/lib/script-population.sh`, which carries quote, here-doc and nesting
state across lines the way 3.2 carries them. A word in a comment or in a
here-doc body is not a command to it; a word in a string literal is, which is an
over-read taken on purpose so that nothing assembled in a string goes unread.
The here-doc half of that state comes from the one tokenizer in the tree,
`SCRIPT_HEREDOC_AWK` in the same file, which the userland scan reads as well,
through the same reader and so on the text outside every quote: a `<<` inside a
quoted argument, past a `#`, or inside `(( ))` opens nothing, a `<<<`
here-string opens nothing because the third `<` ends the tag before it starts, a
fourth `<` (`cat <<<<EOF`) is refused by the shell as a syntax error and carries
no property for the scan to hold -- no branch reads it, and none is added,
because no file this population accepts can carry the spelling. A tag that never
terminates leaves the rest of its file unread by the reader and stops the
userland scan with `PORTABILITY-SELF-FAIL` rather than narrowing it in silence.
Every spelling of the tag opens the same body -- bare, `'TAG'`, `"TAG"`, `\TAG`,
`<<-TAG`, and any of those with the blank the shell allows between the operator
and its word. A quoted tag runs to its closing quote, so `<<'EOF-1'` is the tag
`EOF-1` and a plain `EOF` line inside that body closes nothing; an unquoted one
runs to the next blank, `;`, `|`, `&`, `<`, `>`, `(` or `)`, which is where the
shell ends the word, and it takes the `-` a blank separates from the operator
(`<< -TAG` is the tag `-TAG`). A tag may hold a dot, so `cat <<EOF.1` opens a
body that a plain `EOF` line does not close, and reading the tag as the name up
to the dot leaves the rest of that body scanned as commands. The quoting a tag
carries sits anywhere in the word and not only at its head: `cat <<EO"F"` and
`cat <<EO\F` are both the tag `EOF` once the shell has removed the quoting, so a
run between a pair of quotes is taken without them and a backslash is taken off
the character it quotes. Read with the quoting still in the tag, neither body
ever terminates: loud in the userland scan, as a `PORTABILITY-SELF-FAIL`, and
silent in the style walk, which reddens a handler that is right. Reaching that
class at all needs the reader to hand the tokenizer the quoted run, since the
text outside every quote holds `cat <<EO`, so the tail it reads for is the whole
tag word built since the operator rather than the operator alone. That class
costs nothing now that both scans hand this tokenizer the text outside every
quote: read off the raw line instead, `/<<-?[[:space:]]*$/` inside a quoted awk
program is an opener and every line under it goes unscanned. Whether the
terminator may be tab-indented travels as a flag character of its own and not as
a `-` on the tag, since `<< -TAG` and `<<--TAG` name the same tag and strip
differently. A spelling read as no operator leaves the lines under it scanned as
commands, which is the direction that hides a finding behind text no shell runs.

The construct list is itself graded, by three planted spellings: the direct and
the indirect substitution, each of which it must refuse, and an anchored one,
which it must let through. The split `case` scan is graded by six: a header
whose word runs onto the next line, the same header written after a brace, the
same after an `if`, the word written as prose in a string, a comment and a
here-doc body it must leave alone, and the lines under a `<<` spelling written
inside a string, which it must read. The comment walk is graded by six: a paren
in a substitution opened at end of line, an apostrophe in one opened with
content still on the line, a substitution closed on a `done)` whose later
comments it must not touch, a nested pair with the defect on the inner level, a
plain `( )` group inside a substitution whose closing paren must not end it, and
two floors -- on the substitutions that span more than one line and on the lines
read while one is open. The floors grade the numbers they were taken from: the
line anchor these replace entered three substitutions in the whole tree, and a
reader that stopped carrying state across lines would read none of those lines
while reporting more opens than before.

The population is the directory rather than the scripts `Taskfile.yml` names,
because the release path runs `scripts/package-bundle.sh` from a workflow and
`scripts/mbp-build-leg.sh` runs on the macOS host the contract is about — five
scripts no task names, and a broken one there fails where nobody is watching for
it.

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
(`check_tied_spawns`, `check_geometry_sites`, the condition-notice ownership
check) read the raw line the scanner emits. A whole-line comment never reaches
them -- the classifier drops it -- but a spelling written after a `//` on a line
of code does, and it counts as a site.

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
below for a `mktemp`, and the pairing it requires is an armed `EXIT` trap whose
handler *removes* one of the variables a `mktemp` path went into -- resolved
through the function the handler names, since half of these scripts write the
removal in a `cleanup()`, and through a function that body calls, since the
other half hand the root to a `cleanup_root "$X"` that removes its `$1`. The
names a call like that reaches are the ones written at the call. A call is read
where the shell reads one, outside every quote: a callee named inside a string
the handler prints (`echo "run; wipe now"`) runs nothing, and reading it as a
call pairs the root against a removal that never fires. The trap line is a call
site of its own -- the command a trap runs is a string the shell re-parses, so
`trap 'wipe_q "$QROOT"' EXIT` is the call `wipe_q` with `$QROOT` as its
argument, and the quotes around it are not part of it. What the quotes inside
that string decide is whether a name in it is still a name when the trap fires.
A single-quoted or `$'...'` run in the re-parsed command never expands, so the
removal written in one reaches no variable and is no pairing:
`trap "rm -rf '\$X'" EXIT` and a handler line `rm -rf '$X'` both leak. A name
the shell expanded while it built a double-quoted string is a path by the time
the trap is armed, and the quotes the re-parse reads then sit around a value
rather than around a name -- which is why `trap "rm -rf '$SELFCHECK_TMP'" EXIT`,
the line `scripts/acceptance/artifacts.sh` arms, pairs, and why the name travels
outside the quotes it was written inside. A callee defined in a file the script
sources is resolved through that file, one level, tried beside the script, from
the scan root, and under `scripts/lib/`; a path none of the three resolves is
named in the verdict as the line writes it, because a boundary the walk cannot
cross that says nothing reddens a right script with a message claiming the
opposite, and a fragment of the operand names nothing a reader can go and look
at. The operand ends at the first blank outside both the `$( )` and the quotes
that opened it, which is what makes `source "$(dirname "$0")/lib/x.sh"` -- the
shape every source line here writes -- resolve at all, and what keeps a quoted
path holding a blank of its own whole in the verdict. The line number, the path
and the operand travel from that scan to the shell as three records rather than
three fields, for the reason the harvest writes two records: the operand is
script text and can carry any byte a separator could be. Removes and not merely
names: a handler that prints an accumulator (`log="$log made $ROOT"`,
`echo "$log"`) reads as a pairing to a walk asking only whether the name
appears, and the root is never removed. The names a removal reaches are those on
a line running `rm`, plus the list a removed loop variable was bound from, which
is how every array-of-roots cleanup here is written. Every line of the handler
is read as code first: a `#`-led `rm` is a comment and reaches nothing, and a
line under a `<<TAG` the handler never terminates is here-doc text the trap
never runs, so the removal written there is a leak and reads as one. A tag
inside a quoted argument (`printf '%s' "<<x"`) is not the operator and opens
nothing. A `mktemp` seeds through every quoting that still expands and whose
value is the temp path -- `$(mktemp)`, `"$(mktemp)"` and `"'$(mktemp)'"`, whose
single quotes sit inside the double ones and quote nothing -- and through none
that does not: `'$(mktemp)'` and `$'$(mktemp)'` are literal text and make no
file. A prefixed `N="pre$(mktemp)"` is the third kind: it makes a file, and the
name holds `pre` and the path, so a removal over `$N` removes nothing that was
made. It is excluded by name, and the exclusion is fail-closed -- a script whose
`mktemp` reaches no recognised name is reported rather than passed. One case per
shape. Two spellings pass a walk that only asks for the word `trap`:
`trap - EXIT`, which clears the handler rather than arming one, and a trap that
reaps a child and removes nothing. Both are cased, red, at the end of
`scripts/check-style-cases.sh`, beside the function-body pairing that has to
stay green. Keyed on the script rather than on the statement, because the
removal legitimately sits far from the `mktemp`. The name a handler may pair on
is the variable the `mktemp` assigned or a list that variable is appended to
(`ROOTS+=("$ROOT")`, `list="$list $ROOT"`), and it is matched case-sensitively:
a removal written over an array of roots reads `$root` where the `mktemp` named
`ROOT`, and the append is the relationship that actually holds between the two.
Pairing by lowercased name instead reads `TMP=/var/cache/keepme` as the removal
for `tmp=$(mktemp)`, which passes a leak in silence -- that pair is cased, red.
An assignment that is not an append is not a holder: `other=$ROOT/sub` names a
path inside the temp root and removing it removes nothing of the root. What a
removal names is read the same way: a name pairs where the operand is its own
expansion -- `$X`, `"$X"`, `${X}` and the root with a trailing slash -- and
never where it sits inside one. `rm -rf "pre$X"`, `rm -rf "$X.bak"` and
`rm -rf "$X/sub"` each delete a path the root still holds, and `rm -rf "\\$X"`
deletes a path whose name opens with a backslash, which is what bash makes of
that pair: it answered ok while stranding a root under a scratch `TMPDIR`.
Four more spellings pair on the same rule, because each reads out `$X` when `X`
is set: `${X:?}`, `${X:-}`, `${X?}` and `${X-}`, with or without a message and
quoted or not -- the abort handler in `scripts/acceptance/artifacts.sh` writes
`rm -rf "${SELFCHECK_TMP:-}"`. `${X#pattern}`, `${X%pattern}` and
`${X/pattern/repl}` read out a different string and stay refused. The trap line
is read the same way, since the quotes around a name the shell has
already expanded sit around a path rather than around a name. A handler
body ends at the brace that closes the function, counted by depth over the body
with quoted and commented braces ignored and here-doc bodies skipped, never at
an indentation: a `{ ...; } >&2` group written under the header indentation, and
a JSON here-doc holding a `}` in column one, each end the body early under an
indentation rule and the removal below is never read. Both shapes are cased,
green. The population and not `scripts/*.sh`: that glob reaches neither
`scripts/acceptance/` nor a file with no suffix, and the first script it missed
was making a temp directory a Ctrl-C stranded.

## A path a walk is guarded on is required by name

`scripts/check-style.sh` guards each walk on the directory or page it reads, so
a walk is never handed a root that is not there. A guard with no `else` is
fail-open: the run passes having graded nothing, and reports on rules it never
reached. The run therefore names every guarded directory in one
`for required in ...` list and every guarded file in `for required_file in ...`
beside it, and fails closed on each; a pin at the end of
`scripts/check-style-cases.sh` walks every guard spelling in the checker under
test -- `[ -x PATH ]` for any test letter, the `[[ ... ]]` form, a bracket-free
`test -x PATH` in command position, and each of those three negated
(`[ ! -d PATH ]`, `[[ ! -f PATH ]]`, `test ! -e PATH`), which is the spelling
this checker actually writes its own three guards in -- and reddens on the first
path missing from either list, so the next walk added behind any of them trips
there rather than shipping silent. One spelling is left out:
`[ -d PATH ] || mkdir PATH` creates the path, so requiring it to exist
beforehand demands what the line is there to make, and the whole line is passed
over. `test` is read where a command starts and nowhere else, through the same
list the split-`case` scan in `scripts/check-budget-drift-cases.sh` reads, and
the line is read as code, so a guard in a comment or a here-doc body is none.
That list is `SCRIPT_COMMAND_START`, and it is the union of what the three scans
that once spelled it separately carried: the punctuation `^ ; & | ( { !`, and
the words `if`, `then`, `do`, `else`, `elif`, `while`, `until`. A word dropped
from it leaves a walk guarded behind that word fail-open, with the path it
guards never demanded, so the guard harvest plants one spelling per word and the
pin at the end of `scripts/check-style-cases.sh` walks the population for a list
written beside this one rather than out of it. A planted file carrying one guard
per spelling grades the harvest itself, `cargo test -p name` among them, which
is `test` in argument position and no guard at all.

That walk reads one more value beside the command-start list, for the same
reason and because the same thing happened to it: the ASCII field separator a
scan reaches for when it builds a record out of a script line and its own
fields was written twice over, under two names and in two spellings, each with
its own comment arguing the byte. It is `SCRIPT_FIELD_SEP` in
`scripts/lib/script-population.sh`, and the walk reddens on `$'\034'`,
`\x1c` and the byte itself written into any other assignment in the
population. A separator is not what carries the two halves of a harvested
line between two passes over the same script, though: a script line can hold
any byte a separator could be, and a split on the first occurrence cuts such a
line where the script wrote the byte rather than where the harvest did. The
temp-trap harvest writes the two halves as two records instead, since a
newline cannot occur inside a record awk read as a line.

The same shape in one line, `[ -d docs ] && targets="$targets docs"`, is safe
where the population writes it and unsafe wherever its status becomes a
command's status. Mid-body under `set -e` it does what it reads as: the
failing command is the test, which precedes the final `&&`, so errexit is
suppressed and the run continues with `targets` unset. Written last, the
list's status of 1 becomes the status of whatever ends with it, and what
happens next depends on which construct that is. Observed under bash 5.3.9,
each line run with `set -euo pipefail`:

| written last in | what the run does |
|---|---|
| the script | exits 1 |
| a function (`f() { … }; f; echo S`) | exits 1, `S` unprinted |
| a subshell in a list (`( … ); echo S`) | exits 1, `S` unprinted |
| a command substitution (`x=$(f)`) | exits 1 -- the assignment takes what `f` returned |
| a brace group (`{ … }; echo S`) | carries status 1, prints `S`, does not exit |
| a loop body (`for i in 1; do … done; echo S`) | carries status 1, prints `S`, does not exit |

The last two inherit the suppression from the list inside them, so only their
status moves; the first four abort. A missing directory then returns 1 to a
caller reading it as a failure. The population writes the shape mid-body and as
the last command of a loop body (`[ -n "$root" ] && rm -rf "$root"` in the
acceptance cleanups), both of which only carry the status. It writes one line in
a position that does not: the `stale=$(cd "$ROOT" && for f in $GUARDED; …)`
parse leg in `scripts/check-budget-drift-cases.sh` ends its loop body with a
guarded list, the loop ends the substitution, and the status would reach the
assignment -- the `cmdsubst` row above. The `|| true` on the closing `done)` is
what defuses it. That is a claim about the tree rather than about the shell, so
a pin at the end of `scripts/check-style-cases.sh` walks the population for a
guarded `&&` list whose next code line closes a function or a substitution, and
reddens on any that carries no `|| true` and on a second file appearing beside
that one. That walk reads the same `SCRIPT_COMMAND_START`: anchored at the line
start instead, it read neither `x=1; [ -d docs ] && ...` nor the same list
inside a `{ ...; }` group, both of which carry the caller verdict exactly as the
line-start spelling does. Write it `if [ -d docs ]; then ... fi` anyway, because
which of the positions a reader is looking at depends on what follows the line
rather than on the line.

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

`scripts/check-style.sh` grades every comment line under `scripts/` at the width
its markdown pages are held to -- `README.md`, `docs/` and the convention pages
under `.claude/rules/`, which the same walk reads and which the run requires by
name like every other guarded directory -- and a contributor meets the rule for
the first time when it reddens a line. What the message says has to be what an
editor shows, so the walks count characters and not bytes: a rule drawn in box
characters is 77 characters and 105 bytes, and a gate measuring bytes reddens it
while passing a wider line whose long token it subtracts whole.

Counted without a UTF-8 awk, because the same verdict has to come back from
gawk, mawk and BSD awk: under `LC_ALL=C` a character is a lead byte plus its
continuation bytes, so the walks drop the continuations
(`gsub(/[\200-\277]/, "")`) and take the length of what is left. The `LC_ALL=C`
is not decoration -- gawk in a UTF-8 locale refuses that range as a collation
character and the walk dies rather than grading.

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

A line short of the width is graded too, where its paragraph runs on past it.
A sentence added to a wrapped block and re-wrapped only where it went over the
limit leaves the line before it short, and the width walk reads that page as
clean because it grades the maximum alone -- five paragraphs of this page were
written that way and nothing could see them. So a line under 60 characters
whose next line continues the same paragraph reddens, over the same pages the
width walk grades. Three shapes are short for a reason and are not graded: a
line that ends its paragraph, a line whose next word is longer than what is
left of the limit, and a line a heading, a table row, a list item, a rule or a
fence follows. Each is cased beside the width cases. The word that decides
whether the next line would have fitted is the whole inline code span at the
head of it plus the punctuation behind it, because that is what a re-wrap would
have to move: measured to the first blank instead, the span's first word fits,
the seam reddens, and the only way to satisfy it is to break the span.

Which a wrap may not do. A wrap landing inside an inline code span leaves the
page reading the same and the span gone, and these pages are read by their
spans: `crates/view-harness/src/bin/bench.rs` looks its fixtures up in
`docs/benchmarking.md` by the span each is written as, so a split span is a page
whose next edit reddens a test rather than the page. A span the width walk finds
still open at the end of a line reddens over the same pages, and a run of
backticks closes only on a run of its own length, so a span holding a lone
backtick inside a doubled pair is one span rather than three. A span longer than
the limit is left alone for the reason the over-long run is: it has nowhere to
go.

The span state belongs to the paragraph, because a span cannot cross a blank
line and cannot cross a fence. Carried past either, one stray backtick used as
punctuation pairs with the opening tick of the next real span, the text that
accumulates runs past the limit and takes the over-long exemption, and the
genuine split span below it is never reported -- silent, which is the
direction that matters. So the run and the text it holds are cleared on a
blank line and at a fence line, and a stray tick costs the paragraph it sits
in rather than the rest of the page.

A list item a wrap pulled up onto the line above it reddens too, over the same
pages. Nothing else in the walk can see one: a list opener is exempt from the
ragged rule, the merged line is inside the width, and a comparison of the word
stream before and after reads the `-` either way -- three bullets were spent
that way to rejoin split spans, and one of them was a convention heading that
stopped being an item of its list. The rule is anchored on the end of a
sentence, because prose writes a bare `-` or `+` mid-line as arithmetic far
more often than as a bullet, and it grades a list opener as well as a
continuation, since an item is merged into the opener above it as often as
into one of its continuations. What it cannot see is the other half of the
same damage: a paragraph break the wrap deleted leaves text no rule can call
wrong, and only a comparison against the revision before it finds one.
`scripts/check-rewrap-structure.sh BASE HEAD` is that comparison, and a sweep
runs it before it commits. Per page it reports the blank lines and list openers
that fell between the two trees where the words themselves did not change, and
every base block whose text survives verbatim inside a bigger head block, which
is the deleted break itself -- the same words, one paragraph shorter. It reports
and never gates: the second shape names a paragraph that legitimately gained a
sentence as readily as one a wrap swallowed, and over the sweep this rule came
out of it named the merged block and two more. A hard gate needs that tail
answered first.

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
