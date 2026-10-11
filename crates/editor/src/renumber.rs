//! Keeping numbered lists counting. When an edit adds an item to an ordered
//! list, takes one out, or moves one to another level, the items below it are
//! renumbered so the list still counts up one at a time.
//!
//! It is one pass over the edit itself — the text before it, the range
//! replaced and what replaced it — so Enter, backspace, delete, cut, paste and
//! typing over a selection all get it alike, and the renumbering lands in the
//! same edit, to be undone with it in one step.
//!
//! An item counts for as long as its line starts with its number, not just
//! while its marker parses: rubbing out `2. ` a character at a time leaves
//! `2.`, `2text` or `2` standing for the item, and the items below only move
//! up once the number is gone too. Typing an item works the same way round.
//!
//! A blank line opened up between two items splits the list there — pressing
//! Enter on an empty item midway, or at the start of an item — and the part
//! below starts again from 1. Removing that blank line joins them back, the
//! lower list carrying on the upper one's count. A list that already had
//! blank lines between its items (a loose list) counts straight through them.
//!
//! It stays out of the way of numbers written on purpose: it acts only when
//! the edit changed which lines carry a number or where the lines break
//! (retyping a number by hand is left alone), and it only renumbers a run that
//! counted up one at a time before the edit, so a list with `1.` on every
//! item, or one that skips, keeps the numbers it was given.

use std::ops::Range;

use crate::{classify_code, heading_prefix_len, is_hr, parse_list_line, quote_prefix};

/// Replacing `range` of `text` with `insert`, widened to renumber the ordered
/// lists below it: the range to replace, the text to put there, and how far
/// into that text the caret lands — the end of `insert`, since the
/// renumbering all lies past it.
pub(crate) fn renumber_edit(text: &str, range: Range<usize>, insert: &str) -> (Range<usize>, String, usize) {
    let mut out = insert.to_string();
    let valid = range.start <= range.end
        && range.end <= text.len()
        && text.is_char_boundary(range.start)
        && text.is_char_boundary(range.end);
    if !valid {
        // Not ours to judge: the buffer clamps a bad range itself.
        return (range, out, insert.len());
    }
    let mut end = range.end;
    for (r, new) in renumbering(text, range.clone(), insert) {
        out.push_str(&text[end..r.start]);
        out.push_str(&new);
        end = r.end;
    }
    (range.start..end, out, insert.len())
}

/// The renumbering that replacing `range` of `t0` with `insert` calls for, as
/// edits to `t0` past the end of `range`, in order.
fn renumbering(t0: &str, range: Range<usize>, insert: &str) -> Vec<(Range<usize>, String)> {
    // Whether the edit ends on a line break on both sides, leaving the line
    // after it untouched — as deleting whole lines does.
    let clean_end = bol(t0, range.end) == range.end
        && match insert.chars().last() {
            Some(c) => c == '\n',
            None => bol(t0, range.start) == range.start,
        };
    // The lines the edit touched, before and after it.
    let from = bol(t0, range.start);
    let to = if clean_end { range.end } else { eol(t0, range.end) };
    let after = [&t0[from..range.start], insert, &t0[range.end..to]].concat();
    let line_breaks = insert.contains('\n') || t0[range.clone()].contains('\n');
    if !line_breaks && items(&t0[from..to]) == items(&after) {
        return Vec::new();
    }
    // A numbered line in a code block is code.
    if classify_code(t0)[t0[..range.start].matches('\n').count()].in_block {
        return Vec::new();
    }

    let t1 = [&t0[..range.start], insert, &t0[range.end..]].concat();
    let change = Change {
        t0,
        t1: &t1,
        touched_end: to,
        item_removed: filled_items(&t0[from..to]) > filled_items(&after),
    };
    let t1_end = range.start + insert.len();
    // Past the edit the two texts agree, a fixed distance apart.
    let to_t0 = |p: usize| p - t1_end + range.end;
    let mut start = to - range.end + t1_end + usize::from(!clean_end);
    let mut prev_blank = start == 0 || t1[bol(&t1, start - 1)..start - 1].trim().is_empty();
    let mut edits = Vec::new();
    // Each list the edit sits in has its first item past the edit somewhere
    // below — a sub-list's before its parent's — and renumbering that item
    // carries the rest of its run along. Walk down until the margin holds
    // something that ends every list.
    while start <= t1.len() {
        let end = eol(&t1, start);
        let (at, line) = (start, &t1[start..end]);
        start = end + 1;
        if line.trim().is_empty() {
            prev_blank = true;
            continue;
        }
        let indent = indent_of(line);
        match numbered(line) {
            Some((_, n, delim)) => {
                if let Some(target) = new_number(&change, at, to_t0(at), n, indent, delim) {
                    let delta = target as i64 - n as i64;
                    let run = shift_run(&t1, at, delta);
                    edits.extend(run.into_iter().map(|(r, s)| (to_t0(r.start)..to_t0(r.end), s)));
                }
                if indent == 0 {
                    break;
                }
            }
            None if indent == 0 && (prev_blank || !paragraph_text(line)) => break,
            None => {}
        }
        prev_blank = false;
    }
    edits.sort_by_key(|(r, _)| r.start);
    edits
}

