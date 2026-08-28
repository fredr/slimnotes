//! The note finder: a fuzzy palette over every note in the vault, opened with
//! ctrl-p / cmd-p.
//!
//! Modelled on Zed's file finder — a query field over a ranked list, matched
//! characters highlighted, arrows and enter to pick, escape to leave — with the
//! two things a notes app wants on top: an empty query lists recently opened
//! notes, and a query that names nothing offers to create it.
//!
//! Everything here is synchronous. The candidate list is a flatten of the tree
//! the app already holds ([`vault::note_paths`], no I/O), and scoring the whole
//! vault costs microseconds at the scale a vault reaches — so there is no
//! background task, no debounce, and no stale-result bookkeeping to get wrong.
//! See [`score`] for the numbers.

use std::ops::Range;
use std::path::PathBuf;

use gpui::prelude::FluentBuilder;
use gpui::{
    actions, div, px, AnyElement, AppContext, Context, Entity, FocusHandle, FontWeight,
    HighlightStyle, InteractiveElement, IntoElement, MouseButton, ParentElement, ScrollHandle,
    SharedString, StatefulInteractiveElement, StyledText, Styled, Subscription, Window,
};
use gpui_component::{
    h_flex,
    input::{Input, InputEvent, InputState},
    v_flex, ActiveTheme, IconName, Sizable,
};
use slimnotes_vault::{self as vault};

use crate::SlimNotes;

/// The key context the finder's bindings are scoped to (bound in
/// [`crate::bind_keys`]). It sits on the panel, and the query field is a
/// descendant of it, so these fire while the field has focus.
pub(crate) const CONTEXT: &str = "NoteFinder";

// Move the selection, open the selected row, dismiss the finder.
actions!(slimnotes_finder, [SelectPrev, SelectNext, Confirm, Dismiss]);

/// How many recently opened notes to remember per vault.
pub(crate) const RECENT_NOTES: usize = 40;

/// How many rows the list will show. The list is not virtualized — every row is
/// a real element — so this is what keeps a query like "e" against a very large
/// vault from laying out thousands of rows. Ranking means the ones past the cap
/// are the ones nobody scrolls to.
const MAX_ROWS: usize = 100;

/// Panel geometry. The panel is a fixed width rather than a fraction of the
/// window so it looks the same whatever the window is doing, and hangs below the
/// top edge rather than centering vertically — a list that grows downwards from
/// a stable position is easier to aim at than one that recenters as it grows.
const PANEL_WIDTH: f32 = 600.;
const PANEL_TOP: f32 = 96.;
const ROW_HEIGHT: f32 = 30.;
/// A row's note name, and the dimmed folders after it. Set here rather than
/// inherited: the theme's default is for body text, which is larger than
/// anything else in the app's chrome, and a list wants to sit near the sidebar's
/// 13px rather than tower over it.
const NAME_FONT: f32 = 14.;
const FOLDERS_FONT: f32 = 12.;
/// Tall enough for eight rows; the list scrolls past that.
const LIST_MAX_HEIGHT: f32 = ROW_HEIGHT * 8.;

/// A note the finder can open, prepared once when the finder opens.
struct Candidate {
    /// Vault-relative path (`Projects/Alpha.md`) — what is matched and shown.
    rel: String,
    /// Absolute path, which is what opening a note needs.
    path: PathBuf,
    /// Byte offset in `rel` where the file name starts, just past the last
    /// separator, so a row can show and highlight the name and the folder it is
    /// in as two separate pieces.
    name_at: usize,
}

impl Candidate {
    /// The text a query is matched against, and the text a row shows: the path
    /// without its `.md` suffix.
    ///
    /// Every note in a vault has that suffix, so it distinguishes nothing.
    /// Matching it would let an "m" or a "d" anywhere in a query hit every note
    /// there is, and showing it would put an extension on every row that the
    /// sidebar does not put there (see `sidebar.rs`, which labels a note the
    /// same way).
    ///
    /// Offsets into this are also offsets into `rel` — only a suffix is missing
    /// — so the row can highlight what was matched without translating them.
    fn haystack(&self) -> &str {
        self.rel.strip_suffix(".md").unwrap_or(&self.rel)
    }

    /// The note's name on its own, which is what a row leads with.
    fn label(&self) -> &str {
        // `get` rather than an index: a name that is nothing *but* the suffix
        // would put `name_at` past the end, and a panic in render is not worth
        // risking over a note the tree does not show anyway.
        self.haystack().get(self.name_at..).unwrap_or("")
    }

