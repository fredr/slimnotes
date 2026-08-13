#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod vault;

use std::path::Path;

use gpui::{
    div, px, rgb, size, AnyElement, App, AppContext, Application, Context, Entity,
    InteractiveElement, IntoElement, ParentElement, Render, SharedString,
    StatefulInteractiveElement, Styled, Subscription, Window, WindowBounds, WindowOptions,
};
use gpui_component::{
    input::{Input, InputEvent, InputState},
    text::TextView,
    v_flex, Root,
};
use gpui_component_assets::Assets;
use slimnotes_core::Entry;

const BG: u32 = 0x1e1e1e;
const BG_ALT: u32 = 0x252525;
const BG_HOVER: u32 = 0x2f2f2f;
const TEXT: u32 = 0xdcddde;
const TEXT_DIM: u32 = 0x8a8a8a;
const WHITE: u32 = 0xffffff;

struct SlimNotes {
    tree: Vec<Entry>,
    current: Option<String>,
    blocks: Vec<String>,
    active: Option<usize>,
    editor: Option<Entity<InputState>>,
    _subs: Vec<Subscription>,
}

impl SlimNotes {
    fn new(_window: &mut Window, _cx: &mut Context<Self>) -> Self {
        let settings = vault::load_settings();
        let tree = settings
            .vault
            .as_deref()
            .map(|v| vault::read_tree(Path::new(v)))
            .unwrap_or_default();

        Self {
            tree,
            current: None,
            blocks: Vec::new(),
            active: None,
            editor: None,
            _subs: Vec::new(),
        }
    }

    fn save(&self) {
        if let Some(path) = &self.current {
            let content = vault::join_blocks(&self.blocks);
            let _ = vault::write_note(Path::new(path), &content);
        }
    }

    fn open_note(&mut self, path: &str, _window: &mut Window, cx: &mut Context<Self>) {
        self.save();
        if let Ok(content) = vault::read_note(Path::new(path)) {
            self.blocks = vault::split_blocks(&content);
            self.current = Some(path.to_string());
            self.active = None;
            self.editor = None;
            self._subs.clear();
            cx.notify();
        }
    }

    fn activate_block(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.save();
        let content = self.blocks.get(index).cloned().unwrap_or_default();

        let editor = cx.new(|cx| {
            InputState::new(window, cx)
                .code_editor("markdown")
                .auto_grow(1, 60)
                .default_value(content)
        });

        let sub = cx.subscribe_in(
            &editor,
            window,
            move |this, state, ev: &InputEvent, _window, cx| match ev {
                InputEvent::Change => {
                    let value = state.read(cx).value().to_string();
                    if let Some(i) = this.active {
                        if let Some(block) = this.blocks.get_mut(i) {
                            *block = value;
                        }
                    }
                }
                InputEvent::Blur => this.save(),
                _ => {}
            },
        );

        editor.update(cx, |state, cx| state.focus(window, cx));

        self.editor = Some(editor);
        self._subs = vec![sub];
        self.active = Some(index);
        cx.notify();
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        v_flex()
            .w(px(260.))
            .h_full()
            .bg(rgb(BG_ALT))
            .child(
                div()
                    .p_3()
                    .text_size(px(15.))
                    .child(SharedString::from("slimnotes")),
            )
            .child(
                v_flex()
                    .p_1()
                    .gap_1()
                    .children(self.render_tree(&self.tree, 0, cx)),
            )
    }

    fn render_tree(
        &self,
        entries: &[Entry],
        depth: usize,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut out = Vec::new();
        let indent = px(8.0 + depth as f32 * 12.0);

        for entry in entries {
            if entry.is_dir {
                out.push(
                    div()
                        .pl(indent)
                        .pr_2()
                        .py_1()
                        .text_color(rgb(TEXT_DIM))
                        .child(SharedString::from(format!("▾ {}", entry.name)))
                        .into_any_element(),
                );
                out.extend(self.render_tree(&entry.children, depth + 1, cx));
            } else {
                let path = entry.path.clone();
                let is_current = self.current.as_deref() == Some(path.as_str());
                let label = entry.name.strip_suffix(".md").unwrap_or(&entry.name).to_string();

                let mut row = div()
                    .id(SharedString::from(format!("file-{path}")))
                    .pl(indent)
                    .pr_2()
                    .py_1()
                    .rounded_md()
                    .cursor_pointer()
                    .child(SharedString::from(label))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.open_note(&path, window, cx);
                    }));

                if is_current {
                    row = row.bg(rgb(BG_HOVER)).text_color(rgb(WHITE));
                }
                out.push(row.into_any_element());
            }
        }
        out
    }

    fn render_content(&self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        if self.current.is_none() {
            return v_flex()
                .flex_1()
                .h_full()
                .items_center()
                .justify_center()
                .text_color(rgb(TEXT_DIM))
                .child(SharedString::from("Select a note to start."))
                .into_any_element();
        }

        let mut items: Vec<AnyElement> = Vec::new();
        for (i, block) in self.blocks.iter().enumerate() {
            if self.active == Some(i) {
                if let Some(editor) = &self.editor {
                    items.push(Input::new(editor).into_any_element());
                    continue;
                }
            }

            let markdown = TextView::markdown(
                SharedString::from(format!("md-{i}")),
                SharedString::from(block.clone()),
                window,
                cx,
            );

            let row = div()
                .id(SharedString::from(format!("block-{i}")))
                .px_2()
                .py_1()
                .min_h(px(28.))
                .rounded_md()
                .cursor_pointer()
                .child(markdown)
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.activate_block(i, window, cx);
                }));
            items.push(row.into_any_element());
        }

        div()
            .id(SharedString::from("content-scroll"))
            .flex_1()
            .h_full()
            .overflow_y_scroll()
            .child(
                v_flex()
                    .w_full()
                    .max_w(px(760.))
                    .p_6()
                    .gap_1()
                    .children(items),
            )
            .into_any_element()
    }
}

impl Render for SlimNotes {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        gpui_component::h_flex()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT))
            .child(self.render_sidebar(cx))
            .child(self.render_content(window, cx))
    }
}

fn main() {
    let app = Application::new().with_assets(Assets);

    app.run(move |cx: &mut App| {
        gpui_component::init(cx);

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
