#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(clippy::collapsible_if)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use gpui::{
    actions, div, px, size, AnyElement, App, AppContext, Application, Context, Entity, FocusHandle,
    DragMoveEvent, InteractiveElement, IntoElement, KeyBinding, MouseButton, ParentElement,
    PathPromptOptions, Pixels, Render, ScrollHandle, SharedString, StatefulInteractiveElement,
    Styled, Subscription, Window, WindowBounds, WindowOptions,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex, v_flex, ActiveTheme, IconName, Root, Sizable, Theme, ThemeMode,
};
use gpui_component_assets::Assets;
use slimnotes_editor::{EditorEvent, ImageSource, MarkdownEditor};
use slimnotes_vault::{self as vault, Entry, FileSig, Settings, VaultIndex, VaultWatcher};

mod cli;
mod finder;
mod search;
mod sidebar;

const CONTEXT: &str = "SlimNotes";

// Save the current note; show/hide the sidebar; open the note finder; find
// within the open note.
actions!(slimnotes, [Save, ToggleSidebar, ToggleNoteFinder, ToggleSearch]);

/// Sidebar width used until the user resizes it.
const DEFAULT_SIDEBAR_WIDTH: f32 = 240.;
/// How far the sidebar can be dragged. The lower bound keeps a usable amount of
/// each name visible; below that, collapsing it is the better answer.
const SIDEBAR_MIN_WIDTH: f32 = 160.;
const SIDEBAR_MAX_WIDTH: f32 = 560.;

/// The settings key for a vault: its path as a lossy string. Every per-vault
/// map in [`Settings`] is keyed this way.
pub(crate) fn vault_key(vault: &Path) -> String {
    vault.to_string_lossy().into_owned()
}

/// The set of expanded folder paths saved for `vault` (empty if none saved, so
/// folders start collapsed).
fn expanded_for(settings: &Settings, vault: Option<&Path>) -> HashSet<String> {
    vault
        .map(vault_key)
        .and_then(|v| settings.expanded_folders.get(&v))
        .map(|f| f.iter().cloned().collect())
        .unwrap_or_default()
}

/// The note to reopen for `vault`: the one open when it was last used, if it is
/// still a file. A saved note that has since been deleted, renamed, or moved is
/// skipped silently — reopening it is a convenience, not something the user
/// asked for, so its absence is not worth a message.
fn last_note_for(settings: &Settings, vault: Option<&Path>) -> Option<PathBuf> {
    let path = PathBuf::from(settings.last_note.get(&vault_key(vault?))?);
    path.is_file().then_some(path)
}

/// Override gpui-component's default dark palette (near-black / near-white,
/// high contrast) with a softer dark scheme.
fn apply_palette(cx: &mut App) {
    fn c(hex: u32) -> gpui::Hsla {
        gpui::rgb(hex).into()
    }
    let t = Theme::global_mut(cx);
    // Surfaces
    t.colors.background = c(0x1e1e1e);
    t.colors.sidebar = c(0x1a1a1a);
    t.colors.border = c(0x323232);
    t.colors.sidebar_border = c(0x2a2a2a);
    // Text
    t.colors.foreground = c(0xdadada);
    t.colors.sidebar_foreground = c(0xb4b4b6);
    t.colors.muted = c(0x2a2a2a);
    t.colors.muted_foreground = c(0x888a8f);
    // Floating surfaces (the note finder, popup menus, drag previews): a step
    // lighter than the background, so one reads as sitting above it.
    t.colors.popover = c(0x262626);
    t.colors.popover_foreground = c(0xdadada);
    // Panels (code blocks, ghost buttons, subtle hovers)
    t.colors.secondary = c(0x2a2a2a);
    t.colors.secondary_foreground = c(0xdadada);
    t.colors.accent = c(0x333333);
    t.colors.accent_foreground = c(0xdadada);
    // Selected file / hover in the sidebar
    t.colors.sidebar_accent = c(0x363636);
    t.colors.sidebar_accent_foreground = c(0xffffff);
    // Accent / links
    t.colors.primary = c(0x9d86e9);
    t.colors.primary_foreground = c(0xffffff);
    t.colors.ring = c(0x9d86e9);
}