    /// The folders the note sits in, `Projects/deep/`, or empty at the vault
    /// root. Trailing separator kept: it is what marks the text as a location
    /// rather than part of the name.
    fn folders(&self) -> &str {
        &self.rel[..self.name_at]
    }
}

/// A row in the list.
enum Row {
    /// A matching note: an index into [`NoteFinder::candidates`], and the byte
    /// offsets in its `rel` that the query matched.
    Note { ix: usize, positions: Vec<usize> },
    /// Create the note the query names, at this vault-relative path.
    Create { rel: String },
}

pub(crate) struct NoteFinder {
    /// The query field. A gpui-component `Input` rather than a hand-rolled one:
    /// it brings IME, selection and clipboard with it, and it is what the
    /// sidebar's rename row already uses.
    query: Entity<InputState>,
    /// Every note in the vault, in sidebar order.
    candidates: Vec<Candidate>,
    /// Candidate indices to list before anything is typed, most recent first.
    recent: Vec<usize>,
    rows: Vec<Row>,
    selected: usize,
    scroll: ScrollHandle,
    /// What had focus when the finder opened, restored when it closes — the
    /// editor, normally, so dismissing puts the caret back where it was.
    restore: Option<FocusHandle>,
    /// Keeps the query subscription alive for as long as the finder.
    _sub: Subscription,
}

