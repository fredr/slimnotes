//! The sidebar: the vault header and the file tree.
//!
//! Split out from `main.rs`, which keeps the app shell (editor, saving, vault
//! selection). Everything the tree does to the vault — creating, renaming,
//! deleting and moving notes and folders — lives here.

use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    div, point, px, AnyElement, App, AppContext, ClickEvent, Context, Div, DragMoveEvent, Entity,
    Focusable, InteractiveElement, IntoElement, ParentElement, Pixels, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState, SelectAll},
    menu::{ContextMenuExt, PopupMenuItem},
    tooltip::Tooltip,
    v_flex, ActiveTheme, Disableable, IconName, Sizable,
};
use slimnotes_vault::{self as vault, Entry};

use crate::SlimNotes;

/// What an in-progress inline edit will do when it is committed.
#[derive(Clone, Debug)]
enum EditKind {
    /// Name a new note, to be created inside this folder.
    NewNote { parent: PathBuf },
    /// Name a new folder, to be created inside this folder.
    NewFolder { parent: PathBuf },
    /// Rename an existing note or folder.
    Rename { path: PathBuf, is_dir: bool },
}

impl EditKind {
    /// The folder the edited row sits in, which has to be expanded for the row
    /// to be visible.
    fn parent(&self) -> &Path {
        match self {
            Self::NewNote { parent } | Self::NewFolder { parent } => parent,
            // A rename replaces a row that is already on screen.
            Self::Rename { path, .. } => path.parent().unwrap_or(path),
        }
    }

    /// Whether committing this edit produces a folder rather than a note, which
    /// decides whether `.md` is appended to the typed name.
    fn is_dir(&self) -> bool {
        match self {
            Self::NewFolder { .. } => true,
            Self::NewNote { .. } => false,
            Self::Rename { is_dir, .. } => *is_dir,
        }
    }

    fn placeholder(&self) -> &'static str {
        match self {
            Self::NewNote { .. } => "Note name",
            Self::NewFolder { .. } => "Folder name",
            Self::Rename { .. } => "New name",
        }
    }
}

/// A row in the tree that is currently a text field: naming a new note or
/// folder, or renaming an existing one.
pub(crate) struct SidebarEdit {
    kind: EditKind,
    input: Entity<InputState>,
    /// Keeps the Enter/blur subscription alive for as long as the edit.
    _sub: Subscription,
}

/// A context-menu click handler. `Rc` because the menu builder runs afresh every
/// time the menu is opened, so the handler has to be cloneable into each build.
type MenuAction = Rc<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

impl SlimNotes {
    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar_bg = cx.theme().sidebar;
        let fg = cx.theme().sidebar_foreground;
        let muted = cx.theme().muted_foreground;
        let vault_name = self
            .vault
            .as_deref()
            .and_then(|v| v.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "No vault".to_string());
        let root = self.vault.clone().unwrap_or_default();