/// Set the app's chrome — sidebar, header, note finder, popups — in the same
/// faces the editor renders notes in.
///
/// gpui-component defaults to `.SystemUIFont`, which is a different typeface
/// from the note text right next to it, and which gpui cannot resolve weights
/// out of: a bold or medium run silently comes back as the regular face (or a
/// synthesized one). The editor bundles IBM Plex with real weight faces, so
/// using it here makes the whole window agree and makes emphasis actually work.
fn apply_fonts(cx: &mut App) {
    slimnotes_editor::load_fonts(cx);
    let t = Theme::global_mut(cx);
    t.font_family = slimnotes_editor::UI_FONT.into();
    t.mono_font_family = slimnotes_editor::MONO_FONT.into();
}

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-s", Save, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-s", Save, Some(CONTEXT)),
        // Same key VS Code and Zed use for the primary side panel.
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-\\", ToggleSidebar, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-\\", ToggleSidebar, Some(CONTEXT)),
        // The key Zed and VS Code open their file finder with. Pressing it
        // again closes the finder: it is bound in the app's context, which the
        // finder sits inside.
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-p", ToggleNoteFinder, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-p", ToggleNoteFinder, Some(CONTEXT)),
        // The finder's own keys, scoped to its panel. `up`/`down` reach it
        // through the query field, which leaves them alone while it is a single
        // line, and `enter`/`escape` are passed along by it deliberately.
        KeyBinding::new("up", finder::SelectPrev, Some(finder::CONTEXT)),
        KeyBinding::new("down", finder::SelectNext, Some(finder::CONTEXT)),
        KeyBinding::new("enter", finder::Confirm, Some(finder::CONTEXT)),
        KeyBinding::new("escape", finder::Dismiss, Some(finder::CONTEXT)),
        // Find within the note, on the key every editor and browser uses for it.
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-f", ToggleSearch, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-f", ToggleSearch, Some(CONTEXT)),
        // The find bar's own keys, scoped to its row. Enter walks the matches
        // (shift to go back) and the arrows do the same, since the field is a
        // single line and leaves them alone.
        KeyBinding::new("enter", search::FindNext, Some(search::CONTEXT)),
        KeyBinding::new("shift-enter", search::FindPrev, Some(search::CONTEXT)),
        KeyBinding::new("down", search::FindNext, Some(search::CONTEXT)),
        KeyBinding::new("up", search::FindPrev, Some(search::CONTEXT)),
        KeyBinding::new("escape", search::Dismiss, Some(search::CONTEXT)),
    ]);
}

/// Dragged while the sidebar divider is being pulled. Carries nothing: it only
/// needs to exist so gpui has an active drag to report moves for.
struct SidebarDrag;

impl Render for SidebarDrag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
    }
}

/// The result of a background save attempt.
enum SaveOutcome {
    Saved(Option<FileSig>),
    /// The file changed on disk since we loaded it — we did NOT overwrite.
    Conflict,
    Failed(String),
}

struct SlimNotes {
    focus_handle: FocusHandle,
    settings: Settings,
    vault: Option<PathBuf>,
    tree: Vec<Entry>,
    /// Basename index for `[[wikilink]]` / embed resolution, rebuilt in the
    /// background when the vault changes or the watcher fires.
    index: Option<Rc<VaultIndex>>,
    /// Folder paths the user has expanded in the current vault (folders default
    /// to collapsed). Mirrors `settings.expanded_folders[vault]` for O(1) lookup.
    expanded: HashSet<String>,
    current: Option<PathBuf>,
    /// Disk signature of the open note when it was last loaded or saved, so an
    /// external edit can be detected before autosave clobbers it.
    open_sig: Option<FileSig>,
    editor: Option<Entity<MarkdownEditor>>,
    /// A transient status/error line (save failure, on-disk conflict).
    status: Option<String>,
    /// Set while the sidebar divider is being dragged, which is what puts the
    /// window-wide mouse listeners in place for the duration of the drag.
    resizing_sidebar: bool,
    /// A tree row that is currently a text field: naming a new note or folder,
    /// or renaming an existing one.
    edit: Option<sidebar::SidebarEdit>,
    /// The note finder, while it is open.
    finder: Option<finder::NoteFinder>,
    /// The find-in-note bar, while it is open.
    search: Option<search::NoteSearch>,
    /// The file tree's scroll offset, owned here rather than left inside the
    /// element so the sidebar can scroll itself — to a row being named, and
    /// while an entry is dragged towards an edge.
    tree_scroll: ScrollHandle,
    /// Pixels to scroll the tree per tick while dragging an entry, or `None`
    /// when the pointer is not in either edge zone. Set from the drag's move
    /// events and consumed by the loop `sidebar::drag_scroll` spawns, which is
    /// what keeps the tree moving while the pointer is held still.
    drag_scroll: Option<Pixels>,
    /// Invalidates a running drag-autoscroll loop, so a new drag never ends up
    /// with two loops scrolling at once.
    drag_scroll_gen: u64,
    save_gen: u64,
    _editor_sub: Option<Subscription>,
    _watcher: Option<VaultWatcher>,
}