impl SlimNotes {
    /// Open the finder, or close it if it is already open. Toggling is what the
    /// same key doing nothing the second time would feel like a bug.
    pub(crate) fn toggle_note_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.finder.is_some() {
            self.close_note_finder(window, cx);
        } else {
            self.open_note_finder(window, cx);
        }
    }

    fn open_note_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.vault.clone() else {
            return; // nothing to find, and the empty state already says so
        };

        // Candidates come from the tree, which is a complete walk of the vault
        // kept fresh by the watcher — so opening the finder reads no files, and
        // there is no long-lived list to keep in step with the disk. This one is
        // a snapshot for as long as the panel is up, which is the point: the
        // rows must not reshuffle under the selection because a sync client
        // touched something.
        let candidates: Vec<Candidate> = vault::note_paths(&self.tree, &root)
            .into_iter()
            .map(|rel| {
                let path = root.join(&rel);
                let rel = rel.to_string_lossy().into_owned();
                let name_at = rel.rfind('/').map(|i| i + 1).unwrap_or(0);
                Candidate { rel, path, name_at }
            })
            .collect();

        // Recents are stored per vault as absolute paths, and are matched up
        // against the candidates rather than tested on disk: a note that has
        // been deleted or renamed away simply isn't there, which is the same
        // answer a `is_file` check would give without touching the filesystem.
        // The open note is left out — the point of the first row is to be the
        // note you came from.
        let recent: Vec<usize> = self
            .settings
            .recent_notes
            .get(&crate::vault_key(&root))
            .into_iter()
            .flatten()
            .map(PathBuf::from)
            .filter(|p| Some(p.as_path()) != self.current.as_deref())
            .filter_map(|p| candidates.iter().position(|c| c.path == p))
            .collect();

        let query = cx.new(|cx| InputState::new(window, cx).placeholder("Search notes…"));
        let sub = cx.subscribe_in(
            &query,
            window,
            |this, _, event: &InputEvent, _, cx| {
                if let InputEvent::Change = event {
                    this.refilter_notes(cx);
                }
            },
        );

        self.finder = Some(NoteFinder {
            query: query.clone(),
            candidates,
            recent,
            rows: Vec::new(),
            selected: 0,
            scroll: ScrollHandle::new(),
            restore: window.focused(cx),
            _sub: sub,
        });
        self.refilter_notes(cx);
        query.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    pub(crate) fn close_note_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(finder) = self.finder.take() else { return };
        // Hand focus back, or to the root if there was nothing to hand it back
        // to — leaving focus on the dropped query field would leave the app with
        // no keyboard at all.
        match finder.restore {
            Some(handle) => handle.focus(window),
            None => self.focus_handle.focus(window),
        }
        cx.notify();
    }

    /// Rank the candidates against the query and rebuild the rows. Called on
    /// every keystroke; see [`score`] for why that is affordable synchronously.
    fn refilter_notes(&mut self, cx: &mut Context<Self>) {
        let Some(finder) = &mut self.finder else { return };
        let query = finder.query.read(cx).value().trim().to_string();

        if query.is_empty() {
            // Nothing typed: the recent notes, in the order they were opened.
            finder.rows = finder
                .recent
                .iter()
                .map(|&ix| Row::Note { ix, positions: Vec::new() })
                .collect();
        } else {
            let mut scored: Vec<(f64, usize, Vec<usize>)> = finder
                .candidates
                .iter()
                .enumerate()
                .filter_map(|(ix, c)| {
                    let s = score(&query, c.haystack())?;
                    Some((s.value, ix, s.positions))
                })
                .collect();
            // Best first, then by path so equal scores keep a stable order
            // instead of shuffling as the vault is rescanned.
            scored.sort_by(|a, b| {
                b.0.total_cmp(&a.0)
                    .then_with(|| finder.candidates[a.1].rel.cmp(&finder.candidates[b.1].rel))
            });
            scored.truncate(MAX_ROWS);
            finder.rows = scored
                .into_iter()
                .map(|(_, ix, positions)| Row::Note { ix, positions })
                .collect();

            // Offer to create what the query names, unless it names a note that
            // already exists — in which case that note is in the list already
            // and a second way to reach it would only be confusing.
            if let Some(rel) = new_note_path(&query) {
                let exists = finder
                    .candidates
                    .iter()
                    .any(|c| c.rel.eq_ignore_ascii_case(&rel));
                if !exists {
                    finder.rows.push(Row::Create { rel });
                }
            }
        }

        finder.selected = 0;
        finder.scroll.scroll_to_item(0);
        cx.notify();
    }

    /// Move the selection by `delta` rows, wrapping at both ends — the list is
    /// short enough that wrapping is quicker than reversing direction.
    fn move_finder_selection(&mut self, delta: isize, cx: &mut Context<Self>) {
        let Some(finder) = &mut self.finder else { return };
        let len = finder.rows.len();
        if len == 0 {
            return;
        }
        let next = (finder.selected as isize + delta).rem_euclid(len as isize) as usize;
        finder.selected = next;
        finder.scroll.scroll_to_item(next);
        cx.notify();
    }

    /// Open the selected row's note, creating it first for a create row.
    fn confirm_note_finder(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(finder) = &self.finder else { return };
        let Some(row) = finder.rows.get(finder.selected) else {
            return;
        };
        let action = match row {
            Row::Note { ix, .. } => finder.candidates[*ix].path.clone(),
            Row::Create { rel } => {
                let Some(root) = self.vault.clone() else { return };
                let path = root.join(rel);
                // `create_note` makes the parent folders too, so a query like
                // "Projects/New idea" creates the folder as well.
                if let Err(e) = vault::create_note(&path) {
                    self.status = Some(format!("Could not create note: {e}"));
                    cx.notify();
                    return;
                }
                self.reload_vault(cx);
                path
            }
        };
        self.close_note_finder(window, cx);
        // Reopening the note that is already open would reload it from disk for
        // no reason, and race the flush of its own unsaved edits.
        if self.current.as_deref() != Some(action.as_path()) {
            self.open_note(action, window, cx);
        }
    }

    pub(crate) fn on_toggle_note_finder(
        &mut self,
        _: &crate::ToggleNoteFinder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_note_finder(window, cx);
    }

    /// The finder, or nothing when it is closed. Rendered last and absolutely
    /// positioned, so it covers the window without disturbing the layout under
    /// it.
    pub(crate) fn render_note_finder(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let finder = self.finder.as_ref()?;
        let border = cx.theme().border;

        let list = if finder.rows.is_empty() {
            div()
                .h(px(ROW_HEIGHT))
                .flex()
                .items_center()
                .px(px(10.))
                .text_color(cx.theme().muted_foreground)
                .child(SharedString::from(if finder.candidates.is_empty() {
                    "No notes in this vault"
                } else {
                    "No notes match"
                }))
                .into_any_element()
        } else {
            div()
                .id(SharedString::from("finder-list"))
                .flex()
                .flex_col()
                .max_h(px(LIST_MAX_HEIGHT))
                .overflow_y_scroll()
                // The rows are this element's own children so
                // `ScrollHandle::scroll_to_item` can address them, which is
                // what keeps the selected row in view — the same arrangement
                // the sidebar's tree uses, and it needs the same
                // `flex_shrink_0` on each row.
                .track_scroll(&finder.scroll)
                .py(px(4.))
                .children(
                    finder
                        .rows
                        .iter()
                        .enumerate()
                        .map(|(ix, row)| self.render_finder_row(ix, row, finder, cx)),
                )
                .into_any_element()
        };

        let panel = v_flex()
            .w(px(PANEL_WIDTH))
            .max_w_full()
            .mt(px(PANEL_TOP))
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .border_1()
            .border_color(border)
            .rounded(px(8.))
            .shadow_lg()
            .overflow_hidden()
            // Stops a click on the panel from also reaching the editor
            // underneath, which places its caret from any mouse-down inside the
            // window rather than from its own hitbox. The panel's children are
            // painted after it and so run first, which is why this can be a
            // blanket stop: the row and the query field still get their clicks.
            .occlude()
            .on_any_mouse_down(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(border)
                    .child(
                        // `large` rather than the small size the sidebar's
                        // rename row uses: that one is a tree row and has to fit
                        // where a row fits, while this is the thing the eye lands
                        // on when the palette opens. It brings the padding and
                        // the 44px height with it.
                        Input::new(&finder.query)
                            .large()
                            .appearance(false)
                            .cleanable(false)
                            .prefix(
                                div()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(IconName::Search),
                            ),
                    ),
            )
            .child(list);

        Some(
            div()
                .absolute()
                .inset_0()
                .key_context(CONTEXT)
                .on_action(cx.listener(|this, _: &SelectPrev, _, cx| {
                    this.move_finder_selection(-1, cx)
                }))
                .on_action(cx.listener(|this, _: &SelectNext, _, cx| {
                    this.move_finder_selection(1, cx)
                }))
                .on_action(cx.listener(|this, _: &Confirm, window, cx| {
                    this.confirm_note_finder(window, cx)
                }))
                .on_action(cx.listener(|this, _: &Dismiss, window, cx| {
                    this.close_note_finder(window, cx)
                }))
                // Clicking away is the other way out. This only fires outside
                // the panel, which occludes it.
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| this.close_note_finder(window, cx)),
                )
                .flex()
                .justify_center()
                // Not the default `stretch`: the panel is as tall as its rows,
                // and stretching it would draw an empty box down to the bottom
                // of the window.
                .items_start()
                .child(panel)
                .into_any_element(),
        )
    }

    fn render_finder_row(
        &self,
        ix: usize,
        row: &Row,
        finder: &NoteFinder,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = ix == finder.selected;
        let muted = cx.theme().muted_foreground;
        let hit = HighlightStyle {
            color: Some(cx.theme().primary),
            // A weight the bundled family actually has a face for: asking for
            // `MEDIUM` gets whatever is nearest, which is not a decision worth
            // leaving to a fallback.
            font_weight: Some(FontWeight::SEMIBOLD),
            ..Default::default()
        };

        let body = match row {
            Row::Note { ix, positions } => {
                let c = &finder.candidates[*ix];
                // The name, then the folder it is in — dimmed and second, so a
                // column of rows reads as a list of note names.
                let name = c.label().to_string();
                let dir = c.folders().to_string();
                let name_hits: Vec<usize> = positions
                    .iter()
                    .filter(|p| **p >= c.name_at)
                    .map(|p| p - c.name_at)
                    .collect();
                let dir_hits: Vec<usize> =
                    positions.iter().copied().filter(|p| *p < c.name_at).collect();

                h_flex()
                    .min_w_0()
                    .items_center()
                    .gap(px(8.))
                    .text_size(px(NAME_FONT))
                    .child(
                        div().flex_none().child(
                            StyledText::new(SharedString::from(name.clone()))
                                .with_highlights(highlights(&name, &name_hits, hit)),
                        ),
                    )
                    .child(
                        div()
                            .min_w_0()
                            .flex_1()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(FOLDERS_FONT))
                            .text_color(muted)
                            .child(
                                StyledText::new(SharedString::from(dir.clone()))
                                    .with_highlights(highlights(&dir, &dir_hits, hit)),
                            ),
                    )
                    .into_any_element()
            }
            Row::Create { rel } => h_flex()
                .min_w_0()
                .items_center()
                .gap(px(8.))
                .text_size(px(NAME_FONT))
                .text_color(muted)
                .child(div().flex_none().child(IconName::Plus))
                .child(
                    div()
                        .min_w_0()
                        .overflow_hidden()
                        .text_ellipsis()
                        // Named the way the note will be labelled once it
                        // exists, not the file name it will be stored under.
                        .child(SharedString::from(format!(
                            "Create “{}”",
                            rel.strip_suffix(".md").unwrap_or(rel)
                        ))),
                )
                .into_any_element(),
        };

        let target = match row {
            Row::Note { ix, .. } => finder.candidates[*ix].path.to_string_lossy().into_owned(),
            Row::Create { rel } => format!("create:{rel}"),
        };

        div()
            .id(SharedString::from(format!("finder-row-{ix}-{target}")))
            .flex_shrink_0()
            .h(px(ROW_HEIGHT))
            .flex()
            .items_center()
            .mx(px(4.))
            .px(px(6.))
            .rounded(px(4.))
            .cursor_pointer()
            .when(selected, |this| {
                this.bg(cx.theme().sidebar_accent)
                    .text_color(cx.theme().sidebar_accent_foreground)
            })
            .when(!selected, |this| {
                this.hover(|s| s.bg(cx.theme().sidebar_accent.alpha(0.5)))
            })
            // A click selects the row and opens it in one go: the selection is
            // what `confirm` acts on, and moving it first keeps the two paths
            // (mouse and keyboard) doing the same thing.
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if let Some(finder) = &mut this.finder {
                        finder.selected = ix;
                    }
                    this.confirm_note_finder(window, cx);
                }),
            )
            .child(body)
            .into_any_element()
    }
}