        v_flex()
            .size_full()
            .min_w(px(0.))
            .bg(sidebar_bg)
            .text_color(fg)
            .child(
                h_flex()
                    .flex_shrink_0()
                    .items_center()
                    .justify_between()
                    .px_3()
                    .py_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .overflow_hidden()
                            .text_ellipsis()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(muted)
                            .child(SharedString::from(vault_name)),
                    )
                    .child(
                        h_flex()
                            .flex_shrink_0()
                            .gap_1()
                            .child(
                                Button::new("new-note")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .tooltip("New note")
                                    .disabled(self.vault.is_none())
                                    .on_click(cx.listener({
                                        let root = root.clone();
                                        move |this, _, window, cx| {
                                            this.begin_edit(
                                                EditKind::NewNote {
                                                    parent: root.clone(),
                                                },
                                                window,
                                                cx,
                                            );
                                        }
                                    })),
                            )
                            .child(
                                Button::new("new-folder")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Folder)
                                    .tooltip("New folder")
                                    .disabled(self.vault.is_none())
                                    .on_click(cx.listener({
                                        let root = root.clone();
                                        move |this, _, window, cx| {
                                            this.begin_edit(
                                                EditKind::NewFolder {
                                                    parent: root.clone(),
                                                },
                                                window,
                                                cx,
                                            );
                                        }
                                    })),
                            )
                            .child(
                                Button::new("open-vault")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::FolderOpen)
                                    .tooltip("Open vault")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_vault(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                // The rows are this element's own children rather than a nested
                // column: `ScrollHandle::scroll_to_item` addresses the tracked
                // element's *immediate* children, which is how a row being
                // named is brought into view.
                div()
                    .id(SharedString::from("tree-scroll"))
                    .flex_1()
                    .flex()
                    .flex_col()
                    .gap_px()
                    .overflow_y_scroll()
                    .track_scroll(&self.tree_scroll)
                    .px_2()
                    .pb_4()
                    // Dropping on the tree's empty space moves an entry to the
                    // vault root — otherwise nothing could be dragged back *out*
                    // of a folder. A drop onto a folder row is unaffected: the
                    // row's hitbox is in front, and it takes the drag before
                    // this ever sees it.
                    .on_drop(cx.listener({
                        let root = root.clone();
                        move |this, dragged: &DragEntry, _, cx| {
                            this.move_into(&dragged.path, &root, cx);
                        }
                    }))
                    // Dragging towards an edge scrolls the tree, so an entry can
                    // be dropped somewhere that is off screen. The listener runs
                    // for every move while an entry is in flight — not only
                    // while the pointer is over the tree — so `bounds` is the
                    // tree's viewport and the pointer may be outside it.
                    .on_drag_move(cx.listener(
                        |this, e: &DragMoveEvent<DragEntry>, _, cx| {
                            let y = e.event.position.y - e.bounds.origin.y;
                            this.drag_scroll(y, e.bounds.size.height, cx);
                        },
                    ))
                    .children(self.render_tree(&self.tree, &root, 0, cx)),
            )
    }

    /// A sidebar row's name: truncated with an ellipsis when it does not fit,
    /// with the full name on hover.
    ///
    /// Deliberately `text_ellipsis()` and *not* `truncate()`, which is the
    /// obvious choice but silently fails here. `truncate()` adds
    /// `white_space: nowrap`, and gpui's text element derives its measurement
    /// cache key from the wrap width — which nowrap pins to `None`. Its cache
    /// check then passes on every later pass, so the size computed by the
    /// *first* measure is kept forever, and that first measure is an intrinsic
    /// one with no definite width, where no truncation is computed. The name is
    /// left full-width and merely clipped at the sidebar edge.
    ///
    /// Leaving the white-space mode alone makes the wrap width follow the
    /// element's actual width, so the pass that finally knows the width misses
    /// the cache, re-measures, and places the ellipsis. The text is truncated to
    /// a single line before shaping, so nothing wraps.
    fn render_name(&self, id: &str, name: String) -> impl IntoElement {
        let full = SharedString::from(name.clone());
        div()
            // An id is what makes the tooltip available (it lives on
            // `StatefulInteractiveElement`), not a layout concern.
            .id(SharedString::from(format!("name-{id}")))
            .flex_1()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .child(SharedString::from(name))
            .tooltip(move |window, cx| Tooltip::new(full.clone()).build(window, cx))
    }

    /// Render the entries of `dir`, which sit at `depth` in the tree.
    fn render_tree(
        &self,
        entries: &[Entry],
        dir: &Path,
        depth: usize,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        // Read colors into owned values so we don't hold a borrow of `cx`
        // across the `&mut cx` recursion / listener calls below.
        let hover = cx.theme().sidebar_accent.alpha(0.5);
        let active_bg = cx.theme().sidebar_accent;
        let active_fg = cx.theme().sidebar_accent_foreground;
        let drop_bg = cx.theme().primary.alpha(0.22);
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().sidebar_foreground;

        let mut out = Vec::new();
        let pad_left = px(row_indent(depth));

        for entry in entries {
            // A rename turns the entry's own row into a text field; a folder
            // being renamed still shows its children.
            if self.is_renaming(&entry.path) {
                out.push(self.render_edit_row(depth, cx));
                if entry.is_dir && self.expanded.contains(&crate::vault_key(&entry.path)) {
                    out.extend(self.render_tree(&entry.children, &entry.path, depth + 1, cx));
                }
                continue;
            }

            if entry.is_dir {
                let path = entry.path.clone();
                let key = crate::vault_key(&path);
                let collapsed = !self.expanded.contains(&key);
                let chevron = if collapsed { "▸" } else { "▾" };
                let name = entry.name.clone();
                let toggle_key = key.clone();
                out.push(
                    h_flex()
                        .id(SharedString::from(format!("dir-{key}")))
                        // A row keeps its natural height: the rows are the
                        // scrolling column's own children, and a flex child is
                        // shrinkable by default — which would squeeze a long
                        // tree to fit instead of letting it scroll.
                        .flex_shrink_0()
                        .relative()
                        .items_center()
                        .gap_1()
                        .pl(pad_left)
                        .pr_2()
                        .py(px(3.))
                        .rounded(px(4.))
                        .text_size(px(13.))
                        .text_color(muted)
                        .cursor_pointer()
                        .hover(move |s| s.bg(hover))
                        // Dropping onto a folder moves the entry into it.
                        .drag_over::<DragEntry>(move |s, _, _, _| s.bg(drop_bg))
                        .on_drop(cx.listener({
                            let path = path.clone();
                            move |this, dragged: &DragEntry, _, cx| {
                                this.move_into(&dragged.path, &path, cx);
                            }
                        }))
                        .on_drag(
                            DragEntry {
                                path: path.clone(),
                                name: name.clone(),
                            },
                            |dragged, _, _, cx| {
                                let dragged = dragged.clone();
                                cx.new(|_| dragged)
                            },
                        )
                        .children(indent_guides(depth, cx))
                        .child(
                            div()
                                .w(px(12.))
                                .flex_shrink_0()
                                .text_size(px(9.))
                                .child(SharedString::from(chevron)),
                        )
                        .child(self.render_name(&key, name))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_folder(&toggle_key, cx);
                        }))
                        .context_menu(self.folder_menu(&path, cx))
                        .into_any_element(),
                );
                if !collapsed {
                    out.extend(self.render_tree(&entry.children, &entry.path, depth + 1, cx));
                }
            } else {
                let path = entry.path.clone();
                let id = crate::vault_key(&path);
                let is_current = self.current.as_deref() == Some(path.as_path());
                let label = entry
                    .name
                    .strip_suffix(".md")
                    .unwrap_or(&entry.name)
                    .to_string();

                let mut row = h_flex()
                    .id(SharedString::from(format!("file-{id}")))
                    .flex_shrink_0()
                    .relative()
                    .items_center()
                    .pl(pad_left)
                    .pr_2()
                    .py(px(3.))
                    .rounded(px(4.))
                    .text_size(px(13.))
                    .text_color(fg)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .on_drag(
                        DragEntry {
                            path: path.clone(),
                            name: label.clone(),
                        },
                        |dragged, _, _, cx| {
                            let dragged = dragged.clone();
                            cx.new(|_| dragged)
                        },
                    )
                    .children(indent_guides(depth, cx))
                    .child(self.render_name(&id, label))
                    .on_click(cx.listener({
                        let path = path.clone();
                        move |this, _, window, cx| {
                            this.open_note(path.clone(), window, cx);
                        }
                    }));

                if is_current {
                    row = row.bg(active_bg).text_color(active_fg);
                }
                out.push(row.context_menu(self.note_menu(&path, cx)).into_any_element());
            }
        }

        // A new note or folder being named inside `dir` is the last row of it.
        if let Some(edit) = &self.edit {
            if matches!(
                edit.kind,
                EditKind::NewNote { .. } | EditKind::NewFolder { .. }
            ) && edit.kind.parent() == dir
            {
                out.push(self.render_edit_row(depth, cx));
            }
        }
        out
    }

    /// The row being edited, as a text field styled to sit in the tree without
    /// making it jump.
    fn render_edit_row(&self, depth: usize, cx: &mut Context<Self>) -> AnyElement {
        let Some(edit) = &self.edit else {
            return div().into_any_element();
        };
        h_flex()
            .flex_shrink_0()
            .relative()
            .items_center()
            .pl(px(2.0 + depth as f32 * 12.0))
            .pr_2()
            .py(px(1.))
            .text_size(px(13.))
            // Escape aborts. The input's own Escape handler calls `propagate`,
            // so it reaches this ancestor after the input has had its say.
            .on_action(cx.listener(|this, _: &gpui_component::input::Escape, _, cx| {
                this.cancel_edit(cx);
            }))
            .children(indent_guides(depth, cx))
            .child(
                Input::new(&edit.input)
                    .xsmall()
                    .cleanable(false)
                    .appearance(true),
            )
            .into_any_element()
    }

    /// Whether `path` is the entry currently being renamed.
    fn is_renaming(&self, path: &Path) -> bool {
        matches!(
            self.edit.as_ref().map(|e| &e.kind),
            Some(EditKind::Rename { path: p, .. }) if p == path
        )
    }

    /// Rename / Delete, plus creation inside the folder.
    fn folder_menu(
        &self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> impl Fn(
        gpui_component::menu::PopupMenu,
        &mut Window,
        &mut Context<gpui_component::menu::PopupMenu>,
    ) -> gpui_component::menu::PopupMenu
    + 'static {
        let new_note = self.edit_action(
            EditKind::NewNote {
                parent: path.to_path_buf(),
            },
            cx,
        );
        let new_folder = self.edit_action(
            EditKind::NewFolder {
                parent: path.to_path_buf(),
            },
            cx,
        );
        let rename = self.edit_action(
            EditKind::Rename {
                path: path.to_path_buf(),
                is_dir: true,
            },
            cx,
        );
        let delete = self.delete_action(path.to_path_buf(), cx);
        move |menu, _, _| {
            let (new_note, new_folder, rename, delete) = (
                new_note.clone(),
                new_folder.clone(),
                rename.clone(),
                delete.clone(),
            );
            menu.item(
                PopupMenuItem::new("New note").on_click(move |e, w, cx| new_note(e, w, cx)),
            )
            .item(
                PopupMenuItem::new("New folder").on_click(move |e, w, cx| new_folder(e, w, cx)),
            )
            .separator()
            .item(PopupMenuItem::new("Rename").on_click(move |e, w, cx| rename(e, w, cx)))
            .item(PopupMenuItem::new("Delete").on_click(move |e, w, cx| delete(e, w, cx)))
        }
    }

    fn note_menu(
        &self,
        path: &Path,
        cx: &mut Context<Self>,
    ) -> impl Fn(
        gpui_component::menu::PopupMenu,
        &mut Window,
        &mut Context<gpui_component::menu::PopupMenu>,
    ) -> gpui_component::menu::PopupMenu
    + 'static {
        let rename = self.edit_action(
            EditKind::Rename {
                path: path.to_path_buf(),
                is_dir: false,
            },
            cx,
        );
        let delete = self.delete_action(path.to_path_buf(), cx);
        move |menu, _, _| {
            let (rename, delete) = (rename.clone(), delete.clone());
            menu.item(PopupMenuItem::new("Rename").on_click(move |e, w, cx| rename(e, w, cx)))
                .item(PopupMenuItem::new("Delete").on_click(move |e, w, cx| delete(e, w, cx)))
        }
    }

    fn edit_action(&self, kind: EditKind, cx: &mut Context<Self>) -> MenuAction {
        Rc::new(cx.listener(move |this, _: &ClickEvent, window, cx| {
            this.begin_edit(kind.clone(), window, cx);
        }))
    }

    fn delete_action(&self, path: PathBuf, cx: &mut Context<Self>) -> MenuAction {
        Rc::new(cx.listener(move |this, _: &ClickEvent, _, cx| {
            this.delete_entry(&path, cx);
        }))
    }

    /// Start an inline edit: put a text field in the tree, focused, with the
    /// existing name selected so typing replaces it.
    fn begin_edit(
        &mut self,
        kind: EditKind,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.vault.is_none() {
            return;
        }
        let initial = match &kind {
            EditKind::Rename { path, is_dir } => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                if *is_dir {
                    name
                } else {
                    name.strip_suffix(".md").map(str::to_string).unwrap_or(name)
                }
            }
            _ => String::new(),
        };

        let placeholder = kind.placeholder();
        let input = cx.new(|cx| InputState::new(window, cx).placeholder(placeholder));
        // `set_value` rather than `default_value`: the latter leaves the cursor
        // at offset 0, so the first thing typed lands *before* the existing name
        // ("NewAlpha.md"). This puts the cursor after the name, which is also the
        // sane fallback if the select-all below does not land.
        if !initial.is_empty() {
            input.update(cx, |state, cx| state.set_value(initial, window, cx));
        }
        // Enter commits, Escape cancels; losing focus deliberately does
        // *neither*. Committing on blur would be worse than untidy: a menu or
        // popover taking focus back would rename the user's note behind them.
        // Cancelling on blur is not safe either, because the context menu this
        // edit is usually started from re-focuses itself while it closes, which
        // would discard the edit the instant it opened. An abandoned field is
        // harmless and goes away on Escape or on opening another note.
        let sub = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, window, cx| {
            if let InputEvent::PressEnter { .. } = event {
                this.commit_edit(window, cx);
            }
        });

        // The row has to be on screen to be typed into.
        let parent = kind.parent().to_path_buf();
        self.edit = Some(SidebarEdit {
            kind,
            input: input.clone(),
            _sub: sub,
        });
        if self.vault.as_deref() != Some(parent.as_path()) {
            self.expand_folder(&parent);
        }
        // A row that is created below the fold is no use: the field is focused,
        // so whatever is typed goes somewhere the user cannot see.
        self.reveal_edit_row();

        // Focus waits for the next frame: an edit started from the context menu
        // runs inside the menu's click handler, and the menu re-focuses itself
        // as it closes, so focusing now would simply be undone.
        //
        // Selecting the old name — what makes a rename a one-word edit — waits
        // for the frame *after* that. Action dispatch resolves its target
        // through the rendered frame's dispatch tree, and next-frame callbacks
        // run before that frame is drawn, so the input is only reachable once
        // the frame that first drew it has been presented. The `notify` is what
        // guarantees there is such a frame.
        let focus = input.read(cx).focus_handle(cx);
        let view = cx.entity();
        window.on_next_frame(move |window, cx| {
            focus.focus(window);
            window.on_next_frame({
                let focus = focus.clone();
                move |window, cx| focus.dispatch_action(&SelectAll, window, cx)
            });
            view.update(cx, |_, cx| cx.notify());
        });
        cx.notify();
    }

    /// Abandon the edit, changing nothing on disk.
    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        if self.edit.take().is_some() {
            cx.notify();
        }
    }

    /// Apply the edit: create the note/folder, or rename the entry.
    ///
    /// A name that cannot be used leaves the field open with a message, so the
    /// user can fix it instead of retyping from scratch.
    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(edit) = &self.edit else { return };
        let name = edit.input.read(cx).value().trim().to_string();
        let kind = edit.kind.clone();

        if name.is_empty() {
            self.cancel_edit(cx);
            return;
        }
        if let Err(reason) = validate_name(&name) {
            self.status = Some(reason.to_string());
            cx.notify();
            return;
        }

        // Folders keep the typed name; notes are markdown files.
        let file_name = if kind.is_dir() || name.ends_with(".md") {
            name
        } else {
            format!("{name}.md")
        };

        let result = match &kind {
            EditKind::NewNote { parent } => {
                let path = parent.join(&file_name);
                vault::create_note(&path).map(|()| Some(path))
            }
            EditKind::NewFolder { parent } => {
                let path = parent.join(&file_name);
                vault::create_folder(&path).map(|()| None)
            }
            EditKind::Rename { path, .. } => {
                let target = path
                    .parent()
                    .map(|p| p.join(&file_name))
                    .unwrap_or_else(|| PathBuf::from(&file_name));
                match vault::rename_entry(path, &target) {
                    Ok(()) => {
                        // The open note, the expanded folders and the saved last
                        // note all refer to the old path.
                        self.remap_paths(path, &target);
                        Ok(None)
                    }
                    Err(e) => Err(e),
                }
            }
        };

        match result {
            Ok(created_note) => {
                self.edit = None;
                self.status = None;
                self.reload_vault(cx);
                if let Some(path) = created_note {
                    self.open_note(path, window, cx);
                }
                cx.notify();
            }
            Err(e) => {
                // Leave the field open: the name is almost certainly taken and
                // the user only needs to adjust it. The io error names an
                // absolute path, which is no help to someone who just typed a
                // name, so say it in those terms instead.
                self.status = Some(if e.kind() == std::io::ErrorKind::AlreadyExists {
                    format!("“{file_name}” already exists")
                } else {
                    format!("Could not use that name: {e}")
                });
                cx.notify();
            }
        }
    }

    /// Move `path` to the vault's `.trash/`, where Obsidian also puts deleted
    /// files — recoverable, and hidden from the tree because it starts with a
    /// dot.
    fn delete_entry(&mut self, path: &Path, cx: &mut Context<Self>) {
        let Some(root) = self.vault.clone() else { return };

        // Close the note first if it is the one going away (or lives inside a
        // folder that is). Autosave writes whatever `current` points at, and
        // `write_note` recreates a missing file — so a pending save would put
        // the note straight back after the delete.
        if self
            .current
            .as_deref()
            .is_some_and(|c| c == path || c.starts_with(path))
        {
            self.close_note();
        }

        match vault::trash_entry(&root, path) {
            Ok(_) => {
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                self.status = Some(format!("Moved {name} to .trash"));
                self.expanded.remove(&crate::vault_key(path));
                self.persist_expanded();
                self.reload_vault(cx);
            }
            Err(e) => self.status = Some(format!("Could not delete: {e}")),
        }
        cx.notify();
    }

    /// Move a note or folder into `dest_dir` (drag and drop).
    fn move_into(&mut self, path: &Path, dest_dir: &Path, cx: &mut Context<Self>) {
        // Dropping something where it already is, or a folder onto itself, is a
        // gesture that means nothing rather than a mistake worth reporting.
        // Dropping a folder into its own *descendant* is a real mistake, and
        // `move_entry` refuses it with a message.
        if path.parent() == Some(dest_dir) || path == dest_dir {
            return;
        }
        match vault::move_entry(path, dest_dir) {
            Ok(target) => {
                self.remap_paths(path, &target);
                self.status = None;
                // Reveal where it landed — except at the root, which is always
                // shown and is not a folder anyone expands.
                if self.vault.as_deref() != Some(dest_dir) {
                    self.expand_folder(dest_dir);
                }
                self.reload_vault(cx);
            }
            Err(e) => self.status = Some(format!("Could not move: {e}")),
        }
        cx.notify();
    }

    /// Follow `old` becoming `new` in everything that remembers a path: the open
    /// note, the expanded folders, and this vault's saved last note and recent
    /// notes. Entries *inside* a renamed folder move with it.
    fn remap_paths(&mut self, old: &Path, new: &Path) {
        let remap = |p: &Path| -> Option<PathBuf> {
            let rest = p.strip_prefix(old).ok()?;
            Some(if rest.as_os_str().is_empty() {
                new.to_path_buf()
            } else {
                new.join(rest)
            })
        };

        if let Some(current) = self.current.clone() {
            if let Some(moved) = remap(&current) {
                self.current = Some(moved);
                // Rewrites this vault's last_note, so a restart reopens the note
                // under its new name rather than silently skipping it.
                self.remember_note();
            }
        }

        let remapped: Vec<String> = self
            .expanded
            .iter()
            .map(|p| match remap(Path::new(p)) {
                Some(moved) => crate::vault_key(&moved),
                None => p.clone(),
            })
            .collect();
        self.expanded = remapped.into_iter().collect();

        // The recent list holds paths too. A stale entry would be dropped by
        // the finder rather than shown wrongly, but following the rename is what
        // keeps a note the user just renamed at the top of the list.
        if let Some(vault) = self.vault.as_deref().map(crate::vault_key) {
            if let Some(recent) = self.settings.recent_notes.get_mut(&vault) {
                for note in recent.iter_mut() {
                    if let Some(moved) = remap(Path::new(note)) {
                        *note = crate::vault_key(&moved);
                    }
                }
            }
        }
        self.persist_expanded();
    }

    /// Expand or collapse a folder, and remember it for this vault.
    fn toggle_folder(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_string());
        }
        self.persist_expanded();
        cx.notify();
    }

    /// Persist the expand/collapse state of the current vault's folders.
    fn persist_expanded(&mut self) {
        let Some(vault) = self.vault.as_deref().map(crate::vault_key) else {
            return;
        };
        let mut folders: Vec<String> = self.expanded.iter().cloned().collect();
        folders.sort();
        self.settings.expanded_folders.insert(vault, folders);
        self.persist_settings();
    }

    /// Mark `dir` expanded so a row inside it is visible.
    fn expand_folder(&mut self, dir: &Path) {
        self.expanded.insert(crate::vault_key(dir));
        self.persist_expanded();
    }

    /// Position of the edited row in the tree's flat list of rows, which is what
    /// [`gpui::ScrollHandle::scroll_to_item`] addresses.
    ///
    /// Walks the tree in exactly the order [`Self::render_tree`] emits rows —
    /// each entry, then its children while it is expanded, and a row being named
    /// last inside its parent — so the two have to be changed together. A rename
    /// takes over the row of the entry it renames, so it has that row's index.
    fn edit_row_index(&self) -> Option<usize> {
        fn walk(
            this: &SlimNotes,
            entries: &[Entry],
            dir: &Path,
            kind: &EditKind,
            next: &mut usize,
        ) -> Option<usize> {
            for entry in entries {
                if this.is_renaming(&entry.path) {
                    return Some(*next);
                }
                *next += 1;
                if entry.is_dir && this.expanded.contains(&crate::vault_key(&entry.path)) {
                    if let Some(found) = walk(this, &entry.children, &entry.path, kind, next) {
                        return Some(found);
                    }
                }
            }
            let new_row = matches!(
                kind,
                EditKind::NewNote { parent } | EditKind::NewFolder { parent } if parent == dir
            );
            new_row.then_some(*next)
        }

        let kind = &self.edit.as_ref()?.kind;
        let root = self.vault.as_deref()?;
        walk(self, &self.tree, root, kind, &mut 0)
    }

    /// Scroll the tree just enough to show the row being named, for a row that
    /// is created off screen — a new folder at the bottom of a long tree, say.
    ///
    /// The scroll is requested rather than performed: the row does not exist in
    /// the layout yet, and `scroll_to_item` waits for the frame where the index
    /// it names has been laid out.
    fn reveal_edit_row(&mut self) {
        if let Some(ix) = self.edit_row_index() {
            self.tree_scroll.scroll_to_item(ix);
        }
    }

    /// Position of `target`'s row in the tree's flat list of rows, or `None` if
    /// it is inside a collapsed folder and so has no row at all.
    ///
    /// Walks in the same order as [`Self::edit_row_index`], which is
    /// [`Self::render_tree`]'s order — change one and all three have to agree.
    /// There is no edit row to account for here: opening a note abandons any
    /// half-typed name first.
    fn note_row_index(&self, target: &Path) -> Option<usize> {
        fn walk(
            this: &SlimNotes,
            entries: &[Entry],
            target: &Path,
            next: &mut usize,
        ) -> Option<usize> {
            for entry in entries {
                if entry.path == target {
                    return Some(*next);
                }
                *next += 1;
                if entry.is_dir && this.expanded.contains(&crate::vault_key(&entry.path)) {
                    if let Some(found) = walk(this, &entry.children, target, next) {
                        return Some(found);
                    }
                }
            }
            None
        }
        walk(self, &self.tree, target, &mut 0)
    }

    /// Show the open note in the tree: expand the folders above it and scroll
    /// its row into view.
    ///
    /// A note can be opened from somewhere other than the tree — the finder, a
    /// `[[wikilink]]`, or the note restored at startup — and a selected row that
    /// is folded away or below the fold tells the user nothing about where they
    /// are.
    pub(crate) fn reveal_note_in_tree(&mut self) {
        let (Some(root), Some(path)) = (self.vault.clone(), self.current.clone()) else {
            return;
        };
        if !path.starts_with(&root) {
            return; // not in this vault, so it has no row to reveal
        }
        // Every folder between the vault root and the note, exclusive of both.
        let mut added = false;
        for dir in path.ancestors().skip(1).take_while(|d| *d != root) {
            added |= self.expanded.insert(crate::vault_key(dir));
        }
        // Writing the settings file is the expensive part, and opening a note in
        // a folder that is already open changes nothing to write.
        if added {
            self.persist_expanded();
        }
        // Requested rather than performed: the rows of a folder expanded just
        // now are not in the layout yet, and `scroll_to_item` waits for the
        // frame in which the index it names has been laid out.
        if let Some(ix) = self.note_row_index(&path) {
            self.tree_scroll.scroll_to_item(ix);
        }
    }

    /// Aim drag-autoscroll at a pointer `y` relative to the top of the tree's
    /// viewport, which is `height` tall. Starting it spawns a loop that keeps
    /// scrolling until the pointer leaves the edge zone or the drag ends;
    /// calling it again while that loop runs only changes the speed.
    fn drag_scroll(&mut self, y: Pixels, height: Pixels, cx: &mut Context<Self>) {
        let step = drag_scroll_step(y, height);
        let start = self.drag_scroll.is_none();
        self.drag_scroll = (step != px(0.)).then_some(step);
        if !start || self.drag_scroll.is_none() {
            return;
        }
        self.drag_scroll_gen = self.drag_scroll_gen.wrapping_add(1);
        let generation = self.drag_scroll_gen;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(DRAG_SCROLL_TICK).await;
                let go = this
                    .update(cx, |this, cx| {
                        this.drag_scroll_gen == generation && this.drag_scroll_tick(cx)
                    })
                    .unwrap_or(false); // the view was dropped
                if !go {
                    break;
                }
            }
        })
        .detach();
    }

    /// One drag-autoscroll tick. Returns whether to keep going.
    fn drag_scroll_tick(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(step) = self.drag_scroll else { return false };
        // The drop (or a cancelled drag) is the end of it. Mouse-up does not
        // reach the tree when it happens somewhere else in the window, so this
        // is what stops the loop then.
        if !cx.has_active_drag() {
            self.drag_scroll = None;
            return false;
        }
        let offset = self.tree_scroll.offset();
        // A scroll offset is 0 at the top and negative from there, so scrolling
        // down (a positive step) subtracts.
        let y = (offset.y - step).clamp(-self.tree_scroll.max_offset().height, px(0.));
        if y != offset.y {
            self.tree_scroll.set_offset(point(offset.x, y));
            cx.notify();
        }
        // Keep ticking even at either end: the pointer is still in the edge
        // zone, and the drag may come back the other way.
        true
    }
}

