#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod vault;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
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
use slimnotes_core::{Entry, Settings};
use slimnotes_editor::{EditorEvent, MarkdownEditor};

const CONTEXT: &str = "SlimNotes";

// Save the current note.
actions!(slimnotes, [Save]);

/// Override gpui-component's default dark palette (near-black / near-white,
/// high contrast) with a softer, Obsidian-like dark scheme.
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
    // Accent / links (Obsidian's purple)
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

struct SlimNotes {
    focus_handle: FocusHandle,
    vault: Option<String>,
    tree: Vec<Entry>,
    collapsed: HashSet<String>,
    current: Option<String>,
    editor: Option<Entity<MarkdownEditor>>,
    save_gen: u64,
    _editor_sub: Option<Subscription>,
}

impl SlimNotes {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let vault = vault::load_settings().vault;
        let tree = vault
            .as_deref()
            .map(|v| vault::read_tree(Path::new(v)))
            .unwrap_or_default();

        // Focus the root so keyboard shortcuts work before any block is edited.
        let focus_handle = cx.focus_handle();
        focus_handle.focus(window);

        Self {
            focus_handle,
            vault,
            tree,
            collapsed: HashSet::new(),
            current: None,
            editor: None,
            save_gen: 0,
            _editor_sub: None,
        }
    }

    fn toggle_folder(&mut self, path: &str, cx: &mut Context<Self>) {
        if !self.collapsed.remove(path) {
            self.collapsed.insert(path.to_string());
        }
        cx.notify();
    }

    /// Vault-relative breadcrumb for the current note, e.g. "Projects / Alpha".
    fn breadcrumb(&self) -> String {
        let Some(current) = &self.current else {
            return String::new();
        };
        let rel = self
            .vault
            .as_deref()
            .and_then(|v| Path::new(current).strip_prefix(v).ok())
            .unwrap_or_else(|| Path::new(current));
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
        let path_str = path.to_string_lossy().into_owned();
        vault::save_settings(&Settings {
            vault: Some(path_str.clone()),
        });

        self.tree = vault::read_tree(&path);
        self.vault = Some(path_str);
        self.current = None;
        self.editor = None;
        self._editor_sub = None;
        cx.notify();
    }

    /// Open a native save dialog to name/place a new note, then create and open
    /// it. Naming here also stands in for the (not-yet-built) rename feature.
    fn new_note(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(vault) = self.vault.clone() else {
            return;
        };
        let path = cx.prompt_for_new_path(Path::new(&vault), Some("Untitled.md"));

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
            let _ = vault::write_note(&path, "");
        }
        if let Some(vault) = self.vault.clone() {
            self.tree = vault::read_tree(Path::new(&vault));
        }
        self.open_note(&path.to_string_lossy(), window, cx);
    }

    fn save(&self, cx: &App) {
        if let (Some(path), Some(editor)) = (&self.current, &self.editor) {
            let content = editor.read(cx).text().to_string();
            let _ = vault::write_note(Path::new(path), &content);
        }
    }

    /// Debounced autosave: write ~400ms after the last edit.
    fn schedule_save(&mut self, cx: &mut Context<Self>) {
        self.save_gen = self.save_gen.wrapping_add(1);
        let generation = self.save_gen;
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(400))
                .await;
            let _ = this.update(cx, |this, cx| {
                if this.save_gen == generation {
                    this.save(cx);
                }
            });
        })
        .detach();
    }

    fn open_note(&mut self, path: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.save(cx);
        if let Ok(content) = vault::read_note(Path::new(path)) {
            let base_dir = Path::new(path).parent().map(|p| p.to_path_buf());
            let vault_dir = self.vault.as_ref().map(PathBuf::from);
            let editor =
                cx.new(|cx| MarkdownEditor::new(content, base_dir, vault_dir, window, cx));
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
            self.current = Some(path.to_string());
            cx.notify();
        }
    }

    /// Open a note by its `[[wikilink]]` name, resolved across the vault.
    fn open_note_by_name(&mut self, name: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(vault) = self.vault.clone() else { return };
        if let Some(path) = vault::resolve_note(Path::new(&vault), name) {
            self.open_note(&path.to_string_lossy(), window, cx);
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
            .and_then(|v| Path::new(v).file_name())
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
                let path = entry.path.clone();
                let collapsed = self.collapsed.contains(&path);
                let chevron = if collapsed { "▸" } else { "▾" };
                let name = entry.name.clone();
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
                            this.toggle_folder(&path, cx);
                        }))
                        .into_any_element(),
                );
                if !collapsed {
                    out.extend(self.render_tree(&entry.children, depth + 1, cx));
                }
            } else {
                let path = entry.path.clone();
                let is_current = self.current.as_deref() == Some(path.as_str());
                let label = entry.name.strip_suffix(".md").unwrap_or(&entry.name).to_string();

                let mut row = h_flex()
                    .id(SharedString::from(format!("file-{path}")))
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
                        this.open_note(&path, window, cx);
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

        v_flex()
            .flex_1()
            .h_full()
            .min_w(px(0.))
            .child(
                h_flex()
                    .flex_shrink_0()
                    .items_center()
                    .h(px(40.))
                    .px(px(40.))
                    .border_b_1()
                    .border_color(border)
                    .text_size(px(13.))
                    .text_color(muted)
                    .child(SharedString::from(self.breadcrumb())),
            )
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