/// The note a create row would make for `query`, vault-relative and with `.md`
/// appended, or `None` when the query cannot name one.
///
/// Slashes are allowed — creating `Projects/New idea` is the point — but a
/// component that is `.` or `..`, or starts with a dot, is not: those either
/// escape the vault or make a note the tree hides, which would look like the
/// creation silently failed. A query that *ends* in a slash names a folder to
/// look inside, not a note, so it gets no offer either: `Projects/` should not
/// propose a `Projects.md` sitting next to the folder of the same name.
fn new_note_path(query: &str) -> Option<String> {
    let query = query.trim();
    if query.is_empty()
        || query.ends_with('/')
        || query.starts_with('/')
        || query.contains('\\')
    {
        return None;
    }
    let mut out = String::new();
    let mut components = query.split('/').map(str::trim).peekable();
    while let Some(component) = components.next() {
        if component.is_empty() || component.starts_with('.') {
            return None;
        }
        if !out.is_empty() {
            out.push('/');
        }
        out.push_str(component);
        // Only the last component becomes the file, so only it gets `.md`.
        if components.peek().is_none() && !component.ends_with(".md") {
            out.push_str(".md");
        }
    }
    Some(out)
}

/// Highlight runs for `positions` (ascending byte offsets into `text`).
/// Neighbouring characters coalesce into one run, so a matched word draws as a
/// span rather than as a row of separately styled letters.
fn highlights(
    text: &str,
    positions: &[usize],
    style: HighlightStyle,
) -> Vec<(Range<usize>, HighlightStyle)> {
    let mut runs: Vec<Range<usize>> = Vec::new();
    for &p in positions {
        let end = p + text[p..].chars().next().map_or(1, char::len_utf8);
        match runs.last_mut() {
            Some(last) if last.end == p => last.end = end,
            _ => runs.push(p..end),
        }
    }
    runs.into_iter().map(|r| (r, style)).collect()
}

