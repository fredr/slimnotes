//! Find within the open note: the bar ctrl-f / cmd-f opens above it.
//!
//! Modelled on Obsidian's — a query field, a `3 / 11` counter, arrows to step
//! through the matches, and an X to leave — with the matching itself left where
//! it belongs: the editor owns the query, highlights every match, rings the
//! current one and scrolls it into view (see `slimnotes_editor`'s search
//! module). What lives here is the bar, its keys, and keeping the two in step.
//!
//! The bar is a row in the note pane rather than an overlay over it. A find is
//! not modal — the note stays editable underneath, and an edit re-runs the query
//! — so nothing here occludes the editor, and the row pushes the text down
//! instead of covering the first lines of it.

use gpui::prelude::FluentBuilder;
use gpui::{
    actions, div, px, AnyElement, AppContext, Context, Entity, FocusHandle, Focusable,
    InteractiveElement, IntoElement, ParentElement, SharedString, Styled, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState},
    ActiveTheme, IconName, Sizable,
};

use crate::SlimNotes;

/// The key context the bar's bindings are scoped to (bound in
/// [`crate::bind_keys`]). It sits on the row, and the query field is a
/// descendant of it, so these fire while the field has focus.
pub(crate) const CONTEXT: &str = "NoteSearch";

// Step to the next / previous match, and close the bar.
actions!(slimnotes_search, [FindNext, FindPrev, Dismiss]);

/// Matches the note finder's panel, so the two line up when both have been
/// open.
const BAR_WIDTH: f32 = 600.;

pub(crate) struct NoteSearch {
    /// The query field. The same gpui-component `Input` the finder uses, for
    /// the same reason: it brings IME, selection and clipboard with it.
    query: Entity<InputState>,
    /// What had focus when the bar opened — the editor, normally — so closing
    /// hands the keyboard back rather than dropping it on the floor.
    restore: Option<FocusHandle>,
    /// Keeps the query subscription alive for as long as the bar.
    _sub: Subscription,
}

