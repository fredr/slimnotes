//! Editor input: the action definitions, their default key bindings, and the
//! action handlers.
//!
//! Bindings are declarative (gpui `Action`s bound in the editor's key context)
//! rather than a `match` on key names, so a host can rebind any of them by
//! registering its own `KeyBinding` for the same action afterwards.
//!
//! Text *entry* deliberately does not go through here: printable characters
//! arrive via the platform input handler (the `EntityInputHandler` impl in
//! `lib.rs`), which is what makes IME composition work.

use gpui::{actions, App, Context, InteractiveElement, KeyBinding, Window};

use crate::MarkdownEditor;

/// The key context the bindings below are scoped to. `Render` puts the same
/// string on the editor's root element.
pub(crate) const CONTEXT: &str = "MarkdownEditor";

actions!(
    slimnotes_editor,
    [
        // Caret motion. Each has an `…Extend` sibling that grows the selection.
        MoveLeft,
        MoveRight,
        MoveUp,
        MoveDown,
        MoveLeftExtend,
        MoveRightExtend,
        MoveUpExtend,
        MoveDownExtend,
        MoveWordLeft,
        MoveWordRight,
        MoveWordLeftExtend,
        MoveWordRightExtend,
        MovePageUp,
        MovePageDown,
        MovePageUpExtend,
        MovePageDownExtend,
        MoveLineStart,
        MoveLineEnd,
        MoveLineStartExtend,
        MoveLineEndExtend,
        MoveDocStart,
        MoveDocEnd,
        MoveDocStartExtend,
        MoveDocEndExtend,
        // Editing.
        Backspace,
        Delete,
        InsertNewline,
        InsertTab,
        Outdent,
        // Clipboard, history, selection.
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        SelectAll,
        // Dismiss the table context menu.
        Cancel,
    ]
);

static KEYS_BOUND: std::sync::Once = std::sync::Once::new();

/// Register the default bindings, once per process. Note `ctrl-s`/`cmd-s` is
/// deliberately absent: saving belongs to the host, and leaving it unbound here
/// lets it bubble up out of the editor.
pub(crate) fn bind_default_keys(cx: &mut App) {
    KEYS_BOUND.call_once(|| {
        let c = Some(CONTEXT);
        // Motion + editing keys are the same on every platform.
        let mut b = vec![
            KeyBinding::new("left", MoveLeft, c),
            KeyBinding::new("right", MoveRight, c),
            KeyBinding::new("up", MoveUp, c),
            KeyBinding::new("down", MoveDown, c),
            KeyBinding::new("shift-left", MoveLeftExtend, c),
            KeyBinding::new("shift-right", MoveRightExtend, c),
            KeyBinding::new("shift-up", MoveUpExtend, c),
            KeyBinding::new("shift-down", MoveDownExtend, c),
            KeyBinding::new("pageup", MovePageUp, c),
            KeyBinding::new("pagedown", MovePageDown, c),
            KeyBinding::new("shift-pageup", MovePageUpExtend, c),
            KeyBinding::new("shift-pagedown", MovePageDownExtend, c),
            KeyBinding::new("home", MoveLineStart, c),
            KeyBinding::new("end", MoveLineEnd, c),
            KeyBinding::new("shift-home", MoveLineStartExtend, c),
            KeyBinding::new("shift-end", MoveLineEndExtend, c),
            KeyBinding::new("backspace", Backspace, c),
            KeyBinding::new("delete", Delete, c),
            KeyBinding::new("enter", InsertNewline, c),
            KeyBinding::new("tab", InsertTab, c),
            KeyBinding::new("shift-tab", Outdent, c),
            KeyBinding::new("escape", Cancel, c),
        ];
        // Word-wise arrows use alt on macOS, ctrl elsewhere; the clipboard /
        // history / document-extent bindings use the platform modifier.
        #[cfg(target_os = "macos")]
        b.extend([
            KeyBinding::new("alt-left", MoveWordLeft, c),
            KeyBinding::new("alt-right", MoveWordRight, c),
            KeyBinding::new("alt-shift-left", MoveWordLeftExtend, c),
            KeyBinding::new("alt-shift-right", MoveWordRightExtend, c),
            KeyBinding::new("cmd-left", MoveLineStart, c),
            KeyBinding::new("cmd-right", MoveLineEnd, c),
            KeyBinding::new("cmd-shift-left", MoveLineStartExtend, c),
            KeyBinding::new("cmd-shift-right", MoveLineEndExtend, c),
            KeyBinding::new("cmd-up", MoveDocStart, c),
            KeyBinding::new("cmd-down", MoveDocEnd, c),
            KeyBinding::new("cmd-shift-up", MoveDocStartExtend, c),
            KeyBinding::new("cmd-shift-down", MoveDocEndExtend, c),
            KeyBinding::new("cmd-home", MoveDocStart, c),
            KeyBinding::new("cmd-end", MoveDocEnd, c),
            KeyBinding::new("cmd-c", Copy, c),
            KeyBinding::new("cmd-x", Cut, c),
            KeyBinding::new("cmd-v", Paste, c),
            KeyBinding::new("cmd-a", SelectAll, c),
            KeyBinding::new("cmd-z", Undo, c),
            KeyBinding::new("cmd-shift-z", Redo, c),
        ]);
        #[cfg(not(target_os = "macos"))]
        b.extend([
            KeyBinding::new("ctrl-left", MoveWordLeft, c),
            KeyBinding::new("ctrl-right", MoveWordRight, c),
            KeyBinding::new("ctrl-shift-left", MoveWordLeftExtend, c),
            KeyBinding::new("ctrl-shift-right", MoveWordRightExtend, c),
            KeyBinding::new("ctrl-home", MoveDocStart, c),
            KeyBinding::new("ctrl-end", MoveDocEnd, c),
            KeyBinding::new("ctrl-shift-home", MoveDocStartExtend, c),
            KeyBinding::new("ctrl-shift-end", MoveDocEndExtend, c),
            KeyBinding::new("ctrl-c", Copy, c),
            KeyBinding::new("ctrl-x", Cut, c),
            KeyBinding::new("ctrl-v", Paste, c),
            KeyBinding::new("ctrl-a", SelectAll, c),
            KeyBinding::new("ctrl-z", Undo, c),
            KeyBinding::new("ctrl-shift-z", Redo, c),
            KeyBinding::new("ctrl-y", Redo, c),
        ]);
        cx.bind_keys(b);
    });
}