/// A candidate's fitness for a query.
#[derive(Debug, PartialEq)]
struct Score {
    /// Higher is better. Only meaningful against other candidates' scores for
    /// the same query.
    value: f64,
    /// Byte offsets in the candidate of the characters the query matched,
    /// ascending — what the row highlights.
    positions: Vec<usize>,
}

// The scoring model, lifted from Zed's `fuzzy` crate: a character's worth
// depends on what precedes it, and the score is the product of its characters'
// worths. Multiplying rather than adding is what makes one badly placed
// character cost a candidate the match rather than being averaged away.
/// A character immediately after the one before it — an unbroken run, and the
/// best a match can do.
const RUN: f64 = 1.0;
/// The first character of a path component.
const AFTER_SEPARATOR: f64 = 0.9;
/// The start of a word: after a dash, underscore or space, or the upper case of
/// a camelCase hump.
const WORD_START: f64 = 0.8;
/// After a dot — an extension, mostly.
const AFTER_DOT: f64 = 0.7;
/// A character in the middle of a word, decaying by how much was skipped to
/// reach it so that scattered matches lose to tight ones.
const IN_WORD: f64 = 0.6;
const SKIP_PENALTY: f64 = 0.05;
const MIN_IN_WORD: f64 = 0.2;
/// A character whose case differs from the query's. Only applied when the query
/// has an upper case character in it, which is taken as the user meaning it:
/// then `Alpha` outranks `alpha` decisively, while an all-lower-case query
/// stays case-blind.
const CASE_MISMATCH: f64 = 0.001;

