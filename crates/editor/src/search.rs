//! Find within the open note: where a query matches, and which match the view
//! is pointing at.
//!
//! The host owns the search bar itself — the field, the counter, the arrows —
//! and this owns what a query *means*: the match set, the current match, and
//! keeping both true while the note is edited under them.
//!
//! Highlighting is paint-only. A match set never changes what a line looks like,
//! so a query costs one scan and a repaint: no reparse, no relayout, no
//! reshaping (see `element.rs`, which reads the set in `finish`).

use std::ops::Range;
use std::rc::Rc;

use gpui::Context;

use crate::MarkdownEditor;

/// An open find.
pub(crate) struct Search {
    /// What was typed. Kept so an edit to the note can re-run it.
    query: String,
    /// Match ranges as document byte offsets, in order and non-overlapping.
    /// Behind an `Rc` because every painted frame takes the set with it.
    matches: Rc<Vec<Range<usize>>>,
    /// Index into `matches`, in range whenever there is a match at all.
    current: usize,
}

impl MarkdownEditor {
    /// Open the find, or re-run it with a new query.
    ///
    /// The current match becomes the first one at or after the caret, wrapping
    /// to the top when there is none — so typing into the bar walks forward
    /// from where the reader already is rather than jumping back to the start of
    /// the note.
    pub fn set_search_query(&mut self, query: &str, cx: &mut Context<Self>) {
        let matches = Rc::new(find_matches(&self.text, query));
        let current = at_or_after(&matches, self.cursor);
        self.search = Some(Search {
            query: query.to_string(),
            matches,
            current,
        });
        self.reveal_current_match(cx);
    }

    /// Step to the next or previous match, wrapping at both ends — a find that
    /// stops dead at the last match only makes you retype the query.
    pub fn step_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(search) = &mut self.search else { return };
        let len = search.matches.len();
        if len == 0 {
            return;
        }
        let delta = if forward { 1 } else { -1 };
        search.current = (search.current as isize + delta).rem_euclid(len as isize) as usize;
        self.reveal_current_match(cx);
    }

    /// Close the find, dropping the highlights. The caret stays on the match it
    /// had reached, which is where the reader was last looking.
    pub fn clear_search(&mut self, cx: &mut Context<Self>) {
        if self.search.take().is_some() {
            cx.notify();
        }
    }

    /// `(the current match's one-based number, how many there are)` while the
    /// find is open — `(0, 0)` when nothing matches — and `None` when it is not.
    /// This is what the bar's counter shows.
    pub fn search_status(&self) -> Option<(usize, usize)> {
        let search = self.search.as_ref()?;
        let total = search.matches.len();
        Some((if total == 0 { 0 } else { search.current + 1 }, total))
    }

    /// Put the caret on the current match and scroll it into view.
    ///
    /// The caret rather than a selection: the match already has a highlight of
    /// its own, and a selection under it would only muddy the color. Moving it
    /// is also what makes closing the bar leave the reader *at* the match, and
    /// it reuses the scrolling every other caret move goes through.
    fn reveal_current_match(&mut self, cx: &mut Context<Self>) {
        if let Some(search) = &self.search {
            if let Some(m) = search.matches.get(search.current) {
                self.cursor = m.start;
                self.anchor = None;
                self.goal_x = None;
                self.scroll_to_caret = true;
            }
        }
        cx.notify();
    }

    /// Re-run the find after an edit. Called from `sync_text`, so every path
    /// that touches the buffer goes through it.
    ///
    /// Deliberately silent: it neither moves the caret nor scrolls, because the
    /// edit that triggered it has just put both where the typist wants them.
    pub(crate) fn refresh_search(&mut self) {
        let Some(query) = self.search.as_ref().map(|s| s.query.clone()) else {
            return;
        };
        let matches = find_matches(&self.text, &query);
        // The old index named a match that may no longer exist, so the current
        // match is picked afresh the way a new query picks it.
        let current = at_or_after(&matches, self.cursor);
        if let Some(search) = &mut self.search {
            search.matches = Rc::new(matches);
            search.current = current;
        }
    }

    /// The match set and the current index, for painting. `None` when the find
    /// is closed or matches nothing, which is what lets `paint` skip it.
    pub(crate) fn search_paint(&self) -> Option<(Rc<Vec<Range<usize>>>, usize)> {
        let search = self.search.as_ref()?;
        (!search.matches.is_empty()).then(|| (search.matches.clone(), search.current))
    }
}

/// The index of the first match starting at or after `offset`, or 0 — which is
/// both the wrap-around answer and the right answer for an empty set.
fn at_or_after(matches: &[Range<usize>], offset: usize) -> usize {
    let ix = matches.partition_point(|m| m.start < offset);
    if ix >= matches.len() { 0 } else { ix }
}

/// Every case-insensitive occurrence of `needle` in `hay`, in order and
/// non-overlapping. Empty for an empty needle.
///
/// Case folding is per character rather than lowercasing the two strings and
/// searching those: `str::to_lowercase` can change a character's byte length
/// (`İ` folds to two code points), and the ranges this returns index the
/// *original* text — one wrong offset there would highlight, and place the
/// caret at, the wrong place.
///
/// The scan is the naive O(n·m) one. A note is not a corpus and a query is a
/// few characters, so the whole document is scanned in well under the frame
/// this runs in — the same reasoning the note finder's ranking uses.
pub(crate) fn find_matches(hay: &str, needle: &str) -> Vec<Range<usize>> {
    let needle: Vec<char> = needle.chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut at = 0;
    while at < hay.len() {
        if let Some(end) = match_at(hay, at, &needle) {
            out.push(at..end);
            at = end;
            continue;
        }
        // `at` is always on a character boundary, so this slice is safe and the
        // next one is too.
        at += hay[at..].chars().next().map_or(1, char::len_utf8);
    }
    out
}

/// Where the match starting at `start` ends, or `None` if there isn't one.
/// `needle` is already folded.
fn match_at(hay: &str, start: usize, needle: &[char]) -> Option<usize> {
    let mut ni = 0;
    let mut end = start;
    for c in hay[start..].chars() {
        // One character can fold to several (`ẞ` to `ss`); every one of them has
        // to line up, so a fold running past the end of the needle is not a
        // match rather than a partial one.
        for folded in c.to_lowercase() {
            if needle.get(ni) != Some(&folded) {
                return None;
            }
            ni += 1;
        }
        end += c.len_utf8();
        if ni == needle.len() {
            return Some(end);
        }
    }
    None
}

/// The matches lying in the byte range `start..end`, each with its index in the
/// set — which is what says whether it is the current one.
///
/// A binary search in and a walk until the first match past the end, so painting
/// a line costs the matches *on* it rather than a pass over every match in the
/// note.
pub(crate) fn search_in(
    matches: &[Range<usize>],
    start: usize,
    end: usize,
) -> impl Iterator<Item = (usize, &Range<usize>)> {
    let first = matches.partition_point(|m| m.end <= start);
    matches[first..]
        .iter()
        .take_while(move |m| m.start < end)
        .enumerate()
        .map(move |(i, m)| (first + i, m))
}
