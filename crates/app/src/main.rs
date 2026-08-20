#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
#![allow(clippy::collapsible_if)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::time::Duration;

use gpui::{
    actions, div, px, size, AnyElement, App, AppContext, Application, Context, Entity, FocusHandle,
    InteractiveElement, IntoElement, KeyBinding, ParentElement, PathPromptOptions, Render,
    SharedString, StatefulInteractiveElement, Styled, Subscription, Window, WindowBounds,
    WindowOptions,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex, v_flex, ActiveTheme, Disableable, IconName, Root, Sizable, Theme, ThemeMode,
};
use gpui_component_assets::Assets;
use slimnotes_editor::{EditorEvent, MarkdownEditor};
use slimnotes_vault::{self as vault, Entry, FileSig, Settings, VaultIndex, VaultWatcher};

const CONTEXT: &str = "SlimNotes";

// Save the current note.
actions!(slimnotes, [Save]);

/// The set of expanded folder paths saved for `vault` (empty if none saved, so
/// folders start collapsed).
fn expanded_for(settings: &Settings, vault: Option<&Path>) -> HashSet<String> {
    vault
        .map(|v| v.to_string_lossy().into_owned())
        .and_then(|v| settings.expanded_folders.get(&v))
        .map(|f| f.iter().cloned().collect())
        .unwrap_or_default()
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

fn bind_keys(cx: &mut App) {
    cx.bind_keys([
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-s", Save, Some(CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-s", Save, Some(CONTEXT)),
    ]);
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
            save_gen: 0,
            _editor_sub: None,
            _watcher: None,
        };
        // Scan the tree + build the index off the UI thread, and start watching.
        this.reload_vault(cx);
        this.start_watcher(cx);
        this
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

    fn toggle_folder(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.expanded.remove(path) {
            self.expanded.insert(path.to_string());
        }
        // Persist the new expand/collapse state for this vault.
        if let Some(vault) = self.vault.as_ref().map(|v| v.to_string_lossy().into_owned()) {
            let mut folders: Vec<String> = self.expanded.iter().cloned().collect();
            folders.sort();
            self.settings.expanded_folders.insert(vault, folders);
            self.persist_settings();
        }
        cx.notify();
    }

    fn persist_settings(&mut self) {
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
    fn open_vault(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let paths = cx.prompt_for_paths(PathPromptOptions {
            files: false,
            directories: true,
            multiple: false,
            prompt: Some("Select vault folder".into()),
        });

        let view = cx.entity();
        cx.spawn_in(window, async move |_, cx| {
            let path = paths.await.ok()?.ok()??.into_iter().next()?;
            cx.update(|_window, cx| {
                view.update(cx, |this, cx| this.set_vault(path, cx));
            })
            .ok()
        })
        .detach();
    }

    fn set_vault(&mut self, path: PathBuf, cx: &mut Context<Self>) {
        self.save(cx);
        self.settings.vault = Some(path.clone());
        self.persist_settings();
        self.expanded = expanded_for(&self.settings, Some(&path));
        self.vault = Some(path);
        self.current = None;
        self.open_sig = None;
        self.editor = None;
        self._editor_sub = None;
        self.status = None;
        self.reload_vault(cx);
        self.start_watcher(cx);
        cx.notify();
    }

    /// Open a native save dialog to name/place a new note, then create and open
    /// it. Naming here also stands in for the (not-yet-built) rename feature.
    fn new_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(vault) = self.vault.clone() else {
            return;
        };
        let path = cx.prompt_for_new_path(&vault, Some("Untitled.md"));

        let view = cx.entity();
        cx.spawn_in(window, async move |_, cx| {
            let path = path.await.ok()?.ok()??;
            cx.update(|window, cx| {
                view.update(cx, |this, cx| this.create_and_open(path, window, cx));
            })
            .ok()
        })
        .detach();
    }

    fn create_and_open(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let path = if path.extension().is_some() {
            path
        } else {
            path.with_extension("md")
        };
        // Create the file if it's new; don't clobber an existing note.
        if !path.exists() {
            if let Err(e) = vault::write_note(&path, "") {
                self.status = Some(format!("Could not create note: {e}"));
                cx.notify();
                return;
            }
        }
        self.reload_vault(cx);
        self.open_note(path, window, cx);
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

    fn open_note(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
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
            },
        );
        self.editor = Some(editor);
        self._editor_sub = Some(sub);
        self.current = Some(path);
        self.open_sig = sig;
        self.status = None;
        cx.notify();
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

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let sidebar_bg = cx.theme().sidebar;
        let border = cx.theme().sidebar_border;
        let fg = cx.theme().sidebar_foreground;
        let muted = cx.theme().muted_foreground;
        let vault_name = self
            .vault
            .as_deref()
            .and_then(|v| v.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "No vault".to_string());

        v_flex()
            .flex_shrink_0()
            .w(px(240.))
            .h_full()
            .bg(sidebar_bg)
            .border_r_1()
            .border_color(border)
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
                            .truncate()
                            .text_size(px(12.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(muted)
                            .child(SharedString::from(vault_name)),
                    )
                    .child(
                        h_flex()
                            .gap_1()
                            .child(
                                Button::new("new-note")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::Plus)
                                    .disabled(self.vault.is_none())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.new_note(window, cx);
                                    })),
                            )
                            .child(
                                Button::new("open-vault")
                                    .ghost()
                                    .xsmall()
                                    .icon(IconName::FolderOpen)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_vault(window, cx);
                                    })),
                            ),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from("tree-scroll"))
                    .flex_1()
                    .overflow_y_scroll()
                    .px_2()
                    .pb_4()
                    .child(v_flex().gap_px().children(self.render_tree(&self.tree, 0, cx))),
            )
    }

    fn render_tree(
        &self,
        entries: &[Entry],
        depth: usize,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        // Read colors into owned values so we don't hold a borrow of `cx`
        // across the `&mut cx` recursion / listener calls below.
        let hover = cx.theme().sidebar_accent.alpha(0.5);
        let active_bg = cx.theme().sidebar_accent;
        let active_fg = cx.theme().sidebar_accent_foreground;
        let muted = cx.theme().muted_foreground;
        let fg = cx.theme().sidebar_foreground;

        let mut out = Vec::new();
        let pad_left = px(6.0 + depth as f32 * 12.0);

        for entry in entries {
            if entry.is_dir {
                let path = entry.path.to_string_lossy().into_owned();
                let collapsed = !self.expanded.contains(&path);
                let chevron = if collapsed { "▸" } else { "▾" };
                let name = entry.name.clone();
                let toggle_path = path.clone();
                out.push(
                    h_flex()
                        .id(SharedString::from(format!("dir-{path}")))
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
                        .child(
                            div()
                                .w(px(12.))
                                .text_size(px(9.))
                                .child(SharedString::from(chevron)),
                        )
                        .child(div().flex_1().truncate().child(SharedString::from(name)))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            this.toggle_folder(&toggle_path, cx);
                        }))
                        .into_any_element(),
                );
                if !collapsed {
                    out.extend(self.render_tree(&entry.children, depth + 1, cx));
                }
            } else {
                let path = entry.path.clone();
                let id = path.to_string_lossy().into_owned();
                let is_current = self.current.as_deref() == Some(path.as_path());
                let label = entry.name.strip_suffix(".md").unwrap_or(&entry.name).to_string();

                let mut row = h_flex()
                    .id(SharedString::from(format!("file-{id}")))
                    .items_center()
                    .pl(pad_left)
                    .pr_2()
                    .py(px(3.))
                    .rounded(px(4.))
                    .text_size(px(13.))
                    .text_color(fg)
                    .cursor_pointer()
                    .hover(move |s| s.bg(hover))
                    .child(div().flex_1().truncate().child(SharedString::from(label)))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_note(path.clone(), window, cx);
                    }));

                if is_current {
                    row = row.bg(active_bg).text_color(active_fg);
                }
                out.push(row.into_any_element());
            }
        }
        out
    }

    fn render_content(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let warn = gpui::rgb(0xdd8a8a);

        let Some(editor) = &self.editor else {
            let message = if self.vault.is_none() {
                "Open a vault to get started."
            } else {
                "Select a note to start."
            };
            return v_flex()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .text_color(muted)
                .child(SharedString::from(message))
                .into_any_element();
        };

        let mut header = h_flex()
            .flex_shrink_0()
            .items_center()
            .justify_between()
            .h(px(40.))
            .px(px(40.))
            .border_b_1()
            .border_color(border)
            .text_size(px(13.))
            .text_color(muted)
            .child(SharedString::from(self.breadcrumb()));
        if let Some(status) = &self.status {
            header = header.child(div().text_color(warn).child(SharedString::from(status.clone())));
        }

        v_flex()
            .flex_1()
            .h_full()
            .min_w(px(0.))
            .child(header)
            .child(
                // The editor manages its own scrolling; clip it to this pane.
                div()
                    .flex_1()
                    .min_h(px(0.))
                    .overflow_hidden()
                    .child(editor.clone()),
            )
            .into_any_element()
    }
}

impl Render for SlimNotes {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let bg = cx.theme().background;
        let fg = cx.theme().foreground;
        gpui_component::h_flex()
            .key_context(CONTEXT)
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::on_save))
            .size_full()
            .bg(bg)
            .text_color(fg)
            .child(self.render_sidebar(cx))
            .child(self.render_content(cx))
    }
}

fn main() {
    let app = Application::new().with_assets(Assets);

    app.run(move |cx: &mut App| {
        gpui_component::init(cx);
        Theme::change(ThemeMode::Dark, None, cx);
        apply_palette(cx);
        bind_keys(cx);

        let window_options = WindowOptions {
            window_bounds: Some(WindowBounds::centered(size(px(1100.), px(750.)), cx)),
            ..Default::default()
        };

        cx.spawn(async move |cx| {
            cx.open_window(window_options, |window, cx| {
                let view = cx.new(|cx| SlimNotes::new(window, cx));
                cx.new(|cx| Root::new(view, window, cx))
            })?;
            Ok::<_, anyhow::Error>(())
        })
        .detach();
    });
}