/// Score `candidate`, a vault-relative path, against `query`, and report where
/// the query's characters landed. `None` if the query does not match at all.
///
/// The search is a small dynamic program: for each query character, the best
/// score for placing it at each candidate position it could occupy, given the
/// best placement of the character before it. That is `O(q · n²)` in the worst
/// case for a query of `q` characters and a path of `n`, but the inner loop only
/// visits positions where the *previous* character could sit, which for real
/// paths is a handful. At vault scale — a few hundred notes of ~25 characters —
/// a full pass costs tens of microseconds, well inside a frame, which is why
/// this runs on the UI thread on every keystroke.
///
/// A hit in the file name outranks one that spends characters on the folders
/// above it, but there is no separate bonus for it: the normalization below
/// divides by the length of the text the match sits in, measured from the
/// separator before it, so a match that starts inside the name is divided by the
/// name alone. Two mechanisms pulling the same direction only fight each other
/// — an explicit bonus large enough to matter also fires for every note at the
/// vault root, where the name *is* the whole path.
fn score(query: &str, candidate: &str) -> Option<Score> {
    let hay: Vec<char> = candidate.chars().collect();
    let byte_of: Vec<usize> = candidate.char_indices().map(|(b, _)| b).collect();
    let hay_lower: Vec<char> = hay.iter().copied().map(lower).collect();
    let q: Vec<char> = query.chars().collect();
    if q.is_empty() || hay.is_empty() {
        return None;
    }
    let smart_case = q.iter().any(|c| c.is_uppercase());

    // Where each query character could sit. An absent character rejects the
    // candidate here, before any scoring — which is the common case and the
    // reason a whole-vault pass is cheap.
    let mut slots: Vec<Vec<usize>> = Vec::with_capacity(q.len());
    for qc in &q {
        let qc = lower(*qc);
        let positions: Vec<usize> = hay_lower
            .iter()
            .enumerate()
            .filter(|(_, c)| **c == qc)
            .map(|(i, _)| i)
            .collect();
        if positions.is_empty() {
            return None;
        }
        slots.push(positions);
    }

    // The worth of putting query character `qi` at candidate position `j`,
    // having put the one before it at `prev`.
    let char_score = |qi: usize, j: usize, prev: Option<usize>| -> f64 {
        // A character continuing an unbroken run, and the first character of the
        // path, are both the best a placement can be.
        let mut s = if prev == Some(j.wrapping_sub(1)) || j == 0 {
            RUN
        } else {
            let before = hay[j - 1];
            if before == '/' {
                AFTER_SEPARATOR
            } else if before == '-'
                || before == '_'
                || before == ' '
                || before.is_numeric()
                || (before.is_lowercase() && hay[j].is_uppercase())
            {
                WORD_START
            } else if before == '.' {
                AFTER_DOT
            } else {
                match prev {
                    // Decay by the run of characters skipped to get here.
                    Some(p) => MIN_IN_WORD.max(IN_WORD - (j - p - 1) as f64 * SKIP_PENALTY),
                    None => IN_WORD,
                }
            }
        };
        if smart_case && q[qi] != hay[j] {
            s *= CASE_MISMATCH;
        }
        s
    };

    // Normalize the first character's placement by the length of the text the
    // match sits in, measured from the separator before it: the same hit in a
    // short name beats one buried in a long path.
    let span_from = |j: usize| -> f64 {
        let start = hay[..j].iter().rposition(|c| *c == '/').map_or(0, |i| i + 1);
        (hay.len() - start) as f64
    };

    // `dp[qi][k]` is the best score for placing query characters `0..=qi` with
    // `qi` at `slots[qi][k]`, plus the `k` in the previous row it came from.
    // Zero means that placement is impossible.
    let mut dp: Vec<Vec<(f64, usize)>> = Vec::with_capacity(q.len());
    dp.push(
        slots[0]
            .iter()
            .map(|&j| (char_score(0, j, None) / span_from(j), usize::MAX))
            .collect(),
    );
    for qi in 1..q.len() {
        let row: Vec<(f64, usize)> = slots[qi]
            .iter()
            .map(|&j| {
                let mut best = (0.0, usize::MAX);
                for (k, &prev) in slots[qi - 1].iter().enumerate() {
                    let (before, _) = dp[qi - 1][k];
                    if prev >= j || before <= 0.0 {
                        continue;
                    }
                    let value = before * char_score(qi, j, Some(prev));
                    if value > best.0 {
                        best = (value, k);
                    }
                }
                best
            })
            .collect();
        if row.iter().all(|(v, _)| *v <= 0.0) {
            return None; // the characters are all present, but out of order
        }
        dp.push(row);
    }

    // Walk the best placement of the last character back to the first.
    let last = dp.len() - 1;
    let (mut k, value) = dp[last]
        .iter()
        .enumerate()
        .filter(|(_, (v, _))| *v > 0.0)
        .max_by(|a, b| a.1 .0.total_cmp(&b.1 .0))
        .map(|(k, (v, _))| (k, *v))?;
    let mut chars = vec![slots[last][k]];
    for qi in (1..=last).rev() {
        k = dp[qi][k].1;
        chars.push(slots[qi - 1][k]);
    }
    chars.reverse();

    Some(Score {
        value,
        positions: chars.iter().map(|&i| byte_of[i]).collect(),
    })
}

