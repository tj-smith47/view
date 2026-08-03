# A boundary predicate that could not match any phrase containing a space

Found 2026-07-26 while splitting the first-paint metric. Engine pin
`v0.12.4`. Host: dev-linux.

## The defect

`crates/view-bench/src/boundaries.rs::row_text` built a row's text by
concatenating `cell.contents()` for each column. A terminal cell that was
never written holds *no contents at all*, not a space, and a diffing
painter never writes a space it can leave blank. So every space vanished
from the reconstructed row, and `screen_holds` silently failed against any
needle containing one.

Observed directly, from the harness, on a screen plainly showing the text:

```
needle  = "view: waiting for nvim..."
row 20  = "view: waiting for nvim..."     (vt100's own contents())
row_text= "view:waitingfornvim..."        (what the predicate compared)
hit     = false
```

The failure was silent in the worst direction: not a wrong number, but a
boundary that never fires, which the scenario then reports as the event
never having happened.

## Why it survived until now

Every marker in the tree is a single word. `VIEWBENCHCOLDSTARTMARKER`,
`L000042`, the `~` empty-buffer glyph: none has a space to lose, so no
existing row could observe the bug. It surfaced the moment a boundary
needed real prose, and it would have surfaced the same way for any future
one.

## Confirmed not to have corrupted a recorded metric

- `first_paint` matched `VIEWBENCHCOLDSTARTMARKER`: no space, unaffected.
- `scroll` matched an `L%06d` label through its own copy of the same
  concatenation: no space, unaffected. It now routes through the fixed
  helper rather than keeping a second copy of the rule.
- `flood` built whole-screen text and read the largest `cat -n` counter
  out of it with `max_screen_line`, which splits on *any* non-digit. Both
  `"   1201 y"` and `"1201y"` yield 1201, so the drain meter reads the
  same number either way and the recorded `cadence_p99_ms` / `pace_ratio`
  do not move. Cadence itself is detected by `screen_hash`, per cell, and
  never touched `row_text`.

## The fix

`row_text` now renders one character per column, matching vt100's own row
renderer: a cell with contents contributes them, a cell without
contributes the space it displays, and a wide glyph's continuation column
contributes nothing (its glyph already came from the preceding column).

Two regression tests, both watched failing against the old body:

- `a_column_the_painter_skipped_reads_back_as_the_space_it_shows` writes
  `two` and `words` with an absolute cursor move over the gap, exactly as
  the real painter emits it, and asserts both the reconstructed row and a
  `screen_holds` match. Against the old body the row reads `"twowords"`
  and the match fails.
- `a_wide_glyphs_continuation_column_adds_no_character` pins the other
  direction, so the blank-cell fix cannot be implemented as a blanket
  `""` to `" "` mapping that pads a space after every wide character.