impl SlimNotes {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let settings = vault::load_settings();
        let vault = settings.vault.clone();
        let expanded = expanded_for(&settings, vault.as_deref());

        // Focus the root so keyboard shortcuts work before any block is edited.
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);

        let mut this = Self {
            focus_handle,
            settings,
            vault,
            tree: Vec::new(),
            index: None,
            expanded,
            current: None,
            open_sig: None,
            editor: None,
            status: None,
            resizing_sidebar: false,
            edit: None,
            finder: None,
            search: None,
            tree_scroll: ScrollHandle::new(),
            drag_scroll: None,
            drag_scroll_gen: 0,
            save_gen: 0,
            _editor_sub: None,
            _watcher: None,
        };
        // Scan the tree + build the index off the UI thread, and start watching.
        this.reload_vault(cx);
        this.start_watcher(cx);
        this.restore_last_note(window, cx);
        this
    }

    /// Reopen the note that was open when this vault was last used. Reading it
    /// is asynchronous, so this does not delay the window appearing.
    fn restore_last_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = last_note_for(&self.settings, self.vault.as_deref()) {
            self.open_note(path, window, cx);
        }
    }

    /// Record the open note as this vault's last note, so a restart reopens it,
    /// and put it at the head of the vault's recent list, which is what the note
    /// finder offers before anything is typed.
    fn remember_note(&mut self) {
        let (Some(vault), Some(current)) = (self.vault.as_deref(), self.current.as_deref()) else {
            return;
        };
        let note = current.to_string_lossy().into_owned();
        let key = vault_key(vault);
        let previous = self.settings.last_note.insert(key.clone(), note.clone());

        let recent = self.settings.recent_notes.entry(key).or_default();
        let already_first = recent.first().map(String::as_str) == Some(note.as_str());
        if !already_first {
            // Reopening a note moves it up rather than appearing twice.
            recent.retain(|p| p != &note);
            recent.insert(0, note.clone());
            recent.truncate(finder::RECENT_NOTES);
        }
        // Only touch the file when something actually changed: this runs on every
        // note switch, and rewriting identical settings buys nothing. Both halves
        // are checked because a settings file written before the recent list
        // existed has a `last_note` and no list to go with it.
        if previous.as_deref() != Some(note.as_str()) || !already_first {
            self.persist_settings();
        }
    }

    /// Forget the open note, so nothing further is written to it. Autosave reads
    /// `current` when it fires, so clearing it is what stops a queued save from
    /// recreating a note that has just been deleted or moved away.
    pub(crate) fn close_note(&mut self) {
        self.current = None;
        self.open_sig = None;
        self.editor = None;
        self._editor_sub = None;
        self.edit = None;
        self.discard_search();
    }

    /// Rescan the vault tree and rebuild the resolver index on the background
    /// executor, then swap the results in. Keeps a large vault from freezing the
    /// UI on open / vault switch / external change.
    fn reload_vault(&mut self, cx: &mut Context<Self>) {
        let Some(root) = self.vault.clone() else {
            self.tree = Vec::new();
            self.index = None;
            return;
        };
        cx.spawn(async move |this, cx| {
            let (tree, index) = cx
                .background_executor()
                .spawn(async move {
                    (vault::read_tree(&root), VaultIndex::build(&root))
                })
                .await;
            this.update(cx, |this, cx| {
                this.tree = tree;
                this.index = Some(Rc::new(index));
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Watch the vault on disk so external edits (git, sync clients, Obsidian)
    /// refresh the tree and are noticed before we overwrite them.
    fn start_watcher(&mut self, cx: &mut Context<Self>) {
        self._watcher = None;
        let Some(root) = self.vault.clone() else { return };
        let Ok((watcher, rx)) = vault::watch(&root) else { return };
        self._watcher = Some(watcher);
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(500))
                    .await;
                let mut changed = false;
                while rx.try_recv().is_ok() {
                    changed = true;
                }
                let alive = this
                    .update(cx, |this, cx| {
                        if changed {
                            this.on_vault_changed(cx);
                        }
                    })
                    .is_ok();
                if !alive {
                    break; // the view was dropped
                }
            }
        })
        .detach();
    }

    /// The vault changed on disk: refresh the tree/index and flag it if the open
    /// note itself was touched externally.
    fn on_vault_changed(&mut self, cx: &mut Context<Self>) {
        self.reload_vault(cx);
        if let (Some(path), Some(prev)) = (self.current.clone(), self.open_sig) {
            if let Some(cur) = vault::file_sig(&path) {
                if cur != prev {
                    self.status =
                        Some("Note changed on disk — save will not overwrite it".to_string());
                    cx.notify();
                }
            }
        }
    }

    pub(crate) fn persist_settings(&mut self) {
        if let Err(e) = vault::save_settings(&self.settings) {
            self.status = Some(format!("Could not save settings: {e}"));
        }
    }

    /// Vault-relative breadcrumb for the current note, e.g. "Projects / Alpha".
    fn breadcrumb(&self) -> String {
        let Some(current) = &self.current else {
            return String::new();
        };
        let rel = self
            .vault
            .as_deref()
            .and_then(|v| current.strip_prefix(v).ok())
            .unwrap_or(current);
        rel.components()
            .map(|c| c.as_os_str().to_string_lossy())
            .map(|s| s.strip_suffix(".md").map(str::to_string).unwrap_or_else(|| s.into_owned()))
            .collect::<Vec<_>>()
            .join("  /  ")
    }

    /// Open the native folder picker and switch to the chosen vault.
    pub(crate) fn open_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select vault folder".into()),
        });

        let view = cx.entity();
        cx.spawn_in(window, async move |_, cx| {
            let path = paths.await.ok()?.ok()??.into_iter().next()?;
            cx.update(|window, cx| {
                view.update(cx, |this, cx| this.set_vault(path, window, cx));
            })
            .ok()
        })
        .detach();
    }

    fn set_vault(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
        self.settings.vault = Some(path.clone());
        self.persist_settings();
        self.expanded = expanded_for(&self.settings, Some(&path));
        self.vault = Some(path);
        self.close_note();
        self.status = None;
        self.reload_vault(cx);
        self.start_watcher(cx);
        // Switching to a vault lands where the user left it, as on startup.
        self.restore_last_note(window, cx);
        cx.notify();
    }

    /// Snapshot the open note's content and write it atomically off the UI
    /// thread, refusing to overwrite if the file changed on disk since we loaded
    /// it. Surfaces failures/conflicts via `status`.
    fn save(&mut self, cx: &mut Context<Self>) {
        let (Some(path), Some(editor)) = (self.current.clone(), self.editor.clone()) else {
            return;
        };
        let content = editor.read(cx).text().to_string();
        let prev_sig = self.open_sig;
        let done_path = path.clone(); // which note this save applies to
        cx.spawn(async move |this, cx| {
            let outcome = cx
                .background_executor()
                .spawn(async move {
                    // Conflict: the file changed on disk since we loaded/saved it.
                    if let (Some(prev), Some(cur)) = (prev_sig, vault::file_sig(&path)) {
                        if cur != prev {
                            return SaveOutcome::Conflict;
                        }
                    }
                    match vault::write_note(&path, &content) {
                        Ok(()) => SaveOutcome::Saved(vault::file_sig(&path)),
                        Err(e) => SaveOutcome::Failed(e.to_string()),
                    }
                })
                .await;
            this.update(cx, |this, cx| this.finish_save(&done_path, outcome, cx))
                .ok();
        })
        .detach();
    }

    /// Apply a save's outcome. `path` is the note the save was *for*: a save runs
    /// in the background and can land after the user switched notes, so a stale
    /// completion must not stamp its signature (or its error) onto the note that
    /// is open now — that would make the next autosave see a bogus conflict and
    /// silently refuse to write.
    fn finish_save(&mut self, path: &Path, outcome: SaveOutcome, cx: &mut Context<Self>) {
        if self.current.as_deref() != Some(path) {
            return;
        }
        match outcome {
            SaveOutcome::Saved(sig) => {
                self.open_sig = sig;
                if self
                    .status
                    .as_deref()
                    .is_some_and(|s| s.starts_with("Save failed"))
                {
                    self.status = None;
                }
            }
            SaveOutcome::Conflict => {
                self.status =
                    Some("Note changed on disk — save skipped to avoid overwriting".to_string());
            }
            SaveOutcome::Failed(e) => {
                self.status = Some(format!("Save failed: {e}"));
            }
        }
        cx.notify();
    }

    /// Debounced autosave: write ~400ms after the last edit.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_gen = self.save_gen.wrapping_add(1);
        let generation = self.save_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            this.update(cx, |this, cx| {
                if this.save_gen == generation {
                    this.save(cx);
                }
            })
            .ok();
        })
        .detach();
    }

    pub(crate) fn open_note(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        // Opening a note is the clearest sign a half-typed name in the tree has
        // been abandoned, and blur alone does not clear it.
        self.edit = None;
        self.save(cx); // flush the previous note (async, uses its captured path)
        let view = cx.entity();
        let read_path = path.clone();
        cx.spawn_in(window, async move |_, cx| {
            let (content, sig) = cx
                .background_executor()
                .spawn(async move {
                    (vault::read_note(&read_path).ok(), vault::file_sig(&read_path))
                })
                .await;
            let Some(content) = content else {
                view.update(cx, |this, cx| {
                    this.status = Some("Could not read note".to_string());
                    cx.notify();
                })
                .ok();
                return;
            };
            cx.update(|window, cx| {
                view.update(cx, |this, cx| {
                    this.install_editor(path, content, sig, window, cx);
                });
            })
            .ok();
        })
        .detach();
    }

    fn install_editor(
        &mut self,
        path: PathBuf,
        content: String,
        sig: Option<FileSig>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let base_dir = path.parent().map(|p| p.to_path_buf());
        let vault_dir = self.vault.clone();
        let editor = cx.new(|cx| MarkdownEditor::new(content, base_dir, vault_dir, window, cx));
        let sub = cx.subscribe_in(
            &editor,
            window,
            |this, _editor, event: &EditorEvent, window, cx| match event {
                EditorEvent::Changed => this.schedule_save(cx),
                EditorEvent::OpenNote(name) => this.open_note_by_name(name, window, cx),
                EditorEvent::AddImages(images) => this.add_images(images.clone(), cx),
            },
        );
        // The bar was searching the note being replaced, and the new editor
        // takes focus for itself.
        self.discard_search();
        self.editor = Some(editor);
        self._editor_sub = Some(sub);
        self.current = Some(path);
        self.open_sig = sig;
        self.status = None;
        self.remember_note();
        // Keep the tree pointing at the note that is open, however it was
        // reached — the finder and `[[wikilinks]]` can both land somewhere the
        // sidebar is not showing.
        self.reveal_note_in_tree();
        cx.notify();
    }

    /// Store images pasted or dropped into the note, then embed them.
    ///
    /// The editor has already put the caret where they belong and does not touch
    /// the buffer until the files exist, so the insertion happens here once the
    /// write is done. That write runs on the background executor: a dropped file
    /// can be large, and typing must not stall behind it.
    fn add_images(&mut self, images: Arc<Vec<ImageSource>>, cx: &mut Context<Self>) {
        let (Some(vault), Some(current), Some(editor)) =
            (self.vault.clone(), self.current.clone(), self.editor.clone())
        else {
            self.status = Some("Open a note in a vault to add images".to_string());
            cx.notify();
            return;
        };
        let note_dir = current.parent().unwrap_or(&vault).to_path_buf();
        cx.spawn(async move |this, cx| {
            let (names, error) = cx
                .background_executor()
                .spawn(async move { store_images(&vault, &note_dir, &images) })
                .await;
            editor
                .update(cx, |editor, cx| editor.insert_image_embeds(&names, cx))
                .ok();
            this.update(cx, |this, cx| {
                this.status = error;
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Open a note by its `[[wikilink]]` name, resolved across the vault.
    fn open_note_by_name(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(path) = self.index.as_ref().and_then(|i| i.resolve_note(name)) {
            self.open_note(path, window, cx);
        }
    }

    fn on_save(&mut self, _: &Save, _window: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
    }

    /// Show or hide the sidebar, remembering the choice.
    fn toggle_sidebar(&mut self, cx: &mut Context<Self>) {
        self.settings.sidebar_collapsed = !self.settings.sidebar_collapsed;
        self.persist_settings();
        cx.notify();
    }

    fn on_toggle_sidebar(&mut self, _: &ToggleSidebar, _window: &mut Window, cx: &mut Context<Self>) {
        self.toggle_sidebar(cx);
    }

    /// The sidebar's current width, clamped to the range the handle allows in
    /// case a hand-edited settings file asks for something unusable.
    fn sidebar_width(&self) -> Pixels {
        let w = self.settings.sidebar_width.unwrap_or(DEFAULT_SIDEBAR_WIDTH);
        px(w.clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH))
    }

    /// Start dragging the divider. The move/up listeners live on the root
    /// element rather than the divider, so the pointer can run ahead of the
    /// divider mid-drag without the drag stalling.
    fn begin_sidebar_drag(&mut self, cx: &mut Context<Self>) {
        self.resizing_sidebar = true;
        cx.notify();
    }

    /// Track the divider. `x` is a window coordinate, and the sidebar starts at
    /// the window's left edge, so it *is* the new width.
    fn drag_sidebar(&mut self, x: Pixels, cx: &mut Context<Self>) {
        if !self.resizing_sidebar {
            return;
        }
        let width = f32::from(x).clamp(SIDEBAR_MIN_WIDTH, SIDEBAR_MAX_WIDTH);
        if self.settings.sidebar_width != Some(width) {
            self.settings.sidebar_width = Some(width);
            cx.notify();
        }
    }

    /// Finish the drag, writing the settings file once rather than on every one
    /// of the hundreds of mouse-move events a drag produces.
    fn end_sidebar_drag(&mut self, cx: &mut Context<Self>) {
        if !self.resizing_sidebar {
            return;
        }
        self.resizing_sidebar = false;
        self.persist_settings();
        cx.notify();
    }

    /// The draggable divider between sidebar and note. Wider than the line it
    /// draws so it is comfortable to grab.
    ///
    /// Tracking uses `on_drag_move` rather than `on_mouse_move`, which never
    /// fires here: a plain move listener is gated on its hitbox being hovered,
    /// and gpui reports nothing as hovered while a drag is active — so the
    /// pointer leaving the 5px line would end the resize. `on_drag_move` is
    /// gpui's answer for exactly this (its own docs cite resizing), and reports
    /// every move for as long as the drag lives, wherever the pointer goes.
    fn render_sidebar_divider(&self, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("sidebar-divider")
            .w(px(5.))
            .h_full()
            .flex_shrink_0()
            .border_l_1()
            .border_color(cx.theme().sidebar_border)
            .cursor_col_resize()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.begin_sidebar_drag(cx)),
            )
            // Arms gpui's drag machinery, which is what makes `on_drag_move`
            // fire. The dragged "preview" is deliberately an empty element: the
            // divider itself follows the pointer, so a ghost would just smear.
            .on_drag(SidebarDrag, |_, _, _, cx| cx.new(|_| SidebarDrag))
            .on_drag_move(cx.listener(
                |this, ev: &DragMoveEvent<SidebarDrag>, _, cx| {
                    this.drag_sidebar(ev.event.position.x, cx);
                },
            ))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_sidebar_drag(cx)),
            )
            .on_mouse_up_out(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| this.end_sidebar_drag(cx)),
            )
    }

    /// The note pane: a header that is always present, over the editor or the
    /// empty-state message.
    ///
    /// The header renders even with no note open because it carries the sidebar
    /// toggle — hiding it there would let the user collapse the sidebar on an
    /// empty vault and have no way back short of the keyboard.
    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let warn = gpui::rgb(0xdd8a8a);
        let collapsed = self.settings.sidebar_collapsed;

        let toggle = Button::new("toggle-sidebar")
            .ghost()
            .xsmall()
            .icon(if collapsed {
                IconName::PanelLeftOpen
            } else {
                IconName::PanelLeftClose
            })
            .tooltip(if collapsed {
                "Show sidebar"
            } else {
                "Hide sidebar"
            })
            .on_click(cx.listener(|this, _, _, cx| this.toggle_sidebar(cx)));

        let mut header = h_flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .h(px(40.))
            .pl(px(8.))
            .pr(px(40.))
            .overflow_hidden()
            .border_b_1()
            .border_color(border)
            .text_size(px(13.))
            .text_color(muted)
            .child(
                h_flex()
                    .min_w(px(0.))
                    .items_center()
                    .gap(px(8.))
                    .child(toggle)
                    // Leaves the breadcrumb at the same inset as the editor text.
                    .child(div().min_w(px(0.)).truncate().child(SharedString::from(self.breadcrumb()))),
            );
        if let Some(status) = &self.status {
            header = header.child(
                div()
                    // Allowed to shrink and ellipsize: a long message (a failed
                    // save naming a path) must not push the breadcrumb off the
                    // left edge or spill across it.
                    .min_w(px(0.))
                    .max_w(px(420.))
                    .overflow_hidden()
                    .text_ellipsis()
                    .text_color(warn)
                    .child(SharedString::from(status.clone())),
            );
        }

        let body = match &self.editor {
            // The editor manages its own scrolling; clip it to this pane.
            Some(editor) => div()
                .flex_1()
                .min_h(px(0.))
                .overflow_hidden()
                .child(editor.clone()),
            None => {
                let message = if self.vault.is_none() {
                    "Open a vault to get started."
                } else {
                    "Select a note to start."
                };
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(muted)
                    .child(SharedString::from(message))
            }
        };

        v_flex()
            .flex_1()
            .h_full()
            .min_w(px(0.))
            .child(header)
            .children(self.render_search_bar(cx))
            .child(body)
            .into_any_element()
    }
}