/// How often drag-autoscroll ticks (~60Hz), which is what makes the steps below
/// a speed rather than a distance.
const DRAG_SCROLL_TICK: Duration = Duration::from_millis(16);
/// Depth of the hot zone at each end of the tree's viewport.
const DRAG_SCROLL_EDGE: f32 = 28.0;
/// Step at the inner edge of the zone (~90 px/s): slow enough to aim at a folder
/// that is only just coming into view.
const DRAG_SCROLL_MIN: f32 = 1.5;
/// Step at the viewport edge and beyond (~1080 px/s), to cross a long tree.
const DRAG_SCROLL_MAX: f32 = 18.0;

/// How far to scroll the tree in one tick while dragging an entry, given the
/// pointer's y relative to the top of the tree's viewport: negative to scroll
/// up, positive to scroll down, zero anywhere in the middle.
///
/// A function of position only, never of movement — that is what lets the tree
/// keep scrolling while the pointer is held at the edge, which is the whole
/// point. Unlike the editor's drag-selection equivalent, the zone reaches
/// *inside* the viewport: a drop target has to be visible to be aimed at, so the
/// pointer is over the last visible row exactly when the list needs to move. It
/// keeps ramping past the edge, where the pointer has left the tree altogether.
fn drag_scroll_step(y: Pixels, height: Pixels) -> Pixels {
    let (y, h) = (f32::from(y), f32::from(height));
    // In a viewport too short for two full zones they meet in the middle rather
    // than overlapping, which would make the speed jump at the seam.
    let edge = DRAG_SCROLL_EDGE.min(h / 2.);
    if edge <= 0. {
        return px(0.);
    }
    let over = if y < edge {
        y - edge
    } else if y > h - edge {
        y - (h - edge)
    } else {
        return px(0.);
    };
    let t = (over.abs() / edge).min(1.);
    px(over.signum() * (DRAG_SCROLL_MIN + (DRAG_SCROLL_MAX - DRAG_SCROLL_MIN) * t))
}

