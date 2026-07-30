//! The live-preview markdown editor.
//!
//! A note is split into *blocks* (paragraphs, headings, lists, fenced code —
//! separated by blank lines). Every block is rendered as HTML, except the one
//! block the cursor is in, which is shown as an editable `<textarea>` with the
//! raw markdown source. This reproduces Obsidian's "Live Preview" feel: put
//! the cursor on something and you see the markdown; move away and it renders.

use leptos::prelude::*;
use pulldown_cmark::{html, Options, Parser};

use crate::app::{save_note, AppState};

/// Split markdown text into blocks on blank lines, keeping fenced code blocks
/// (```` ``` ```` / `~~~`) intact even when they contain blank lines.
pub fn split_blocks(text: &str) -> Vec<String> {
    let mut blocks: Vec<String> = Vec::new();
    let mut current: Vec<&str> = Vec::new();
    let mut in_fence = false;

    for line in text.split('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
            in_fence = !in_fence;
        }

        if line.trim().is_empty() && !in_fence {
            if !current.is_empty() {
                blocks.push(current.join("\n"));
                current.clear();
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        blocks.push(current.join("\n"));
    }
    if blocks.is_empty() {
        blocks.push(String::new());
    }
    blocks
}

/// Reassemble blocks into a single markdown document.
pub fn join_blocks(blocks: &[String]) -> String {
    let mut out = blocks.join("\n\n");
    out.push('\n');
    out
}

/// Render a single markdown block to an HTML string.
pub fn render_markdown(md: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_TASKLISTS);
    options.insert(Options::ENABLE_FOOTNOTES);

    let parser = Parser::new_ext(md, options);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    out
}

#[component]
pub fn Editor() -> impl IntoView {
    let state = expect_context::<AppState>();

    // The single active textarea (only one block is editable at a time).
    let textarea: NodeRef<leptos::html::Textarea> = NodeRef::new();

    // Focus the textarea and drop the caret at the end whenever it mounts
    // (i.e. whenever the active block changes).
    Effect::new(move |_| {
        if let Some(el) = textarea.get() {
            let _ = el.focus();
            let len = el.value().len() as u32;
            let _ = el.set_selection_range(len, len);
        }
    });

    view! {
        <div class="editor">
            {move || {
                let blocks = state.blocks.get();
                let active = state.active.get();
                blocks
                    .into_iter()
                    .enumerate()
                    .map(|(i, block)| block_view(state, textarea, i, block, active == Some(i)))
                    .collect_view()
            }}
        </div>
    }
}

fn block_view(
    state: AppState,
    textarea: NodeRef<leptos::html::Textarea>,
    index: usize,
    block: String,
    is_active: bool,
) -> AnyView {
    if is_active {
        let rows = (block.matches('\n').count() + 1).max(1);
        view! {
            <textarea
                node_ref=textarea
                class="block-edit"
                rows=rows
                prop:value=block
                on:input=move |ev| {
                    let value = event_target_value(&ev);
                    state
                        .blocks
                        .update(|blocks| {
                            if let Some(slot) = blocks.get_mut(index) {
                                *slot = value;
                            }
                        });
                }
                on:blur=move |_| {
                    save_note(state);
                }
                on:keydown=move |ev: web_sys::KeyboardEvent| {
                    if ev.key() == "Escape" {
                        state.active.set(None);
                        save_note(state);
                    } else if ev.key() == "s" && (ev.ctrl_key() || ev.meta_key()) {
                        ev.prevent_default();
                        save_note(state);
                    }
                }
            ></textarea>
        }
        .into_any()
    } else {
        let html = if block.trim().is_empty() {
            "<p class=\"empty\"></p>".to_string()
        } else {
            render_markdown(&block)
        };
        view! {
            <div
                class="block-view"
                inner_html=html
                on:click=move |_| {
                    state.active.set(Some(index));
                }
            ></div>
        }
        .into_any()
    }
}
