//! Picking the selected row out of a menu that carries no selection index,
//! from how its rows are painted.

use std::collections::HashSet;

use view_core::native::views::{Span, StyleRole};

/// The one row of `rows` whose highlights set it apart from every other row:
/// how a menu that carries no selection index shows its selected row. `None`
/// when no single row stands out, or there are fewer than three rows to tell
/// it by.
pub(super) fn standout_row(rows: &[Vec<Span>]) -> Option<usize> {
    if rows.len() < 3 {
        return None;
    }
    let sets: Vec<HashSet<StyleRole>> = rows
        .iter()
        .map(|row| row.iter().map(|span| span.role).collect())
        .collect();
    let selection = |index: usize| stands_apart(rows, &sets, index).then_some(index);
    painted_apart(rows, &sets).or_else(|| alone_in_its_highlights(&sets).and_then(selection))
}

/// Whether `count` of a row's `total` cells set it apart enough to be a
/// selection: more than half the row, and at least three cells.
///
/// A row set apart in a cell or two, as a kind glyph sets one, or a row two
/// cells wide painted in its kind's own colour, is no selection.
fn more_than_half_and_at_least_three(count: usize, total: usize) -> bool {
    count * 2 > total && count >= 3
}

fn cells(span: &Span) -> usize {
    span.text.chars().count()
}

/// Whether row `index` is painted apart enough to be a selection: the
/// cells whose highlights no other row carries are enough of the row.
///
/// The selected row keeps its kind glyph's highlight, which every row of
/// that kind carries, so a bound that asked every cell to be the row's own
/// would never find the selection.
fn stands_apart(rows: &[Vec<Span>], sets: &[HashSet<StyleRole>], index: usize) -> bool {
    let alone = |span: &Span| {
        sets.iter()
            .enumerate()
            .all(|(other, set)| other == index || !set.contains(&span.role))
    };
    rows.get(index).is_some_and(|row| {
        let total: usize = row.iter().map(cells).sum();
        let own: usize = row.iter().filter(|span| alone(span)).map(cells).sum();
        more_than_half_and_at_least_three(own, total)
    })
}

/// The one row that [`stands_apart`], which is a selection painted across
/// its row.
fn painted_apart(rows: &[Vec<Span>], sets: &[HashSet<StyleRole>]) -> Option<usize> {
    let mut apart = (0..rows.len()).filter(|&index| stands_apart(rows, sets, index));
    let index = apart.next()?;
    apart.next().is_none().then_some(index)
}

/// The one row whose highlights differ from the set every other row has.
fn alone_in_its_highlights(sets: &[HashSet<StyleRole>]) -> Option<usize> {
    // two of the first three agree, and that set is every row's but the
    // selected one's
    let common = if sets.first()? == sets.get(1)? {
        sets.first()?
    } else {
        sets.get(2)?
    };
    let mut odd = sets.iter().enumerate().filter(|(_, set)| *set != common);
    let (index, _) = odd.next()?;
    odd.next().is_none().then_some(index)
}