/// Left padding of a tree row at `depth`: where a folder's chevron, or a note's
/// name, starts.
fn row_indent(depth: usize) -> f32 {
    6.0 + depth as f32 * 12.0
}

/// How far into a folder's chevron box its indent guide runs: under the tip of
/// the glyph, which is 5px wide and sits at the left of the box rather than
/// centered in it.
const GUIDE_OFFSET: f32 = 2.0;

/// Indent guides for a row at `depth`: a thin vertical line under the chevron
/// of each folder the row is inside. Every row of an expanded folder draws
/// them, so they join up into one line from the folder down to its last
/// descendant, which is what makes the nesting of a deep tree readable.
///
/// Each line reaches 1px above its row, across the tree's `gap_px`, or the line
/// would be dashed. That is why the rows do not clip their children: any
/// overflow other than visible clips *both* axes in gpui, and a row's name
/// ellipsizes itself.
///
/// Translucent rather than a solid theme color, so the line still shows over a
/// hovered or selected row instead of breaking there.
fn indent_guides(depth: usize, cx: &App) -> Vec<Div> {
    let color = cx.theme().muted_foreground.alpha(0.3);
    (0..depth)
        .map(|level| {
            div()
                .absolute()
                .top(px(-1.))
                .bottom_0()
                .left(px(row_indent(level) + GUIDE_OFFSET))
                .w_px()
                .bg(color)
        })
        .collect()
}

