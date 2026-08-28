//! Moving a list item around: up and down its list by dragging the gutter grip,
//! and in and out a level with tab / shift-tab.
//!
//! Both are the same problem — work out which lines belong to the item, then
//! rewrite that region — so they share the scanners below.
//!
//! All of it is plain text work over source line indices, so the decisions that
//! are easy to get wrong — a parent carrying its children, where a run of
//! siblings stops, what becomes of the blank lines in a loose list — are unit
//! testable without a window or a layout. The element only supplies the
//! pointer's y and paints the outcome.

use std::ops::Range;

use crate::{line_start, line_str, parse_list_line};

/// A run of sibling list items: the items themselves in document order, plus
/// the blank lines that separate them.
///
/// `seps[i]` is the (possibly empty) run of blank lines between `items[i]` and
/// `items[i + 1]`. It stays at that slot when the items are reordered, so a
/// loose list keeps its blank lines *between* items rather than dragging them
/// around with whichever item happened to precede them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    /// Source line range of each item: its marker line plus everything indented
    /// underneath it — nested items and continuation lines — which is what
    /// makes a parent carry its children.
    pub(crate) items: Vec<Range<usize>>,
    pub(crate) seps: Vec<Range<usize>>,
}

impl Run {
    /// The source lines the run occupies, from its first item's marker to the
    /// end of its last item.
    pub(crate) fn region(&self) -> Range<usize> {
        match (self.items.first(), self.items.last()) {
            (Some(f), Some(l)) => f.start..l.end,
            _ => 0..0,
        }
    }
}

/// An in-flight grip drag: the run of siblings captured when the item was
/// grabbed, which of them is moving, and the slot it would land in if the
/// button were released now.
///
/// The run is captured once rather than rescanned every frame, so the gaps
/// can't shift under the pointer mid-drag; it is re-derived from the text and
/// compared before the edit lands (see `MarkdownEditor::move_list_item`).
#[derive(Clone)]
pub(crate) struct GripDrag {
    pub(crate) run: Run,
    pub(crate) from: usize,
    pub(crate) to: usize,
}

