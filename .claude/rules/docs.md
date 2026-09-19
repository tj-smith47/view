# User-facing page conventions

## The README lists what a person gets

`README.md` is read by someone deciding whether to try view. Every row of
its Features and Roadmap lists names a capability that person uses, in
their words, and nothing else. What does not belong there, whatever
streak produced it:

| shape | where it goes instead |
|---|---|
| a bug fix, or behaviour a person expects to be true (the engine dies with the terminal, `:qa!` quits) | the commit subject; `docs/` if a person has to know |
| internals (RPC seam, redraw path, multigrid, process supervision mechanics) | the spec, `docs/architecture` pages |
| evidence and receipts (oracle, fuzz harness, compat suite, benchmark matrix, "the build fails if…") | `docs/benchmarking.md`, `docs/performance.md` |
| a session note, a phase or streak name, a "landed in" reference | the plan ledger, `.claude/` |

A capability the tooling makes possible is stated as the capability: "your
plugins keep working" and never "compat suite with pinned plugin stacks".

The page earns its reader's attention with a small number of chosen
visuals (a screenshot, a tape or gif of a moment, a diagram for an idea
prose explains badly), each placed beside the feature it shows, never
as decoration. A row that needs a paragraph to explain is a row that
needs a picture or a cut.

Before a push, the Shipped and Landing lists are audited against this
table as a whole, not only the rows the streak touched.

## A page is written for a reader who has not doubted anything

A sentence on a page a person reads exists to say what is true or what
to do. Every other sentence, clause or word is removed: whatever
justifies, defends, anticipates an objection, argues for a design, or
denies an alternative. It is removed, never rewritten into another
shape, and a page that comes out thin is the right length.

The stance has many spellings and the words are only the visible ones:
the contrast frame ("X, not Y", "not X but Y", "rather than", "instead
of", "never Y", "isn't X, it's Y"), the tell words (`claim`, `prove`,
`honest`, `genuine`, `deliberate`, `fair`, `the reason`, `which is why`,
`the trade`, `so that nobody`), the dash-joined afterthought that denies
what the reader never proposed, and the conditions of fairness folded
into an adverbial (`in the same run`, `paired`, `interleaved`,
`on the same machine`, `under a real config`). A comparison that belongs on a
page is a table with the other column beside view's; the prose beside it
says nothing about how the numbers were taken, and the method lives in
`docs/benchmarking.md`.

```
Before:  Every performance claim is a moment you live through, measured
         paired against bare Neovim in the same run, so nothing here
         borrows a fixture number.
After:   Launch, keypress and scroll are measured with your config loaded.
```

The em dash and its two-hyphen stand-in are not sentence joiners. One
thought per sentence and a full stop between them.

`check_prose_frames` in `scripts/check-style.sh` greps `README.md` and
every page under `docs/` for the frame, the tell words and the joiner as
a tripwire. The gate finds the spellings it knows; the paragraph above
is the rule.