/// The edit being renumbered after: the text before and after it, where the
/// lines it touched end in `t0`, and whether it took out an item that had
/// something in it (see [`filled_items`]).
struct Change<'a> {
    t0: &'a str,
    t1: &'a str,
    touched_end: usize,
    item_removed: bool,
}

/// The number the item at `at` in `t1` (`at0` in `t0`) should now carry, if it
/// isn't the one it has.
///
/// If its list counted up into it, it carries on from the item now above it;
/// from the number its list started at, if the edit left it at the head of
/// one; or from 1, if a blank line now splits it off from the items above. If
/// a blank line kept it apart from the list above and the edit took that line
/// away, it carries on that list's count.
///
/// `None` unless it is the first item past the edit at its level.
fn new_number(c: &Change, at: usize, at0: usize, n: u64, indent: usize, delim: Option<u8>) -> Option<u64> {
    // A later item counts on from one that is past the edit too, and is
    // renumbered along with the first or not at all.
    let (p0, n0, gap0) =
        prev_sibling(c.t0, at0, indent, delim).filter(|&(p0, ..)| p0 < c.touched_end)?;
    let above = prev_sibling(c.t1, at, indent, delim);
    let target = if n0 + 1 == n {
        match above {
            // A blank line has opened up between it and the item above —
            // unless that blank is all that is left of an item taken out.
            Some((_, _, true)) if !gap0 && !c.item_removed => 1,
            Some((_, m, _)) => m + 1,
            None => list_start(c.t0, p0, n0, indent, delim),
        }
    } else {
        // Joining two lists: only one that counted up one at a time is
        // carried on from, so two lists numbered `1.` throughout stay so.
        let (p1, m, gap1) = above?;
        let counts_up = prev_sibling(c.t1, p1, indent, delim).is_none_or(|(_, k, _)| k + 1 == m);
        if !gap0 || gap1 || !counts_up {
            return None;
        }
        m + 1
    };
    (target != n).then_some(target)
}

/// The number the run counting up to the item at `at` (numbered `n`) starts
/// from.
fn list_start(text: &str, mut at: usize, mut n: u64, indent: usize, delim: Option<u8>) -> u64 {
    while let Some((p, m, _)) = prev_sibling(text, at, indent, delim) {
        if m + 1 != n {
            break;
        }
        (at, n) = (p, m);
    }
    n
}

/// Shift the item at `at`, and the siblings after it that count up from it,
/// by `delta`. Only proper items are rewritten; the run stops at any other
/// numbered line (see [`numbered`]).
fn shift_run(text: &str, at: usize, delta: i64) -> Vec<(Range<usize>, String)> {
    let line = &text[at..eol(text, at)];
    let Some((digits, mut prev, delim @ Some(_))) = numbered(line) else {
        return Vec::new();
    };
    let indent = indent_of(line);
    let shifted = |n: u64| n.saturating_add_signed(delta).to_string();
    let mut edits = vec![(at + digits.start..at + digits.end, shifted(prev))];
    let mut cur = at;
    while let Some((next, digits, n, d)) = next_sibling(text, cur, indent, delim) {
        if prev + 1 != n || d.is_none() {
            break;
        }
        edits.push((digits, shifted(n)));
        (cur, prev) = (next, n);
    }
    edits
}

