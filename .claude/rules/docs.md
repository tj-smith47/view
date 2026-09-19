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
