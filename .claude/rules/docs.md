# User-facing page conventions

## The README lists what a person gets

`README.md` is read by someone deciding whether to try view. Every row of its
Features and Roadmap lists names a capability that person uses, in their words,
and nothing else. What does not belong there, whatever streak produced it:

| shape | where it goes instead |
|---|---|
| a bug fix, or behaviour a person expects to be true (the engine dies with the terminal, `:qa!` quits, a theme following the system's colorscheme switcher) | the commit subject; `docs/` if a person has to know |
| internals (RPC seam, redraw path, multigrid, process supervision mechanics) | the spec, `docs/architecture` pages |
| evidence and receipts (oracle, fuzz harness, compat suite, benchmark matrix, "the build fails if…") | `docs/benchmarking.md`, `docs/performance.md` |
| a session note, a phase or streak name, a "landed in" reference | the plan ledger, `.claude/` |

A capability the tooling makes possible is stated as the capability: "your
plugins keep working" and never "compat suite with pinned plugin stacks".
A feature is stated as what the person sees, and never as how it is built:
"the character you press appears at once" and never "keystrokes are echoed
before the round trip returns". The words of the mechanism (`round trip`,
`paint`, `RPC`, `OSC 52`, `surface`, `chrome`, `passthrough`, `tier` and
their siblings) belong on the pages under `docs/`; `check_prose_frames`
refuses them on `README.md` from the `PROSE_MECHANISM` list in
`scripts/check-style.sh`, and a new one is added there. Neovim is named as
the engine once, in the sentence that says your config runs unchanged.

view is described in its own terms. Neovim is named where a fact about the
engine is stated: the bundled version, the pin, a paired number in the
Performance section. A feature row gives the capability and never credits
Neovim for it.

The influences view takes something from (Omarchy, Hyprland, qutebrowser, tmux,
herdr, ACP, kitty, mpv) are named once, in the paragraph under Roadmap that says
what view takes from each.

The page earns its reader's attention with a small number of chosen visuals (a
screenshot, a tape or gif of a moment, a diagram for an idea prose explains
badly), each placed beside the feature it shows. A row that needs a paragraph to
explain is a row that needs a picture or a cut.

Before a push, the whole Shipped and Landing lists are audited against this
table.

## A page is written for a reader who has not doubted anything

A sentence on a page a person reads exists to say what is true or what to do.
Every other sentence, clause or word is removed: whatever justifies, defends,
anticipates an objection, argues for a design, or denies an alternative. Removal
is the whole of the fix, and a page that comes out thin is the right length.

The stance has many spellings and the words are only the visible ones: the
contrast frame (`X, not Y`, `not X but Y`, `rather than`, `instead of`,
`never Y`, `isn't X, it's Y`), the tell words (`claim`, `prove`, `honest`,
`genuine`, `deliberate`, `fair`, `the reason`, `which is why`, `the trade`,
`so that nobody`), the dash-joined afterthought that denies what the reader
never proposed, and the conditions of fairness folded into an adverbial
(`in the same run`, `paired`, `interleaved`, `on the same machine`,
`under a real config`). A comparison that belongs on a page is a table with the
other column beside view's; the prose beside it says nothing about how the
numbers were taken, and the method lives in `docs/benchmarking.md`.

```
Before:  Every performance claim is a moment you live through, measured
         paired against bare Neovim in the same run, so nothing here
         borrows a fixture number.
After:   Launch, keypress and scroll are measured with your config loaded.
```

The em dash and its two-hyphen stand-in are not sentence joiners. One thought
per sentence and a full stop between them.

## A rule page states its rule and its reason once

The pages under `.claude/rules/` are read the same way, with one addition: each
rule keeps its own reason, in a sentence or two, because a rule whose reason is
gone is a rule the next session deletes. A shipped defect that grounds a rule is
worth one sentence of fact. Everything past that is cut: the argument for the
design, the objection answered before a reader raised it, and the retelling of
how the defect was found.

`check_prose_frames` in `scripts/check-style.sh` greps `README.md`, every page
under `docs/` and every page under `.claude/rules/` for the frame, the tell
words and the joiner as a tripwire. A page that defines one of those spellings
writes it in backticks, which is how the walk tells a sample of text from prose.
Every table cell is read as a sentence of its own, apart from the separator row,
and every line is read with the line under it joined on, so a spelling that sits
across the 80-character margin is graded where it starts. A dash with nothing on
one side of it is the placeholder a generated table writes for an empty column.
The gate finds the spellings it knows; the two paragraphs above are the rule.
