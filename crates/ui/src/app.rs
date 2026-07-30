//! Top-level app: shared state, layout, sidebar (vault tree) and the editor.

use leptos::prelude::*;
use slimnotes_core::Entry;
use wasm_bindgen_futures::spawn_local;

use crate::api;
use crate::editor::{self, Editor};

/// Shared, reactive application state. Every field is a Leptos signal handle
/// (cheap `Copy`), so `AppState` can be freely copied into event handlers and
/// pulled from context by any component.
#[derive(Clone, Copy)]
pub struct AppState {
    pub vault: RwSignal<Option<String>>,
    pub tree: RwSignal<Vec<Entry>>,
    pub current: RwSignal<Option<String>>,
    pub blocks: RwSignal<Vec<String>>,
    pub active: RwSignal<Option<usize>>,
    pub status: RwSignal<String>,
}

impl AppState {
    fn new() -> Self {
        Self {
            vault: RwSignal::new(None),
            tree: RwSignal::new(Vec::new()),
            current: RwSignal::new(None),
            blocks: RwSignal::new(Vec::new()),
            active: RwSignal::new(None),
            status: RwSignal::new(String::new()),
        }
    }
}

pub fn refresh_tree(state: AppState) {
    let Some(vault) = state.vault.get_untracked() else {
        return;
    };
    spawn_local(async move {
        match api::read_tree(vault).await {
            Ok(tree) => state.tree.set(tree),
            Err(e) => state.status.set(format!("tree error: {e}")),
        }
    });
}

pub fn open_note(state: AppState, path: String) {
    spawn_local(async move {
        match api::read_note(path.clone()).await {
            Ok(content) => {
                state.blocks.set(editor::split_blocks(&content));
                state.active.set(None);
                state.current.set(Some(path));
                state.status.set(String::new());
            }
            Err(e) => state.status.set(format!("open error: {e}")),
        }
    });
}

pub fn save_note(state: AppState) {
    let Some(path) = state.current.get_untracked() else {
        return;
    };
    let content = editor::join_blocks(&state.blocks.get_untracked());
    spawn_local(async move {
        match api::write_note(path, content).await {
            Ok(()) => state.status.set("saved".into()),
            Err(e) => state.status.set(format!("save error: {e}")),
        }
    });
}

fn create_note_action(state: AppState, new_name: RwSignal<String>) {
    let Some(vault) = state.vault.get_untracked() else {
        return;
    };
    let name = new_name.get_untracked();
    spawn_local(async move {
        match api::create_note(vault, name).await {
            Ok(path) => {
                new_name.set(String::new());
                refresh_tree(state);
                open_note(state, path);
            }
            Err(e) => state.status.set(format!("create error: {e}")),
        }
    });
}

#[component]
pub fn App() -> impl IntoView {
    let state = AppState::new();
    provide_context(state);

    // Load persisted settings on startup; if a vault is remembered, list it.
    spawn_local(async move {
        if let Ok(settings) = api::load_settings().await {
            if let Some(vault) = settings.vault {
                state.vault.set(Some(vault));
                refresh_tree(state);
            }
        }
    });

    view! {
        <div class="app">
            <Sidebar />
            <main class="content">
                {move || match state.current.get() {
                    Some(path) => view! {
                        <div class="titlebar">
                            <span class="title">{file_stem(&path)}</span>
                            <span class="status">{move || state.status.get()}</span>
                        </div>
                        <Editor />
                    }
                    .into_any(),
                    None => view! {
                        <div class="placeholder">
                            <p>"Select a note, or open a vault to get started."</p>
                        </div>
                    }
                    .into_any(),
                }}
            </main>
        </div>
    }
}

#[component]
fn Sidebar() -> impl IntoView {
    let state = expect_context::<AppState>();
    let new_name = RwSignal::new(String::new());

    let open_vault = move |_| {
        spawn_local(async move {
            match api::pick_vault().await {
                Ok(Some(vault)) => {
                    state.vault.set(Some(vault));
                    refresh_tree(state);
                }
                Ok(None) => {}
                Err(e) => state.status.set(format!("pick error: {e}")),
            }
        });
    };

    view! {
        <aside class="sidebar">
            <div class="sidebar-header">
                <span class="brand">"slimnotes"</span>
                <button class="btn" on:click=open_vault>"Open vault…"</button>
            </div>
            {move || {
                if state.vault.get().is_some() {
                    view! {
                        <div class="new-note">
                            <input
                                class="new-note-input"
                                placeholder="New note name…"
                                prop:value=move || new_name.get()
                                on:input=move |ev| new_name.set(event_target_value(&ev))
                                on:keydown=move |ev: web_sys::KeyboardEvent| {
                                    if ev.key() == "Enter" {
                                        create_note_action(state, new_name);
                                    }
                                }
                            />
                            <button
                                class="btn"
                                on:click=move |_| create_note_action(state, new_name)
                            >
                                "+"
                            </button>
                        </div>
                    }
                    .into_any()
                } else {
                    ().into_any()
                }
            }}
            <nav class="tree">
                <For
                    each=move || state.tree.get()
                    key=|entry| entry.path.clone()
                    let:entry
                >
                    <TreeEntry entry=entry />
                </For>
            </nav>
        </aside>
    }
}

#[component]
fn TreeEntry(entry: Entry) -> impl IntoView {
    let state = expect_context::<AppState>();

    if entry.is_dir {
        let children = entry.children.clone();
        view! {
            <details class="tree-folder" open>
                <summary>{entry.name.clone()}</summary>
                <div class="tree-children">
                    <For
                        each=move || children.clone()
                        key=|child| child.path.clone()
                        let:child
                    >
                        <TreeEntry entry=child />
                    </For>
                </div>
            </details>
        }
        .into_any()
    } else {
        let path = entry.path.clone();
        let is_current = move || state.current.get().as_deref() == Some(path.as_str());
        let open_path = entry.path.clone();
        view! {
            <div
                class="tree-file"
                class:active=is_current
                on:click=move |_| open_note(state, open_path.clone())
            >
                {display_name(&entry.name)}
            </div>
        }
        .into_any()
    }
}

/// Strip the `.md` extension for display.
fn display_name(name: &str) -> String {
    name.strip_suffix(".md").unwrap_or(name).to_string()
}

/// Extract the note title (file stem) from an absolute path.
fn file_stem(path: &str) -> String {
    let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
    display_name(file)
}