/// Put each image in the vault's attachment folder, returning the file names to
/// embed and the first failure's message, if any. Pure filesystem work, called
/// from the background executor.
fn store_images(
    vault: &Path,
    note_dir: &Path,
    images: &[ImageSource],
) -> (Vec<String>, Option<String>) {
    let dir = vault::attachment_dir(vault, note_dir);
    let mut names = Vec::new();
    let mut error = None;
    for image in images {
        let stored = match image {
            ImageSource::Clipboard { ext, bytes } => {
                vault::save_attachment(&dir, &vault::pasted_image_stem(), ext, bytes)
            }
            // A file already in the vault is embedded where it lies — dragging a
            // note's own image around shouldn't quietly duplicate it.
            ImageSource::File(src) if src.starts_with(vault) => Ok(src.clone()),
            ImageSource::File(src) => vault::import_attachment(&dir, src),
        };
        match stored {
            // The embed resolves by basename, vault-wide, so that is all it needs.
            Ok(path) => names.extend(path.file_name().map(|n| n.to_string_lossy().into_owned())),
            Err(e) => {
                if error.is_none() {
                    error = Some(format!("Could not add image: {e}"));
                }
            }
        }
    }
    (names, error)
}

impl Render for SlimNotes {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = cx.theme().background;
        let fg = cx.theme().foreground;
        let mut root = gpui_component::h_flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_toggle_note_finder))
            .on_action(cx.listener(Self::on_toggle_search))
            // What the note finder's overlay is positioned against.
            .relative()
            .size_full()
            .bg(bg)
            .text_color(fg);

        if !self.settings.sidebar_collapsed {
            root = root
                .child(
                    div()
                        .w(self.sidebar_width())
                        .h_full()
                        .flex_shrink_0()
                        .child(self.render_sidebar(cx)),
                )
                .child(self.render_sidebar_divider(cx));
        }
        root = root.child(self.render_content(cx));
        // Last, so it paints over everything — including, for mouse events,
        // over the editor's window-wide listeners.
        root = root.children(self.render_note_finder(cx));

        root
    }
}

fn main() {
    let args = match cli::parse(
        std::env::args().skip(1),
        std::env::var("SLIMNOTES_APP_ID").ok(),
    ) {
        cli::Parsed::Run(args) => args,
        cli::Parsed::Usage => {
            println!("{}", cli::USAGE);
            return;
        }
        cli::Parsed::Error(msg) => {
            eprintln!("slimnotes: {msg}\n\n{}", cli::USAGE);
            std::process::exit(2);
        }
    };

    let app = Application::new().with_assets(Assets);

    app.run(move |cx: &mut App| {
        gpui_component::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        apply_palette(cx);
        apply_fonts(cx);
        bind_keys(cx);

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(750.)), cx)),
            // What a compositor matches window rules against; see `cli`.
            app_id: Some(args.app_id.clone()),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                // gpui leaves the title unset on Wayland, which shows up as a
                // nameless window in taskbars and window switchers. The app_id
                // doubles as the name so both agree when it has been overridden.
                window.set_window_title(&args.app_id);
                let view = cx.new(|cx| SlimNotes::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })?;
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