/// An entry being dragged through the tree. Renders as the label that follows
/// the pointer.
#[derive(Clone)]
struct DragEntry {
    path: PathBuf,
    name: String,
}

impl gpui::Render for DragEntry {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .px_2()
            .py(px(2.))
            .rounded(px(4.))
            .text_size(px(13.))
            .bg(cx.theme().popover)
            .border_1()
            .border_color(cx.theme().border)
            .text_color(cx.theme().popover_foreground)
            .child(SharedString::from(self.name.clone()))
    }
}

/// Reject names that would not do what the user means. Path separators would
/// silently create or move into another folder, `.` and `..` are not names, and
/// a leading dot makes the entry hidden — so it would vanish from the tree.
fn validate_name(name: &str) -> Result<(), &'static str> {
    if name.contains('/') || name.contains('\\') {
        return Err("Names cannot contain slashes");
    }
    if name == "." || name == ".." {
        return Err("That is not a usable name");
    }
    if name.starts_with('.') {
        return Err("Names starting with a dot are hidden");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn step(y: f32, height: f32) -> f32 {
        f32::from(drag_scroll_step(px(y), px(height)))
    }

    #[test]
    fn drag_scroll_is_still_in_the_middle_of_the_tree() {
        // Anywhere clear of both zones, dragging must not move the list.
        for y in [DRAG_SCROLL_EDGE, 100., 200., 300. - DRAG_SCROLL_EDGE] {
            assert_eq!(step(y, 300.), 0., "y = {y}");
        }
    }

    #[test]
    fn drag_scroll_ramps_towards_the_edge_it_is_nearest() {
        // Up (negative) at the top, down (positive) at the bottom, and the two
        // ends mirror each other.
        assert_eq!(step(DRAG_SCROLL_EDGE / 2., 300.), -step(300. - DRAG_SCROLL_EDGE / 2., 300.));
        assert!(step(20., 300.) < 0.);
        assert!(step(290., 300.) > 0.);
        // Speed grows as the pointer nears the edge...
        assert!(step(5., 300.) < step(20., 300.));
        // ...to a cap that a pointer dragged clean out of the tree cannot exceed.
        assert_eq!(step(0., 300.), -DRAG_SCROLL_MAX);
        assert_eq!(step(-500., 300.), -DRAG_SCROLL_MAX);
        assert_eq!(step(300., 300.), DRAG_SCROLL_MAX);
        assert_eq!(step(9000., 300.), DRAG_SCROLL_MAX);
    }

    #[test]
    fn drag_scroll_zones_meet_rather_than_overlap_in_a_short_tree() {
        // A viewport shorter than two zones: every point still resolves to one
        // direction, and the middle is the only place with no scroll.
        let h = DRAG_SCROLL_EDGE; // half a zone at each end
        assert!(step(1., h) < 0.);
        assert!(step(h - 1., h) > 0.);
        assert_eq!(step(h / 2., h), 0.);
        // A collapsed viewport has no edges to speak of.
        assert_eq!(step(0., 0.), 0.);
        assert_eq!(step(10., -5.), 0.);
    }
}