/// Lower case `c` for matching, one character in and one out. Deliberately not
/// full case folding: a character count that changes would put the byte offsets
/// a highlight is drawn from off the characters they belong to.
fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A candidate as the finder builds it, so the tests match against exactly
    /// what the list does.
    fn candidate(rel: &str) -> Candidate {
        Candidate {
            rel: rel.to_string(),
            path: PathBuf::from(rel),
            name_at: rel.rfind('/').map(|i| i + 1).unwrap_or(0),
        }
    }

    /// Rank `candidates` against `query`, best first — what the list shows.
    fn rank<'a>(query: &str, candidates: &[&'a str]) -> Vec<&'a str> {
        let mut scored: Vec<(f64, &str)> = candidates
            .iter()
            .filter_map(|c| Some((score(query, candidate(c).haystack())?.value, *c)))
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        scored.into_iter().map(|(_, c)| c).collect()
    }

    fn hits(query: &str, rel: &str) -> Vec<usize> {
        score(query, candidate(rel).haystack()).unwrap().positions
    }

    #[test]
    fn matches_characters_in_order_anywhere_in_the_path() {
        assert!(score("alpha", "Projects/Alpha.md").is_some());
        // Gaps are fine, and may span the separator.
        assert!(score("proal", "Projects/Alpha.md").is_some());
        // Order is not.
        assert_eq!(score("ahpla", "Alpha.md"), None);
        // A character that isn't there at all.
        assert_eq!(score("alphaz", "Alpha.md"), None);
        assert_eq!(score("", "Alpha.md"), None);
    }

    #[test]
    fn positions_are_byte_offsets_of_the_matched_characters() {
        assert_eq!(hits("alpha", "Alpha.md"), vec![0, 1, 2, 3, 4]);
        // Two components: the run in the name, not the earlier stray letters.
        assert_eq!(hits("alpha", "Projects/Alpha.md"), vec![9, 10, 11, 12, 13]);
        // Non-ASCII: offsets have to land on character boundaries, and 'ä' is
        // two bytes, so the 'r' after it is at 4 rather than 3.
        assert_eq!(hits("är", "Här.md"), vec![1, 3]);
    }

    #[test]
    fn a_hit_in_the_name_beats_one_in_the_folders() {
        assert_eq!(
            rank("alpha", &["alpha/Notes.md", "Projects/Alpha.md"]),
            vec!["Projects/Alpha.md", "alpha/Notes.md"]
        );
        // And a short name beats a long one containing the same run.
        assert_eq!(
            rank("alpha", &["Alphabetical ordering.md", "Alpha.md"]),
            vec!["Alpha.md", "Alphabetical ordering.md"]
        );
    }

    #[test]
    fn tight_matches_beat_scattered_ones() {
        assert_eq!(
            rank("meet", &["Meeting.md", "My extremely tedious notes.md"]),
            vec!["Meeting.md", "My extremely tedious notes.md"]
        );
        // Initials of words rank well: this is how "rwm" finds a weekly
        // meeting note without typing any of it in full.
        assert_eq!(
            rank("rwm", &["Rough week maybe.md", "Rewrite worm.md"]),
            vec!["Rough week maybe.md", "Rewrite worm.md"]
        );
    }

    /// A row leads with the note's name, labelled the way the sidebar labels it,
    /// and follows with the folders it sits in.
    #[test]
    fn a_row_shows_the_name_without_its_extension() {
        let c = candidate("Projects/deep/Ideas.md");
        assert_eq!(c.label(), "Ideas");
        assert_eq!(c.folders(), "Projects/deep/");

        // At the vault root there are no folders to show.
        let c = candidate("Welcome.md");
        assert_eq!(c.label(), "Welcome");
        assert_eq!(c.folders(), "");

        // Only the suffix goes: dots inside the name are part of it.
        let c = candidate("Notes/v1.2 plan.md");
        assert_eq!(c.label(), "v1.2 plan");

        // A note that somehow isn't a `.md` file keeps its whole name.
        let c = candidate("Projects/README.txt");
        assert_eq!(c.label(), "README.txt");
    }

    /// Every note is a `.md` file, so the extension says nothing about which one
    /// you want — and matching it would let it say the wrong thing.
    #[test]
    fn the_extension_is_not_matched() {
        assert_eq!(rank("md", &["Alpha.md", "Beta.md"]), Vec::<&str>::new());
        // "dm" only matches by reaching into the extension for the "m".
        assert_eq!(rank("dm", &["Second.md"]), Vec::<&str>::new());
        // A note whose name genuinely contains them still matches.
        assert_eq!(rank("md", &["Markdown.md"]), vec!["Markdown.md"]);
    }

    #[test]
    fn an_upper_case_query_prefers_matching_case() {
        assert_eq!(
            rank("Alpha", &["alpha.md", "Alpha.md"]),
            vec!["Alpha.md", "alpha.md"]
        );
        // A lower-case query stays case-blind, so the shorter name wins on
        // length alone rather than on case.
        assert_eq!(
            rank("alpha", &["Alpha.md", "alphabet.md"]),
            vec!["Alpha.md", "alphabet.md"]
        );
    }

    #[test]
    fn typing_a_folder_narrows_to_it() {
        // The query spans the separator, which only the note in that folder can
        // satisfy.
        assert_eq!(
            rank("proal", &["Projects/Alpha.md", "Alpha.md", "Proposals.md"]),
            vec!["Projects/Alpha.md", "Proposals.md"]
        );
    }

    #[test]
    fn create_row_names_a_markdown_note_in_the_vault() {
        assert_eq!(new_note_path("Idea").as_deref(), Some("Idea.md"));
        // `.md` is not appended twice, and only the last component gets it.
        assert_eq!(new_note_path("Idea.md").as_deref(), Some("Idea.md"));
        assert_eq!(
            new_note_path("Projects/New idea").as_deref(),
            Some("Projects/New idea.md")
        );
        // Spaces around a separator are the user typing, not part of a name.
        assert_eq!(
            new_note_path("Projects / Idea").as_deref(),
            Some("Projects/Idea.md")
        );
        // A trailing slash is someone looking inside a folder.
        assert_eq!(new_note_path("Projects/"), None);
        assert_eq!(new_note_path("Projects/ "), None);
        // Nothing that would escape the vault or land somewhere hidden.
        assert_eq!(new_note_path(""), None);
        assert_eq!(new_note_path("   "), None);
        assert_eq!(new_note_path("../outside"), None);
        assert_eq!(new_note_path("/absolute"), None);
        assert_eq!(new_note_path(".hidden"), None);
        assert_eq!(new_note_path("Projects/.hidden"), None);
        assert_eq!(new_note_path("Projects//Idea"), None);
    }

    #[test]
    fn highlight_runs_coalesce_neighbouring_characters() {
        let style = HighlightStyle::default();
        // One span for the run, one for the stray character after the gap.
        let runs = highlights("Alpha.md", &[0, 1, 2, 6], style);
        assert_eq!(
            runs.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>(),
            vec![0..3, 6..7]
        );
        // A run over a two-byte character covers both of its bytes.
        let runs = highlights("Här.md", &[1, 3], style);
        assert_eq!(
            runs.iter().map(|(r, _)| r.clone()).collect::<Vec<_>>(),
            vec![1..4]
        );
        assert!(highlights("Alpha.md", &[], style).is_empty());
    }
}