/// Attach one action handler per action. `$edits` marks the handlers that mutate
/// the buffer, so they emit `Changed` for the host to persist.
macro_rules! on_actions {
    ($el:expr, $cx:expr, $( $action:ty => $edits:literal, |$this:ident, $c:ident| $body:expr );+ $(;)?) => {{
        let el = $el;
        $(
            let el = el.on_action($cx.listener(
                |$this: &mut MarkdownEditor,
                 _: &$action,
                 _window: &mut Window,
                 $c: &mut Context<MarkdownEditor>| {
                    $body;
                    if $edits {
                        $c.emit(crate::EditorEvent::Changed);
                    }
                    $this.after_input($c);
                },
            ));
        )+
        el
    }};
}

impl MarkdownEditor {
    /// Wire every input action onto the editor's root element. Kept beside the
    /// bindings so an action can't be bound without also being handled.
    pub(crate) fn on_input_actions<E: InteractiveElement>(
        &self,
        el: E,
        cx: &mut Context<Self>,
    ) -> E {
        on_actions!(el, cx,
            MoveLeft            => false, |this, _c| this.move_horizontal(false, false);
            MoveRight           => false, |this, _c| this.move_horizontal(true, false);
            MoveLeftExtend      => false, |this, _c| this.move_horizontal(false, true);
            MoveRightExtend     => false, |this, _c| this.move_horizontal(true, true);
            MoveUp              => false, |this, _c| this.move_vertical(false, false);
            MoveDown            => false, |this, _c| this.move_vertical(true, false);
            MoveUpExtend        => false, |this, _c| this.move_vertical(false, true);
            MoveDownExtend      => false, |this, _c| this.move_vertical(true, true);
            MoveWordLeft        => false, |this, _c| this.move_word(false, false);
            MoveWordRight       => false, |this, _c| this.move_word(true, false);
            MoveWordLeftExtend  => false, |this, _c| this.move_word(false, true);
            MoveWordRightExtend => false, |this, _c| this.move_word(true, true);
            MovePageUp          => false, |this, _c| this.move_page(false, false);
            MovePageDown        => false, |this, _c| this.move_page(true, false);
            MovePageUpExtend    => false, |this, _c| this.move_page(false, true);
            MovePageDownExtend  => false, |this, _c| this.move_page(true, true);
            MoveLineStart       => false, |this, _c| this.move_home(false);
            MoveLineEnd         => false, |this, _c| this.move_end(false);
            MoveLineStartExtend => false, |this, _c| this.move_home(true);
            MoveLineEndExtend   => false, |this, _c| this.move_end(true);
            MoveDocStart        => false, |this, _c| this.set_cursor(0, false);
            MoveDocStartExtend  => false, |this, _c| this.set_cursor(0, true);
            MoveDocEnd          => false, |this, _c| { let e = this.text.len(); this.set_cursor(e, false) };
            MoveDocEndExtend    => false, |this, _c| { let e = this.text.len(); this.set_cursor(e, true) };
            Backspace           => true,  |this, _c| this.backspace();
            Delete              => true,  |this, _c| this.delete_forward();
            InsertNewline       => true,  |this, _c| this.insert_newline();
            // These two decide for themselves whether anything changed: on the
            // first item of a list, or outside one, there may be nothing to do.
            InsertTab           => false, |this, c| if this.insert_tab() {
                c.emit(crate::EditorEvent::Changed)
            };
            Outdent             => false, |this, c| if this.outdent_item() {
                c.emit(crate::EditorEvent::Changed)
            };
            Copy                => false, |this, c| this.copy(c);
            Cut                 => true,  |this, c| this.cut(c);
            // Also decides for itself: an image paste puts nothing in the buffer
            // until the host has stored the file (see `EditorEvent::AddImages`).
            Paste               => false, |this, c| if this.paste(c) {
                c.emit(crate::EditorEvent::Changed)
            };
            Undo                => true,  |this, _c| this.undo();
            Redo                => true,  |this, _c| this.redo();
            SelectAll           => false, |this, _c| this.select_all();
            Cancel              => false, |this, _c| { /* after_input closes the menu */ };
        )
    }

    /// Shared tail for every input action: an open table menu is transient, so
    /// any input dismisses it; the caret is kept in view and a repaint queued.
    pub(crate) fn after_input(&mut self, cx: &mut Context<Self>) {
        self.table_menu = None;
        self.scroll_to_caret = true;
        cx.notify();
    }
}
