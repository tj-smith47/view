---
paths: ["scripts/**", "Taskfile.yml"]
---
# Shell conventions

## Every script under `scripts/` is written to bash 3.2

`Taskfile.yml` runs its scripts as `bash scripts/…`, so whichever bash is first
on `PATH` decides whether a gate runs at all. macOS ships `/bin/bash` 3.2.57,
and a contributor there gets a gate that dies mid-run with a message reading as
a script bug. CI's macos leg passes only because the runner image puts a newer
bash ahead of it. So the whole population is written to 3.2.

Three shapes break it. The first and the last are constructs you can name. The
middle one is a defect in 3.2's own reader: it finds the end of a `$( )` or
`<( )` by counting parens with quote awareness and no knowledge of comments, so
any `)` it counts that the writer did not mean ends the substitution early, and
any quote it counts swallows the rest.

| shape | 3.2 says | write instead |
|---|---|---|
| `declare -A m=([k]=v)` | `k: unbound variable` under `set -u` | a `case`, or newline-joined strings fed to `grep -Fqx` by here-string (`grep -Fqx -- "$x" <<<"$list"`) and never by a pipe. A quiet `grep` exits at its first match, SIGPIPEs the producer, and under `pipefail` the hit comes back a miss (`no_condition_reads_a_pipe_with_a_quiet_grep` in `view-oracle`'s shell guards refuses the pipe) |
| a paren or a quote inside `$( )` or `<( )` that the reader counts and the writer did not mean: a `case` pattern with no leading paren, or a `)` or an apostrophe in a comment | `syntax error near unexpected token`, or ``bad substitution: no closing `)' `` | give every `case` pattern its leading paren (`case "$x" in (*.*) … ;; esac`) and reword the comment. Keep the `case` word on its header line too. That spelling is the proxy a line scanner can see, and it is how the first instance is caught |
| `${x//a/b}` on anything longer than a word | nothing. It rescans the string per match and runs unbounded | `sed`/`tr` for a rewrite; `[[ $x == *[![:space:]]* ]]` (or its negation) for an emptiness test |

The last row's class is the every-match substitution and nothing wider.
`${x//a/b}`, `${x//a}` and `${!x//a/b}` are in it: the indirect spelling rescans
whatever the name holds, so it costs what the direct one costs, and the
construct grep matches both. `${x/#a/b}`, `${x/%a/b}` and `${x/a/b}` are out,
because each rebuilds the string once. The cost the row names is a rescan of
something long, whichever spelling a rewrite uses: moving a word-sized encode
into a process cost the `loc` gate four seconds against its one and a half.

The last one is the only shape that fails as a hang. The drift check's five
emptiness tests were written as "delete every blank and see what is left", and
the largest of them spun in bash itself, state R with no child, on a 12 kB
variable the shipped baselines build; still running at 283 s under an alarm,
where the glob test answered in 0 s.

The same ban covers `mapfile`, `readarray`, `[[ -v x ]]`, `${x,,}`, `${x^^}`,
`\|&`, `&>>` and `;;&`.

The middle row names two instances, and each has a gate of its own. The first, a
`case` pattern with no leading paren, is visible only to a parser, so what a
scan bans in its place is the proxy: a `case` whose word runs onto the next
line. That spelling took the whole style case matrix out of the 3.2 leg, and
every `case` in this population puts its `in` on the header line, so a `case`
with no `in` beside it is the tell. The defect the row names is the missing
paren: a split header whose patterns carry their parens extracts cleanly, and a
one-line header whose patterns lack them breaks exactly as the split one does.

The second instance, a comment inside a multi-line `$( )` or `<( )` whose own
parens do not balance, or which carries an odd number of quotes, is read
directly, because the reader is inside a substitution there and reads the
comment as code. Balanced parens in such a comment are left alone: the count
comes back and the substitution ends where it was written to, so the shipped
comment naming two sidecars in `check-budget-drift.sh` is no finding. It is
named here by file, since a line number in a rule goes stale at the next edit to
the file it points into.

Five legs in `scripts/check-budget-drift-cases.sh` enforce it over every file
under `scripts/` whose shebang names bash or `sh`; the remote-test fixtures
carry no suffix, so a script is graded without anyone remembering to add it
here. One greps the construct list, one reads the `case` word in command
position, one walks every comment sitting inside a `$( )` or a `<( )`, one
parses each script under `/bin/bash` when that is a pre-4 bash, and one runs the
drift check over the shipped tree, read-only, inside
`perl -e 'alarm 120; exec @ARGV'` and under that same stock `/bin/bash`. The
parse leg is the only one that sees the leading-paren-less `case` pattern, and
it runs on the host the contract is about. The timed leg catches a cost both
reading legs and every case tree are blind to, since a planted file is orders of
magnitude smaller than the baselines that ship; it runs under whatever that
interpreter turns out to be, since an overrun is a cost regression under any
bash, with the pattern substitution the first cause to check.

The same care applies to the awk these gates are written in, where the undefined
construct is a backslash inside a bracket expression: `/[ \t]+/` and
`/[^\\]";$/` are both left undefined by POSIX, and the three awks this tree has
been run under are free to disagree. `[ \t]` is spelled `[[:space:]]` and an
escaped quote is read by `substr` and `index`, so no bracket expression here
holds a backslash at all. A pin at the end of `scripts/check-style-cases.sh`
walks the population and reddens on the next one, reading a `[` where a command
starts as the `test` builtin.

A range of bytes is the one class with no escape-free spelling, so it is built:
`sprintf("[%c-%c]", 128, 191)` gives the continuation-byte range out of two
literal bytes, where `[\200-\277]` gives it out of two escapes POSIX leaves
undefined. The character measure in `scripts/check-style.sh` carries the built
form, hoisted into a variable because a walk calls it per line, and busybox,
mawk and gawk agree on its count. The escape before a digit is on the pin's
counted set, so the octal spelling reddens the moment it is written again.

Both reading legs take their line from one reader in
`scripts/lib/script-population.sh`, which carries quote, here-doc and nesting
state across lines the way 3.2 carries them. A word in a comment or in a
here-doc body is no command to it; a word in a string literal is, an over-read
taken so that nothing assembled in a string goes unread.

The here-doc half of that state comes from the one tokenizer in the tree,
`SCRIPT_HEREDOC_AWK` in the same file, which the userland scan reads as well,
through the same reader and so on the text outside every quote. A `<<` inside a
quoted argument, past a `#`, or inside `(( ))` opens nothing. A `<<<`
here-string opens nothing, because the third `<` ends the tag before it starts.
A fourth `<` (`cat <<<<EOF`) is refused by the shell as a syntax error and
carries no property for the scan to hold, so no branch reads it. A tag that
never terminates leaves the rest of its file unread by the reader and stops the
userland scan with `PORTABILITY-SELF-FAIL`.

Every spelling of the tag opens the same body: bare, `'TAG'`, `"TAG"`, `\TAG`,
`<<-TAG`, and any of those with the blank the shell allows between the operator
and its word. A quoted tag runs to its closing quote, so `<<'EOF-1'` is the tag
`EOF-1` and a plain `EOF` line inside that body closes nothing. An unquoted one
runs to the next blank, `;`, `|`, `&`, `<`, `>`, `(` or `)`, which is where the
shell ends the word, and it takes the `-` a blank separates from the operator
(`<< -TAG` is the tag `-TAG`). A tag may hold a dot, so `cat <<EOF.1` opens a
body a plain `EOF` line does not close. The quoting a tag carries sits anywhere
in the word: `cat <<EO"F"` and `cat <<EO\F` are both the tag `EOF` once the
shell has removed the quoting, so a run between a pair of quotes is taken
without them and a backslash is taken off the character it quotes. Read with the
quoting still in the tag, neither body ever terminates: loud in the userland
scan, as a `PORTABILITY-SELF-FAIL`, and silent in the style walk, which reddens
a handler that is right. Reaching that class needs the reader to hand the
tokenizer the quoted run, since the text outside every quote holds `cat <<EO`,
so the tail it reads for is the whole tag word built since the operator.

Whether the terminator may be tab-indented travels as a flag character of its
own and not as a `-` on the tag, since `<< -TAG` and `<<--TAG` name the same tag
and strip differently. A spelling read as no operator leaves the lines under it
scanned as commands, which is the direction that hides a finding behind text no
shell runs. Both scans hand this tokenizer the text outside every quote; read
off the raw line, `/<<-?[[:space:]]*$/` inside a quoted awk program is an opener
and every line under it goes unscanned.

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
two floors, on the substitutions that span more than one line and on the lines
read while one is open. The floors grade the numbers they were taken from: the
line anchor these replace entered three substitutions in the whole tree.

The population is the whole directory. The release path runs
`scripts/package-bundle.sh` from a workflow and `scripts/mbp-build-leg.sh` runs
on the macOS host the contract is about: five scripts `Taskfile.yml` never
names, and a broken one there fails where nobody is watching for it.

## A script's mode matches how it is run

A file with a bash or sh shebang outside `scripts/lib/` is an entry point and
carries the executable bit; everything else under `scripts/` is a library and
carries none. A script run by path fails with `Permission denied` when the bit
is missing, and a sourced library has no reason to carry one.
`check_script_modes` in `scripts/check-style.sh` grades every tracked
`scripts/**/*.sh` against that rule.

## A pipeline stage names the file it reads

`grep`/`awk`/`sed` with no file argument reads stdin, so a stage whose file list
came out empty does not fail. It sits there having graded nothing. A list built
by a `grep` over another file is checked for emptiness before it is passed on,
and where two legs read the same list the check sits above both: a leg that
reports `ok` having graded nothing disagrees with its own sibling about what an
empty list means.

## A `--prod-lines` pin counts a spelling written in a trailing comment

The three pins in `scripts/check-style.sh` that ask
`scripts/audit-god-files.sh --prod-lines` which lines are production code
(`check_tied_spawns`, `check_geometry_sites`, the condition-notice ownership
check) read the raw line the scanner emits. A whole-line comment never reaches
them, because the classifier drops it, but a spelling written after a `//` on a
line of code does, and it counts as a site.

The scanner's only eliding machinery removes string *contents*, and two of the
geometry spellings are string literals (`"nvim_ui_attach"`,
`"nvim_ui_try_resize"`): an elide-based emit would blind the walk to the wire
names in the one file that sends them. So the walks over-count, and the answer
to a red row whose code holds no geometry is to reword the comment or to move
the row.

Pinned by a case at the end of `scripts/check-style-cases.sh` that plants a
trailing comment naming a spelling and requires the count to rise.

## A `mktemp` is paired with an EXIT trap in the same script

A straight-line `rm` covers the ordinary path and nothing else. The gates here
scan a whole tree for seconds at a time, and a Ctrl-C or a `set -e` abort inside
that window leaves the file under `${TMPDIR:-/tmp}` for good, which is what
`check-style.sh`'s `read_prod_lines` did while it was the one temp file in
`scripts/` without a trap.

The trap is what removes the file; a straight-line `rm` beside it is fine and
stays for the ordinary path. A trap naming a variable needs that variable to
still exist when it fires: a `local` is gone by then and `set -u` aborts the
exit path on it, so the name the trap reads is a script global.

`check_temp_traps` in `scripts/check-style.sh` walks the shebang population
below for a `mktemp`. The pairing it requires is an armed `EXIT` trap whose
handler *removes* one of the variables a `mktemp` path went into, resolved
through the function the handler names, since half of these scripts write the
removal in a `cleanup()`, and through a function that body calls, since the
other half hand the root to a `cleanup_root "$X"` that removes its `$1`. The
names a call like that reaches are the ones written at the call.

A call is read where the shell reads one, outside every quote: a callee named
inside a string the handler prints (`echo "run; wipe now"`) runs nothing. The
trap line is a call site of its own, since the command a trap runs is a string
the shell re-parses, so `trap 'wipe_q "$QROOT"' EXIT` is the call `wipe_q` with
`$QROOT` as its argument, and the quotes around it are no part of it. What the
quotes inside that string decide is whether a name in it is still a name when
the trap fires. A single-quoted or `$'...'` run in the re-parsed command never
expands, so the removal written in one reaches no variable and is no pairing:
`trap "rm -rf '\$X'" EXIT` and a handler line `rm -rf '$X'` both leak. A name
the shell expanded while it built a double-quoted string is a path by the time
the trap is armed, and the quotes the re-parse reads then sit around a value, so
`trap "rm -rf '$SELFCHECK_TMP'" EXIT`, the line
`scripts/acceptance/artifacts.sh` arms, pairs, and the name travels outside the
quotes it was written inside.

A callee defined in a file the script sources is resolved through that file, one
level, tried beside the script, from the scan root, and under `scripts/lib/`. A
path none of the three resolves is named in the verdict as the line writes it,
because a boundary the walk cannot cross that says nothing reddens a right
script with the opposite message. The operand ends at the first blank outside
both the `$( )` and the quotes that opened it, which is what makes
`source "$(dirname "$0")/lib/x.sh"`, the shape every source line here writes,
resolve at all, and what keeps a quoted path holding a blank of its own whole in
the verdict. The line number, the path and the operand travel from that scan to
the shell as three records, since the operand is script text and can carry any
byte a separator could be.

The handler *removes* and does more than name: one that prints an accumulator
(`log="$log made $ROOT"`, `echo "$log"`) reads as a pairing to a walk asking
only whether the name appears, and the root is never removed. The names a
removal reaches are those on a line running `rm`, plus the list a removed loop
variable was bound from, which is how every array-of-roots cleanup here is
written. Every line of the handler is read as code first: a `#`-led `rm` is a
comment and reaches nothing, and a line under a `<<TAG` the handler never
terminates is here-doc text the trap never runs, so the removal written there is
a leak and reads as one. A tag inside a quoted argument (`printf '%s' "<<x"`) is
no operator and opens nothing.

A `mktemp` seeds through every quoting that still expands and whose value is the
temp path: `$(mktemp)`, `"$(mktemp)"` and `"'$(mktemp)'"`, whose single quotes
sit inside the double ones and quote nothing. It seeds through none that does
not: `'$(mktemp)'` and `$'$(mktemp)'` are literal text and make no file. A
prefixed `N="pre$(mktemp)"` is the third kind. It makes a file, and the name
holds `pre` and the path, so a removal over `$N` removes nothing that was made;
it is excluded by name, and the exclusion is fail-closed, so a script whose
`mktemp` reaches no recognised name is reported.

One case per shape. Two spellings pass a walk that only asks for the word
`trap`: `trap - EXIT`, which clears the handler, and a trap that reaps a child
and removes nothing. Both are cased, red, at the end of
`scripts/check-style-cases.sh`, beside the function-body pairing that has to
stay green. The walk is keyed on the script, because the removal legitimately
sits far from the `mktemp`.

The name a handler may pair on is the variable the `mktemp` assigned or a list
that variable is appended to (`ROOTS+=("$ROOT")`, `list="$list $ROOT"`), and it
is matched case-sensitively: a removal written over an array of roots reads
`$root` where the `mktemp` named `ROOT`, and the append is the relationship that
holds between the two. Pairing by lowercased name reads `TMP=/var/cache/keepme`
as the removal for `tmp=$(mktemp)`, which passes a leak in silence; that pair is
cased, red. An assignment that is not an append is no holder: `other=$ROOT/sub`
names a path inside the temp root, and removing it removes nothing of the root.

What a removal names is read the same way. A name pairs where the operand is its
own expansion (`$X`, `"$X"`, `${X}` and the root with a trailing slash) and
nowhere inside one: `rm -rf "pre$X"`, `rm -rf "$X.bak"` and `rm -rf "$X/sub"`
each delete a path the root still holds, and `rm -rf "\\$X"` deletes a path
whose name opens with a backslash, which is what bash makes of that pair. It
answered ok while stranding a root under a scratch `TMPDIR`. Four more spellings
pair on the same rule, because each reads out `$X` when `X` is set: `${X:?}`,
`${X:-}`, `${X?}` and `${X-}`, with or without a message and quoted or not. The
abort handler in `scripts/acceptance/artifacts.sh` writes
`rm -rf "${SELFCHECK_TMP:-}"`. `${X#pattern}`, `${X%pattern}` and
`${X/pattern/repl}` read out a different string and stay refused. The trap line
is read the same way, since the quotes around a name the shell has already
expanded sit around a path.

A handler body ends at the brace that closes the function, counted by depth over
the body with quoted and commented braces ignored and here-doc bodies skipped,
and never at an indentation: a `{ ...; } >&2` group written under the header
indentation, and a JSON here-doc holding a `}` in column one, each end the body
early under an indentation rule, and the removal below is never read. Both
shapes are cased, green.

The walk reads the shebang population. `scripts/*.sh` reaches neither
`scripts/acceptance/` nor a file with no suffix, and the first script that glob
missed was making a temp directory a Ctrl-C stranded.

## A `mktemp` names the root it writes in

A call handed no template answers under `$TMPDIR`, which is `/tmp` wherever
nothing set that: a small tmpfs every job running beside this one shares, and a
name that says nothing about which script made the file. One root for the whole
population, in `scripts/lib/scratch.sh`: the job's own scratch directory where a
harness set `CLAUDE_JOB_DIR`, the user's cache otherwise. A script sources that
file and writes `mktemp -d "$(scratch_root)/name-XXXXXX"`.

An operand naming some other root is still an operand, and a call handed one is
left alone, with one root excepted. `"${TMPDIR:-/tmp}/…"` is the same shared
tmpfs written longhand, so it buys nothing anywhere but `scripts/acceptance/`,
where what a leg makes is a session root a unix socket path is measured from and
the length a platform allows such a path is why the root is chosen by hand. That
directory is the whole of the exemption. Read as "some other root", the spelling
covered five gates that wanted the scratch directory and got the tmpfs.

`check_temp_roots` in `scripts/check-style.sh` reads each line outside its
single quotes and refuses two shapes: a `mktemp` whose words are all options,
and one whose operand is rooted at `$TMPDIR` in a file outside
`scripts/acceptance/`. An option word is read short or long, because
`--directory` read as an operand is a call that names nowhere passing as one
that names a root; and a line the shell joins on a trailing backslash is joined
here first, because the operand may sit on either side of the break. The quoting
is the whole of how the case files stay green: they plant scripts through
`printf '…'` and through here-doc bodies, and a spelling written in either is a
fixture, since the body is invisible to the reader this shares with the trap
walk above and the single-quoted run is stripped here. Nine cases at the end of
`scripts/check-style-cases.sh`, five red and four green, and each green one
reddens when the one rule it names is reverted on a scratch copy of the checker.

## A path a walk is guarded on is required by name

`scripts/check-style.sh` guards each walk on the directory or page it reads, so
a walk is never handed a root that is not there. A guard with no `else` is
fail-open: the run passes having graded nothing, and reports on rules it never
reached. The run therefore names every guarded directory in one
`for required in ...` list and every guarded file in `for required_file in ...`
beside it, and fails closed on each.

A pin at the end of `scripts/check-style-cases.sh` walks every guard spelling in
the checker under test: `[ -x PATH ]` for any test letter, the `[[ ... ]]` form,
a bracket-free `test -x PATH` in command position, and each of those three
negated (`[ ! -d PATH ]`, `[[ ! -f PATH ]]`, `test ! -e PATH`), which is the
spelling this checker writes its own three guards in. It reddens on the first
path missing from either list, so the next walk added behind any of them trips
there. One spelling is left out: `[ -d PATH ] || mkdir PATH` creates the path,
so requiring it to exist beforehand demands what the line is there to make, and
the whole line is passed over.

`test` is read where a command starts and nowhere else, through the same list
the split-`case` scan in `scripts/check-budget-drift-cases.sh` reads, and the
line is read as code, so a guard in a comment or a here-doc body is none. That
list is `SCRIPT_COMMAND_START`, the union of what the three scans that once
spelled it separately carried: the punctuation `^ ; & | ( { !`, and the words
`if`, `then`, `do`, `else`, `elif`, `while`, `until`. A word dropped from it
leaves a walk guarded behind that word fail-open, with the path it guards never
demanded, so the guard harvest plants one spelling per word and the pin at the
end of `scripts/check-style-cases.sh` walks the population for a list written
beside this one. A planted file carrying one guard per spelling grades the
harvest itself, `cargo test -p name` among them, which is `test` in argument
position and no guard at all.

That walk reads one more value beside the command-start list, because the same
thing happened to it: the ASCII field separator a scan reaches for when it
builds a record out of a script line and its own fields was written twice over,
under two names and in two spellings. It is `SCRIPT_FIELD_SEP` in
`scripts/lib/script-population.sh`, and the walk reddens on `$'\034'`, `\x1c`
and the byte itself written into any other assignment in the population. A
separator carries no harvested line between two passes over the same script: the
temp-trap harvest writes the two halves as two records, since a newline cannot
occur inside a record awk read as a line.

The same shape in one line, `[ -d docs ] && targets="$targets docs"`, is safe
where the population writes it and unsafe wherever its status becomes a
command's status. Mid-body under `set -e` it does what it reads as: the failing
command is the test, which precedes the final `&&`, so errexit is suppressed and
the run continues with `targets` unset. Written last, the list's status of 1
becomes the status of whatever ends with it, and what happens next depends on
which construct that is. Observed under bash 5.3.9, each line run with
`set -euo pipefail`:

| written last in | what the run does |
|---|---|
| the script | exits 1 |
| a function (`f() { … }; f; echo S`) | exits 1, `S` unprinted |
| a subshell in a list (`( … ); echo S`) | exits 1, `S` unprinted |
| a command substitution (`x=$(f)`) | exits 1. The assignment takes what `f` returned |
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
assignment, the `cmdsubst` row above. The `|| true` on the closing `done)` is
what defuses it. That is a statement about the tree, so a pin at the end of
`scripts/check-style-cases.sh` walks the population for a guarded `&&` list
whose next code line closes a function or a substitution, and reddens on any
that carries no `|| true` and on a second file appearing beside that one. That
walk reads the same `SCRIPT_COMMAND_START`: anchored at the line start, it read
neither `x=1; [ -d docs ] && ...` nor the same list inside a `{ ...; }` group,
both of which carry the caller verdict exactly as the line-start spelling does.
Write it `if [ -d docs ]; then ... fi` anyway, because which of the positions a
reader is looking at depends on what follows the line.

## A symbolic link a script makes is written `ln -sn`

`ln -s` whose destination already exists as a symlink to a directory does not
fail. It follows the link and creates the new one *inside* the target, which is
how a checker-copy helper in `scripts/check-style-cases.sh`, whose `$dir/lib`
points at the graded tree's own `scripts/lib/`, wrote `scripts/lib/lib` into the
tree the gates read when two copies shared one case number. The next population
read could not open it and all three gates over that population reddened, on a
tree nobody had edited. The sibling `ln` beside it, pointing at a file, failed
loudly in the same situation; this one succeeded silently. `-n` is in both BSD
and GNU `ln`, refuses where `ln -s` writes through, and is a no-op where the
destination cannot pre-exist, so the whole population uses it. Pinned by a case
at the end of `scripts/check-style-cases.sh` that walks the population for the
flagless spelling at the start of a command.

## A comment wraps at 80 characters

`scripts/check-style.sh` grades every comment line under `scripts/` at the width
its markdown pages are held to: `README.md`, `docs/` and the convention pages
under `.claude/rules/`, which the same walk reads and which the run requires by
name like every other guarded directory. What the message says has to be what an
editor shows, so the walks count characters and not bytes: a rule drawn in box
characters is 77 characters and 105 bytes, and a gate measuring bytes reddens it
while passing a wider line whose long token it subtracts whole.

Counted without a UTF-8 awk, because the same verdict has to come back from
gawk, mawk and BSD awk: under `LC_ALL=C` a character is a lead byte plus its
continuation bytes, so the walks drop the continuations
(`gsub(/[\200-\277]/, "")`) and take the length of what is left. The `LC_ALL=C`
carries weight: gawk in a UTF-8 locale refuses that range as a collation
character and the walk dies without grading. The three drift scripts export it
beside each script's own `set` line for the other half of the same reason: the
tokenizer in `scripts/lib/moment-grading.sh` slices a page line with `substr`
and hands each byte to a regex, and the awk macOS ships aborts on the lead byte
of a box glyph. `scripts/check-portability.sh` names a consumer of that
tokenizer whose first awk runs above the export.

A character is no terminal column, and the measure has two stated limits because
of it. A double-width glyph counts one and paints two, so a comment of 61
characters can fill 91 columns and pass. A decomposed sequence counts its marks,
so `e` followed by a combining acute counts two and paints one, and a comment of
92 characters reddens at 62 columns painted. Both are the price of one verdict
on every awk, both are cased beside the width cases, and both are why every
message here says `characters`. The only non-ASCII in the graded population is
box drawing, which is East-Asian-Width *ambiguous* and paints one column in a
Latin locale.

A line short of the width is graded too, where its paragraph runs on past it. A
sentence added to a wrapped block and re-wrapped only where it went over the
limit leaves the line before it short, and the width walk reads that page as
clean because it grades the maximum alone; five paragraphs of this page were
written that way and nothing could see them. So a line under 60 characters whose
next line continues the same paragraph reddens, over the same pages the width
walk grades. Three shapes are legitimately short and are not graded: a line that
ends its paragraph, a line whose next word is longer than what is left of the
limit, and a line a heading, a table row, a list item, a rule or a fence
follows. Each is cased beside the width cases. The word that decides whether the
next line would have fitted is the whole inline code span at the head of it plus
the punctuation behind it, because that is what a re-wrap would have to move:
measured to the first blank, the span's first word fits, the seam reddens, and
the only way to satisfy it is to break the span.

Which a wrap may not do. A wrap landing inside an inline code span leaves the
page reading the same and the span gone, and these pages are read by their
spans: `crates/view-harness/src/bin/bench.rs` looks its fixtures up in
`docs/benchmarking.md` by the span each is written as, so a split span is a page
whose next edit reddens a test. A span the width walk finds still open at the
end of a line reddens over the same pages, and a run of backticks closes only on
a run of its own length, so a span holding a lone backtick inside a doubled pair
is one span. A span longer than the limit is left alone, since it has nowhere to
go.

The span state belongs to the paragraph, since a span cannot cross a blank line
and cannot cross a fence. The run and the text it holds are cleared on a blank
line and at a fence line, so a stray tick costs the paragraph it sits in and no
more. Carried past either, a stray backtick pairs with the opening tick of the
next real span and the split span below it goes unreported.

A list item a wrap pulled up onto the line above it reddens too, over the same
pages. Nothing else in the walk can see one: a list opener is exempt from the
ragged rule, the merged line is inside the width, and a comparison of the word
stream before and after reads the `-` either way. Three bullets were spent that
way to rejoin split spans, and one of them was a convention heading that stopped
being an item of its list. The rule is anchored on the end of a sentence,
because prose writes a bare `-` or `+` mid-line as arithmetic far more often
than as a bullet, and it grades a list opener as well as a continuation, since
an item is merged into the opener above it as often as into one of its
continuations.

What it cannot see is the other half of the same damage: a paragraph break the
wrap deleted leaves text no rule can call wrong, and only a comparison against
the revision before it finds one. `scripts/check-rewrap-structure.sh BASE HEAD`
is that comparison, and a sweep runs it before it commits. Per page it reports
the blank lines and list openers that fell between the two trees where the words
themselves did not change, and every base block whose text survives verbatim
inside a bigger head block, which is the deleted break itself: the same words,
one paragraph shorter. It reports and never gates: the second shape names a
paragraph that legitimately gained a sentence as readily as one a wrap
swallowed. A hard gate needs that tail answered first.

What cannot wrap is exempt by shape:

| shape | why it is exempt |
|---|---|
| a run longer than the limit (a path, a captured declaration) | it has nowhere to break, so the line is measured with it taken out and the prose beside it still wraps |
| a comment sharing its line with code | wrapping it would move the code |
| a page's fenced block, table row, heading, or lone link | a sample is a sample, a row is a row, and a heading carries no newline to wrap at |

The population is every file under `scripts/` whose first line names bash or
`sh`, and it is read in one place: `scripts/lib/script-population.sh`, which
`check-style.sh`, `check-portability.sh` and `check-budget-drift-cases.sh` all
source. That is why the remote-test fixtures carrying no suffix are graded by
each of them. Four rules here read that one list, the width walk, the two
citation bans and the temp-file trap walk, because a rule spelled over `*.sh`
grades a subset of its sibling's, and the difference is where a finding sits
unread.

Four shapes decide what the list holds, and each of them is there because a
short list reads exactly like a tree with nothing to report:

| shape | what the list does |
|---|---|
| a file the rules cannot open | red verdict naming it. A `grep -l` or an `xargs awk` drops it and grades the survivors |
| a symlink | listed (`find scripts \( -type f -o -type l \)`), so a dangling one and one pointing at a directory are both that red verdict |
| a fifo, socket or device node | named and passed over: it is readable, it could never carry a shebang, and reddening a gate for it is a false red nobody can act on |
| a path carrying a blank | selected by a loop, so it stays in the population; the bans' `grep` and the walk's `xargs` then refuse it loudly and the run stops |

Net of `check-style.sh` itself the population must still hold something. The two
citation bans skip that one file, since it spells the banned phrases in order to
define them, and a `scripts/` holding nothing else leaves both of them reporting
`ok` having graded nothing, which is the shape the rule above bans. That is its
own red verdict, and it names the root it was run against.