/// The numbered item before the one whose line starts at `at`, in the same
/// list: its line start and number.
///
/// Blank lines and anything nested deeper are stepped over (a loose list is
/// still one list), and so is paragraph text hanging off the item above — a
/// lazy continuation line, which is what an item becomes once its number is
/// rubbed out. Anything else at the item's indent or shallower ends the list,
/// as does a change of delimiter, which starts a new one.
///
/// Also reports whether a blank line lies between the two.
fn prev_sibling(text: &str, at: usize, indent: usize, delim: Option<u8>) -> Option<(usize, u64, bool)> {
    let mut end = at;
    // Paragraph text has to hang straight off an item, with no blank between.
    let mut lazy = false;
    let mut gap = false;
    while end > 0 {
        let start = bol(text, end - 1);
        let line = &text[start..end - 1];
        end = start;
        if line.trim().is_empty() {
            if lazy {
                return None;
            }
            gap = true;
            continue;
        }
        let ind = indent_of(line);
        if ind > indent {
            continue;
        }
        if let Some((_, n, d)) = numbered(line) {
            return (ind == indent && same_list(d, delim)).then_some((start, n, gap));
        }
        if !paragraph_text(line) {
            return None;
        }
        lazy = true;
    }
    None
}

/// The numbered item after the one whose line starts at `at`, in the same list
/// (the mirror of [`prev_sibling`]): its line start, the range of its digits,
/// its number and its delimiter.
fn next_sibling(
    text: &str,
    at: usize,
    indent: usize,
    delim: Option<u8>,
) -> Option<(usize, Range<usize>, u64, Option<u8>)> {
    let mut start = eol(text, at) + 1;
    let mut prev_blank = false;
    while start <= text.len() {
        let end = eol(text, start);
        let (here, line) = (start, &text[start..end]);
        start = end + 1;
        if line.trim().is_empty() {
            prev_blank = true;
            continue;
        }
        let ind = indent_of(line);
        if ind > indent {
            prev_blank = false;
            continue;
        }
        if let Some((digits, n, d)) = numbered(line) {
            return (ind == indent && same_list(d, delim))
                .then_some((here, here + digits.start..here + digits.end, n, d));
        }
        if prev_blank || !paragraph_text(line) {
            return None;
        }
    }
    None
}

/// How many of `lines` hold an item with something in it: any numbered line
/// but an empty item (`3. `). Taking one of those out removes an item; an
/// empty one cleared away is how Enter ends a list.
fn filled_items(lines: &str) -> usize {
    lines
        .split('\n')
        .filter(|l| match numbered(l) {
            Some((digits, _, Some(_))) => !l[digits.end + 1..].trim().is_empty(),
            Some(_) => true,
            None => false,
        })
        .count()
}

/// The indents of the numbered lines among `lines`: what an edit has to change
/// for the numbering to need a look.
fn items(lines: &str) -> Vec<usize> {
    lines
        .split('\n')
        .filter(|l| numbered(l).is_some())
        .map(indent_of)
        .collect()
}

/// The number a line starts with: the byte range of its digits, their value,
/// and — for a proper item, `2. ` or `2) ` — its delimiter.
///
/// A line that starts with a number but isn't a proper item still holds an
/// item's place, since that is what is left of one being typed or rubbed out
/// (`2`, `2.`, `2.text`, `2text`). It is never rewritten, though: it may as
/// well be prose that happens to start with a number (`3D printing`).
fn numbered(line: &str) -> Option<(Range<usize>, u64, Option<u8>)> {
    let indent = indent_of(line);
    let len = line[indent..].bytes().take_while(u8::is_ascii_digit).count();
    // CommonMark caps a list number at nine digits; a longer run is text.
    if len == 0 || len > 9 {
        return None;
    }
    let digits = indent..indent + len;
    let n = line[digits.clone()].parse().ok()?;
    let delim = match &line.as_bytes()[digits.end..] {
        [d @ (b'.' | b')')] | [d @ (b'.' | b')'), b' ', ..] => Some(*d),
        _ => None,
    };
    Some((digits, n, delim))
}

/// Whether two numbered lines can be items of one list: a change between `.`
/// and `)` starts a new list, but a line that isn't a proper item has no
/// delimiter to disagree with.
fn same_list(a: Option<u8>, b: Option<u8>) -> bool {
    a.is_none() || b.is_none() || a == b
}