/// The indent of a movable list item, or `None` if `line` isn't one.
///
/// Movable means a `-`/`*`/`+` bullet, with or without a task box. Ordered
/// items are deliberately excluded: moving one would have to renumber the rest
/// of the run, which is a separate decision about the user's text.
///
/// Nothing here rules out a thematic break, because nothing has to: a rule is
/// an unbroken run of `-` and an item needs a space after its marker, so no
/// line is both. What a line is *rendered* as stays the renderer's call —
/// [`VisualLine::marker_x`] defers to its `hr`, `code_bg` and `table` flags.
pub(crate) fn item_indent(line: &str) -> Option<usize> {
    Some(parse_list_line(line)?.marker.start)
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn blank(text: &str, line: usize) -> bool {
    line_str(text, line).trim().is_empty()
}

fn line_count(text: &str) -> usize {
    text.split('\n').count()
}

/// The lines that move with the item whose marker is on `line`: everything
/// below it indented deeper than its marker, stopping at a blank line or at
/// anything indented level with it (or less).
fn item_lines(text: &str, line: usize, indent: usize, n: usize) -> Range<usize> {
    let mut end = line + 1;
    while end < n {
        let l = line_str(text, end);
        if l.trim().is_empty() || indent_of(l) <= indent {
            break;
        }
        end += 1;
    }
    line..end
}

/// The marker line of the sibling directly above the item starting at `start`,
/// stepping back over that sibling's own children.
fn prev_sibling(text: &str, start: usize, indent: usize, n: usize) -> Option<usize> {
    // Blank lines between two items belong to neither; skip them to find where
    // the item above has to end.
    let mut above = start;
    while above > 0 && blank(text, above - 1) {
        above -= 1;
    }
    let mut j = above.checked_sub(1)?;
    while indent_of(line_str(text, j)) > indent && !blank(text, j) {
        j = j.checked_sub(1)?;
    }
    // It is a sibling only if it is an item at our own indent *and* its block
    // runs right up to the gap — anything else between the two (a paragraph, a
    // blank line inside its children) means they aren't adjacent siblings.
    (item_indent(line_str(text, j)) == Some(indent)
        && item_lines(text, j, indent, n).end == above)
        .then_some(j)
}

/// The blank lines after the item starting at `start`, and the marker line of
/// the sibling that follows them.
fn next_sibling(
    text: &str,
    start: usize,
    indent: usize,
    n: usize,
) -> Option<(Range<usize>, usize)> {
    let end = item_lines(text, start, indent, n).end;
    let mut k = end;
    while k < n && blank(text, k) {
        k += 1;
    }
    (k < n && item_indent(line_str(text, k)) == Some(indent)).then_some((end..k, k))
}

/// The run of siblings around the item whose marker is on `line`, and that
/// item's index within it. `None` if the line isn't a movable list item.
pub(crate) fn sibling_run(text: &str, line: usize) -> Option<(Run, usize)> {
    let indent = item_indent(line_str(text, line))?;
    let n = line_count(text);
    // Walk up to the first sibling and collect the run downward from there, so
    // the item indices are in document order however far in you grabbed.
    let mut first = line;
    while let Some(p) = prev_sibling(text, first, indent, n) {
        first = p;
    }
    let mut items = vec![item_lines(text, first, indent, n)];
    let mut seps = Vec::new();
    let mut at = first;
    while let Some((sep, next)) = next_sibling(text, at, indent, n) {
        seps.push(sep);
        items.push(item_lines(text, next, indent, n));
        at = next;
    }
    let ix = items.iter().position(|r| r.start == line)?;
    Some((Run { items, seps }, ix))
}

/// The run's lines in the order they take once the item at `from` moves to
/// `to` — an index into the run *with that item removed*, which is the natural
/// way to name a gap between the remaining items.
pub(crate) fn reordered(run: &Run, from: usize, to: usize) -> Vec<usize> {
    let mut order: Vec<usize> = (0..run.items.len()).collect();
    if from >= order.len() {
        return Vec::new();
    }
    let moved = order.remove(from);
    order.insert(to.min(order.len()), moved);
    let mut out = Vec::new();
    for (slot, &ix) in order.iter().enumerate() {
        out.extend(run.items[ix].clone());
        // The separators keep their slots: the last item has none after it.
        if let Some(sep) = run.seps.get(slot) {
            out.extend(sep.clone());
        }
    }
    out
}

/// The edit that moves item `from` to slot `to`: the source range to replace,
/// the text to put there, and the new order of the region's lines. `None` when
/// the move would change nothing.
///
/// The replacement is a permutation of the region's own lines, so it is exactly
/// as long as what it replaces. That is what lets the caret be carried across
/// by line number, and leaves every offset after the region untouched.
pub(crate) fn reorder_edit(
    text: &str,
    run: &Run,
    from: usize,
    to: usize,
) -> Option<(Range<usize>, String, Vec<usize>)> {
    let region = run.region();
    let order = reordered(run, from, to);
    // A run whose items and separators don't tile its region would drop or
    // duplicate lines; refuse rather than rewrite the document wrongly.
    if order.len() != region.len() {
        return None;
    }
    if order.iter().enumerate().all(|(i, &l)| l == region.start + i) {
        return None; // the item is already there
    }
    let new = order
        .iter()
        .map(|&l| line_str(text, l))
        .collect::<Vec<_>>()
        .join("\n");
    Some((block_range(text, &region), new, order))
}

/// One level of list indent. Two spaces: it matches what tab already inserted
/// before it demoted items, and it puts a child's marker exactly at its
/// parent's content column, which is what makes it a nested list rather than a
/// stray indent.
pub(crate) const INDENT: &str = "  ";

/// The edit that demotes the item whose marker is on `line`, making it a child
/// of the item above: the range to replace, the text to put there, and how far
/// the line moved right (the caret rides along).
///
/// `None` unless there is a sibling above to become a child *of* — the first
/// item of a list has no parent to join, and indenting it would leave a stray
/// indent rather than a sub-list.
pub(crate) fn indent_edit(text: &str, line: usize) -> Option<(Range<usize>, String, usize)> {
    let indent = item_indent(line_str(text, line))?;
    let n = line_count(text);
    prev_sibling(text, line, indent, n)?;
    let block = item_lines(text, line, indent, n);
    // Everything nested under the item shifts with it, so the shape underneath
    // survives the move.
    let new = block
        .clone()
        .map(|l| format!("{INDENT}{}", line_str(text, l)))
        .collect::<Vec<_>>()
        .join("\n");
    Some((block_range(text, &block), new, INDENT.len()))
}

/// The inverse: promote the item (and its children) one level. `None` for an
/// item already at the left margin, which has nowhere left to go.
pub(crate) fn outdent_edit(text: &str, line: usize) -> Option<(Range<usize>, String, usize)> {
    let indent = item_indent(line_str(text, line))?;
    if indent == 0 {
        return None;
    }
    let by = INDENT.len().min(indent);
    let n = line_count(text);
    let block = item_lines(text, line, indent, n);
    let new = block
        .clone()
        .map(|l| unindent(line_str(text, l), by))
        .collect::<Vec<_>>()
        .join("\n");
    Some((block_range(text, &block), new, by))
}

/// Drop up to `by` bytes of leading whitespace.
fn unindent(line: &str, by: usize) -> &str {
    let ws = line.len() - line.trim_start().len();
    &line[ws.min(by)..]
}

/// The source range a block of lines occupies, excluding the newline that ends
/// the last of them — so a replacement can't disturb what follows.
fn block_range(text: &str, block: &Range<usize>) -> Range<usize> {
    let last = block.end - 1;
    line_start(text, block.start)..line_start(text, last) + line_str(text, last).len()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The run around the item on `line`, as the item texts in order — the
    /// shape of a reorder without the offsets.
    fn run_of(text: &str, line: usize) -> (Vec<Vec<&str>>, usize) {
        let (run, ix) = sibling_run(text, line).expect("a movable item");
        let items = run
            .items
            .iter()
            .map(|r| r.clone().map(|l| line_str(text, l)).collect())
            .collect();
        (items, ix)
    }

    /// Apply a move and return the whole document, so a test reads as the text
    /// before and after.
    fn moved(text: &str, line: usize, to: usize) -> String {
        let (run, from) = sibling_run(text, line).expect("a movable item");
        let Some((range, new, _)) = reorder_edit(text, &run, from, to) else {
            return text.to_string();
        };
        let mut out = text.to_string();
        out.replace_range(range, &new);
        out
    }

    #[test]
    fn a_run_is_the_items_at_one_indent() {
        let text = "intro\n- a\n- b\n- c\nafter\n";
        let (items, ix) = run_of(text, 2);
        assert_eq!(items, vec![vec!["- a"], vec!["- b"], vec!["- c"]]);
        assert_eq!(ix, 1, "grabbed the second item");
    }

    #[test]
    fn an_item_carries_its_nested_children() {
        let text = "- a\n  - a1\n  - a2\n- b\n";
        let (items, _) = run_of(text, 0);
        assert_eq!(items, vec![vec!["- a", "  - a1", "  - a2"], vec!["- b"]]);
        assert_eq!(moved(text, 0, 1), "- b\n- a\n  - a1\n  - a2\n");
    }

    #[test]
    fn a_child_moves_among_its_own_siblings() {
        let text = "- a\n  - a1\n  - a2\n- b\n";
        // Grabbing a child gives a run of children, not of top-level items.
        let (items, ix) = run_of(text, 2);
        assert_eq!(items, vec![vec!["  - a1"], vec!["  - a2"]]);
        assert_eq!(ix, 1);
        assert_eq!(moved(text, 2, 0), "- a\n  - a2\n  - a1\n- b\n");
    }

    #[test]
    fn a_task_item_moves_like_any_other() {
        let text = "- [ ] wash up\n- [x] shop\n- [ ] cook\n";
        assert_eq!(
            moved(text, 2, 0),
            "- [ ] cook\n- [ ] wash up\n- [x] shop\n",
            "the last item dropped at the top"
        );
    }

    #[test]
    fn moving_down_lands_after_the_item_it_passed() {
        let text = "- a\n- b\n- c\n";
        // Slots name gaps in the list without `a`: 0 before b, 1 between, 2 after c.
        assert_eq!(moved(text, 0, 1), "- b\n- a\n- c\n");
        assert_eq!(moved(text, 0, 2), "- b\n- c\n- a\n");
    }

    #[test]
    fn a_no_op_move_makes_no_edit() {
        let text = "- a\n- b\n- c\n";
        let (run, from) = sibling_run(text, 1).unwrap();
        assert_eq!(from, 1);
        assert!(reorder_edit(text, &run, from, 1).is_none(), "back where it was");
    }

    #[test]
    fn blank_lines_stay_between_the_items() {
        // A loose list: the separators are part of the list's shape, not of the
        // item above them, so they must not travel with it.
        let text = "- a\n\n- b\n\n- c\n";
        let (run, _) = sibling_run(text, 0).unwrap();
        assert_eq!(run.items.len(), 3);
        assert_eq!(run.seps, vec![1..2, 3..4]);
        assert_eq!(moved(text, 0, 2), "- b\n\n- c\n\n- a\n");
    }

    #[test]
    fn a_run_stops_at_anything_that_is_not_a_sibling() {
        let text = "- a\n- b\n\nsome prose\n\n- c\n- d\n";
        let (items, ix) = run_of(text, 5);
        assert_eq!(items, vec![vec!["- c"], vec!["- d"]], "the prose splits the list");
        assert_eq!(ix, 0);
    }

    #[test]
    fn an_ordered_item_is_not_movable_and_bounds_the_run() {
        // Reordering these would mean renumbering them, so they are left alone
        // — and a bullet next to one can't be dragged across it either.
        assert_eq!(item_indent("1. first"), None);
        let text = "- a\n1. two\n- b\n";
        let (items, _) = run_of(text, 0);
        assert_eq!(items, vec![vec!["- a"]]);
    }

    #[test]
    fn the_marker_has_to_be_a_marker() {
        assert_eq!(item_indent("- a"), Some(0));
        assert_eq!(item_indent("  - a"), Some(2));
        assert_eq!(item_indent("* a"), Some(0));
        assert_eq!(item_indent("+ a"), Some(0));
        assert_eq!(item_indent("-no space"), None);
        assert_eq!(item_indent("---"), None, "a thematic break, not an item");
        // `- - -` is a rule in some markdown flavours, but not to this parser
        // (`is_hr` wants an unbroken run) — it renders as a bullet with `- -`
        // in it, so it is a real item here and moves like one.
        assert_eq!(item_indent("- - -"), Some(0));
    }

    #[test]
    fn the_last_line_of_the_document_needs_no_trailing_newline() {
        let text = "- a\n- b";
        assert_eq!(moved(text, 0, 1), "- b\n- a");
    }

    /// Apply an indent (or outdent) and return the whole document.
    fn nested(text: &str, line: usize, out: bool) -> String {
        let edit = if out { outdent_edit(text, line) } else { indent_edit(text, line) };
        let Some((range, new, _)) = edit else { return text.to_string() };
        let mut o = text.to_string();
        o.replace_range(range, &new);
        o
    }

    #[test]
    fn tab_makes_an_item_a_child_of_the_one_above() {
        let text = "- a\n- b\n";
        assert_eq!(nested(text, 1, false), "- a\n  - b\n");
        // And again, one level deeper, now under the item it just joined.
        assert_eq!(nested("- a\n  - b\n  - c\n", 2, false), "- a\n  - b\n    - c\n");
    }

    #[test]
    fn the_fresh_item_enter_just_made_can_be_demoted() {
        // The flow this exists for: enter leaves an empty item below, tab turns
        // it into a child of the one it came from.
        assert_eq!(nested("- a\n- ", 1, false), "- a\n  - ");
    }

    #[test]
    fn a_first_item_has_nothing_to_be_a_child_of() {
        assert!(indent_edit("- a\n- b\n", 0).is_none());
        // Nor has the first child of a nested list.
        assert!(indent_edit("- a\n  - b\n  - c\n", 1).is_none());
    }

    #[test]
    fn indenting_carries_the_children_along() {
        let text = "- a\n- b\n  - b1\n  - b2\n";
        assert_eq!(nested(text, 1, false), "- a\n  - b\n    - b1\n    - b2\n");
    }

    #[test]
    fn a_task_item_indents_like_any_other() {
        assert_eq!(nested("- [ ] a\n- [x] b\n", 1, false), "- [ ] a\n  - [x] b\n");
    }

    #[test]
    fn shift_tab_promotes_an_item_and_its_children() {
        let text = "- a\n  - b\n    - b1\n";
        assert_eq!(nested(text, 1, true), "- a\n- b\n  - b1\n");
    }

    #[test]
    fn a_top_level_item_cannot_be_promoted() {
        assert!(outdent_edit("- a\n- b\n", 1).is_none());
    }

    #[test]
    fn only_a_list_item_is_re_nested_at_all() {
        // Ordinary prose, and an ordered item (which would need renumbering).
        assert!(indent_edit("some prose\n- b\n", 0).is_none());
        assert!(indent_edit("1. one\n2. two\n", 1).is_none());
    }

    #[test]
    fn the_shift_is_what_the_caret_should_follow() {
        let (_, _, shift) = indent_edit("- a\n- b\n", 1).unwrap();
        assert_eq!(shift, 2);
        let (_, _, shift) = outdent_edit("- a\n  - b\n", 1).unwrap();
        assert_eq!(shift, 2);
        // An odd indent gives back only what is there.
        let (_, _, shift) = outdent_edit("- a\n - b\n", 1).unwrap();
        assert_eq!(shift, 1);
    }

    #[test]
    fn the_edit_is_a_permutation_so_the_region_keeps_its_length() {
        let text = "- a\n  - a1\n\n- b\n- c\n";
        let (run, from) = sibling_run(text, 0).unwrap();
        let (range, new, order) = reorder_edit(text, &run, from, 2).unwrap();
        assert_eq!(new.len(), range.len(), "same bytes, different order");
        let mut sorted = order.clone();
        sorted.sort_unstable();
        assert_eq!(sorted, (0..5).collect::<Vec<_>>(), "every line, exactly once");
    }
}