impl SlimNotes {
    /// Open the find, or put the keyboard back in it when it is already open —
    /// after clicking into the note to edit around a match, the same key should
    /// return you to the bar you are still looking at.
    ///
    /// Leaving is escape or the X rather than a second press. The query field
    /// claims ctrl-f for a find of its own (which this one does not enable, so
    /// it does nothing), so the key only ever reaches here while focus is
    /// elsewhere — the closing branch is kept anyway, so this behaves like the
    /// note finder's toggle if that ever stops being true.
    pub(crate) fn toggle_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &self.search {
            Some(search) => {
                let field = search.query.clone();
                if field.focus_handle(cx).is_focused(window) {
                    self.close_search(window, cx);
                } else {
                    field.update(cx, |state, cx| state.focus(window, cx));
                }
            }
            None => self.open_search(window, cx),
        }
    }

    fn open_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.clone() else {
            return; // nothing to search, and the empty state already says so
        };

        // Seed the query with what is selected, the way every other find does:
        // select a word, press the key, and it is already looking for it. Only
        // a single line of it — a paragraph in the field would be a query that
        // can never match, since the field holds no newlines.
        let seed = editor
            .read(cx)
            .selected_text()
            .filter(|s| !s.trim().is_empty() && !s.contains('\n'))
            .unwrap_or_default()
            .to_string();

        let query = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder("Find in note…");
            if !seed.is_empty() {
                state.set_value(seed.clone(), window, cx);
            }
            state
        });
        let sub = cx.subscribe_in(&query, window, |this, query, event: &InputEvent, _, cx| {
            if let InputEvent::Change = event {
                let text = query.read(cx).value().to_string();
                this.run_search(&text, cx);
            }
        });

        self.search = Some(NoteSearch {
            query: query.clone(),
            restore: window.focused(cx),
            _sub: sub,
        });
        self.run_search(&seed, cx);
        query.update(cx, |state, cx| state.focus(window, cx));
        cx.notify();
    }

    /// Close the bar and drop the highlights with it.
    pub(crate) fn close_search(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(search) = self.search.take() else { return };
        if let Some(editor) = &self.editor {
            editor.update(cx, |e, cx| e.clear_search(cx));
        }
        // Hand focus back, or to the root if there was nothing to hand it back
        // to — leaving it on the dropped field would leave the app with no
        // keyboard at all.
        match search.restore {
            Some(handle) => handle.focus(window),
            None => self.focus_handle.focus(window),
        }
        cx.notify();
    }

    /// Drop the bar without touching focus, for when the editor under it is
    /// being replaced: its matches point into a note that is no longer open,
    /// and the incoming editor takes focus for itself.
    pub(crate) fn discard_search(&mut self) {
        self.search = None;
    }

    fn run_search(&mut self, query: &str, cx: &mut Context<Self>) {
        if let Some(editor) = &self.editor {
            editor.update(cx, |e, cx| e.set_search_query(query, cx));
        }
        cx.notify(); // the counter lives here, and it has just changed
    }

    fn step_search(&mut self, forward: bool, cx: &mut Context<Self>) {
        if let Some(editor) = &self.editor {
            editor.update(cx, |e, cx| e.step_search(forward, cx));
        }
        cx.notify();
    }

    pub(crate) fn on_toggle_search(
        &mut self,
        _: &crate::ToggleSearch,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_search(window, cx);
    }

    /// The find bar, or nothing when it is closed.
    pub(crate) fn render_search_bar(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let search = self.search.as_ref()?;
        let border = cx.theme().border;
        let muted = cx.theme().muted_foreground;
        // `0 / 0` rather than a blank while nothing matches: the counter going
        // to zero as you type is how you learn the note doesn't say it.
        let counter = self
            .editor
            .as_ref()
            .and_then(|e| e.read(cx).search_status())
            .filter(|_| !search.query.read(cx).value().is_empty())
            .map(|(at, total)| format!("{at} / {total}"));

        let step = |id: &'static str, icon: IconName, tip: &'static str, forward: bool| {
            Button::new(id)
                .ghost()
                .xsmall()
                .icon(icon)
                .tooltip(tip)
                .on_click(cx.listener(move |this, _, _, cx| this.step_search(forward, cx)))
        };

        let bar = h_flex()
            .w(px(BAR_WIDTH))
            .max_w_full()
            .items_center()
            .gap(px(2.))
            .pl(px(10.))
            .pr(px(4.))
            .bg(cx.theme().popover)
            .text_color(cx.theme().popover_foreground)
            .border_1()
            .border_color(border)
            .rounded(px(8.))
            .child(div().flex_none().text_color(muted).child(IconName::Search))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .child(Input::new(&search.query).appearance(false).cleanable(false)),
            )
            .when_some(counter, |el, text| {
                el.child(
                    div()
                        .flex_none()
                        .px(px(6.))
                        .text_size(px(12.))
                        .text_color(muted)
                        .child(SharedString::from(text)),
                )
            })
            .child(step("find-prev", IconName::ArrowUp, "Previous match", false))
            .child(step("find-next", IconName::ArrowDown, "Next match", true))
            .child(
                Button::new("find-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Close")
                    .on_click(cx.listener(|this, _, window, cx| this.close_search(window, cx))),
            );

        Some(
            div()
                .key_context(CONTEXT)
                .on_action(cx.listener(|this, _: &FindNext, _, cx| this.step_search(true, cx)))
                .on_action(cx.listener(|this, _: &FindPrev, _, cx| this.step_search(false, cx)))
                .on_action(
                    cx.listener(|this, _: &Dismiss, window, cx| this.close_search(window, cx)),
                )
                .flex_shrink_0()
                .flex()
                .justify_center()
                .px(px(12.))
                .py(px(8.))
                .child(bar)
                .into_any_element(),
        )
    }
}