/// Whether `line`, which carries no number, is plain paragraph text — the
/// kind that can hang off the item above as a lazy continuation instead of
/// ending its list.
fn paragraph_text(line: &str) -> bool {
    parse_list_line(line).is_none()
        && heading_prefix_len(line).is_none()
        && quote_prefix(line).is_none()
        && !is_hr(line)
        && !line.trim_start().starts_with("```")
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// The start of the line holding byte `at`.
fn bol(text: &str, at: usize) -> usize {
    text[..at].rfind('\n').map_or(0, |i| i + 1)
}

/// The end of the line holding byte `at`, before its line break.
fn eol(text: &str, at: usize) -> usize {
    text[at..].find('\n').map_or(text.len(), |i| at + i)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replace the `«…»` selection in `before` — or insert at the `|` caret —
    /// with `insert`, and return the result with the caret marked `|`.
    fn edit(before: &str, insert: &str) -> String {
        let (text, range) = match (before.find('«'), before.find('»')) {
            (Some(s), Some(e)) => {
                let text = before.replacen('«', "", 1).replacen('»', "", 1);
                (text, s..e - '«'.len_utf8())
            }
            _ => {
                let at = before.find('|').expect("mark a caret or a selection");
                (before.replacen('|', "", 1), at..at)
            }
        };
        let (range, new, caret) = renumber_edit(&text, range, insert);
        let caret = range.start + caret;
        let mut out = text;
        out.replace_range(range, &new);
        out.insert(caret, '|');
        out
    }

    /// Backspace at the `|` caret, `times` times over.
    fn backspace(before: &str, times: usize) -> String {
        let mut s = before.to_string();
        for _ in 0..times {
            let at = s.find('|').unwrap();
            let prev = s[..at].char_indices().last().unwrap().0;
            s.insert(prev, '«');
            s = s.replacen('|', "»", 1);
            s = edit(&s, "");
        }
        s
    }

    #[test]
    fn deleting_items_renumbers_the_items_below() {
        // Whole lines selected and deleted, or cut.
        assert_eq!(edit("1. a\n«2. b\n»3. c\n4. d", ""), "1. a\n|2. c\n3. d");
        assert_eq!(edit("1. a«\n2. b»\n3. c", ""), "1. a|\n2. c");
        assert_eq!(edit("1. a\n«2. b\n3. c\n»4. d\n5. e", ""), "1. a\n|2. d\n3. e");
        // A selection across parts of items merges what is left of them.
        assert_eq!(edit("1. a«a\n2. b\n3. c»c\n4. d", ""), "1. a|c\n2. d");
        // The first item: the list keeps the number it started at.
        assert_eq!(edit("«1. a\n»2. b\n3. c", ""), "|1. b\n2. c");
        assert_eq!(edit("x\n\n«5. a\n»6. b", ""), "x\n\n|5. b");
        // Typing over the selection counts too.
        assert_eq!(edit("1. a\n«2. b\n»3. c", "x\n"), "1. a\nx\n|2. c");
    }

    #[test]
    fn backspacing_an_item_away_renumbers_once_its_number_is_gone() {
        // From the end of its text, through the marker, onto the line above:
        // the item below keeps its number while any of this one's is left.
        assert_eq!(backspace("1. a\n2. b|\n3. c", 2), "1. a\n2.|\n3. c");
        assert_eq!(backspace("1. a\n2. b|\n3. c", 3), "1. a\n2|\n3. c");
        assert_eq!(backspace("1. a\n2. b|\n3. c", 4), "1. a\n|\n2. c");
        assert_eq!(backspace("1. a\n2. b|\n3. c", 5), "1. a|\n2. c");
        // Rubbing out the marker in front of the text: nothing moves until
        // the digit goes, and typing the space back changes nothing either.
        let slip = backspace("1. a\n2. |b\n3. c", 1);
        assert_eq!(slip, "1. a\n2.|b\n3. c");
        assert_eq!(edit(&slip, " "), "1. a\n2. |b\n3. c");
        assert_eq!(backspace("1. a\n2. |b\n3. c", 2), "1. a\n2|b\n3. c");
        assert_eq!(backspace("1. a\n2. |b\n3. c", 3), "1. a\n|b\n2. c");
        // The first item too, keeping the number the list started at.
        assert_eq!(backspace("1. a|\n2. b\n3. c", 4), "|\n1. b\n2. c");
    }

    #[test]
    fn deleting_just_the_marker_removes_the_item() {
        // Its text stays, as a continuation of the item above.
        assert_eq!(edit("1. a\n«2. »b\n3. c", ""), "1. a\n|b\n2. c");
    }

    #[test]
    fn typing_a_number_adds_an_item() {
        assert_eq!(edit("1. a\n|\n2. b", "2"), "1. a\n2|\n3. b");
        // ...and the rest of the marker doesn't count it twice.
        assert_eq!(edit("1. a\n2|\n3. b", "."), "1. a\n2.|\n3. b");
        assert_eq!(edit("1. a\n2.|\n3. b", " "), "1. a\n2. |\n3. b");
    }

    #[test]
    fn prose_that_starts_with_a_number_is_never_rewritten() {
        // Under a list it may be an item half rubbed out, so it keeps the run
        // where it is rather than be renumbered as one.
        for prose in ["3D printing", "3.5 dl milk", "3rd", "3 eggs"] {
            assert_eq!(
                edit(&format!("1. a\n«2. b\n»{prose}\n4. d"), ""),
                format!("1. a\n|{prose}\n4. d"),
                "{prose}"
            );
        }
        // And a run of items being renumbered stops short of it.
        assert_eq!(
            edit("1. a\n«2. b\n»3. c\n4D printing\n5. e", ""),
            "1. a\n|2. c\n4D printing\n5. e"
        );
    }

    #[test]
    fn a_blank_line_opened_between_items_splits_the_list() {
        // A newline at the start of an item: the rest is a list of its own.
        assert_eq!(edit("1. a\n2. b\n|3. c\n4. d", "\n"), "1. a\n2. b\n\n|1. c\n2. d");
        // Wiping an item's text in one go leaves a blank where it was, but
        // that is an item removed, not a list split.
        assert_eq!(edit("1. a\n«2. b»\n3. c", ""), "1. a\n|\n2. c");
        // A loose list, already spaced out with blank lines, isn't split by
        // another one.
        assert_eq!(edit("1. a\n|\n2. b", "\n"), "1. a\n\n|\n2. b");
    }

    #[test]
    fn removing_the_blank_line_between_two_lists_joins_them() {
        let split = "1. a\n2. b\n\n|1. c\n2. d";
        assert_eq!(backspace(split, 1), "1. a\n2. b\n|3. c\n4. d");
        assert_eq!(edit("1. a\n2. b«\n»\n1. c\n2. d", ""), "1. a\n2. b|\n3. c\n4. d");
        // Two lists numbered `1.` throughout stay that way.
        assert_eq!(backspace("1. a\n1. b\n\n|1. c", 1), "1. a\n1. b\n|1. c");
        // Prose between them still keeps them apart.
        assert_eq!(backspace("1. a\n\ntext\n\n|1. c", 1), "1. a\n\ntext\n|1. c");
    }

    #[test]
    fn pasting_items_in_makes_room_below() {
        assert_eq!(edit("1. a\n|2. b", "2. x\n3. y\n"), "1. a\n2. x\n3. y\n|4. b");
    }

    #[test]
    fn nested_lists_renumber_on_their_own() {
        // Removing a sub-item renumbers the sub-list, not its parent's list.
        assert_eq!(
            edit("1. a\n   1. w\n«   2. x\n»   3. y\n2. b", ""),
            "1. a\n   1. w\n|   2. y\n2. b"
        );
        // Removing an item takes its sub-list with it.
        assert_eq!(edit("1. a\n«2. b\n   1. x\n»3. c", ""), "1. a\n|2. c");
        // Lines nested under a sibling are stepped over on the way down.
        assert_eq!(edit("«1. a\n»2. b\n   - x\n3. c", ""), "|1. b\n   - x\n2. c");
    }

    #[test]
    fn numbers_written_on_purpose_are_left_alone() {
        // `1.` on every item, or a deliberate skip.
        assert_eq!(edit("1. a\n«1. b\n»1. c", ""), "1. a\n|1. c");
        assert_eq!(edit("1. a\n«2. b\n»5. c", ""), "1. a\n|5. c");
        // Retyping a number by hand doesn't add or remove an item.
        assert_eq!(edit("1. a\n«2». b\n3. c", "5"), "1. a\n5|. b\n3. c");
        // A different list below — after prose, or with another delimiter —
        // counts on its own.
        assert_eq!(
            edit("1. a\n«2. b\n»\npara\n\n1. x\n2. y", ""),
            "1. a\n|\npara\n\n1. x\n2. y"
        );
        assert_eq!(edit("1. a\n«2. b\n»3) c", ""), "1. a\n|3) c");
        // Separate lists aren't counted as one when a deletion joins them.
        assert_eq!(
            edit("1. a\n2. b\n\npara\n\n«1. x\n»2. y", ""),
            "1. a\n2. b\n\npara\n\n|1. y"
        );
    }

    #[test]
    fn code_is_never_renumbered() {
        assert_eq!(edit("```\n1. a\n«2. b\n»3. c\n```", ""), "```\n1. a\n|3. c\n```");
    }

    #[test]
    fn edits_that_dont_touch_a_numbered_item_change_nothing_else() {
        assert_eq!(edit("1. a|\n2. b", "x"), "1. ax|\n2. b");
        assert_eq!(edit("1. a\n|\n2. b", "text"), "1. a\ntext|\n2. b");
        // An empty edit is a no-op.
        assert_eq!(edit("1. a|\n2. b", ""), "1. a|\n2. b");
    }
}
