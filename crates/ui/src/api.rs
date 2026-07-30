//! Thin typed wrappers over the Tauri IPC bridge.
//!
//! The Tauri JS API is exposed on `window.__TAURI__` because
//! `withGlobalTauri` is enabled in `tauri.conf.json`. We call `invoke`
//! directly through `wasm-bindgen` so the frontend stays pure Rust.

use serde::de::DeserializeOwned;
use serde::Serialize;
use slimnotes_core::{Entry, Settings};
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__TAURI__", "core"], js_name = invoke, catch)]
    async fn invoke_js(cmd: &str, args: JsValue) -> Result<JsValue, JsValue>;
}

fn js_err(value: JsValue) -> String {
    value
        .as_string()
        .unwrap_or_else(|| format!("{value:?}"))
}

async fn invoke_raw(cmd: &str, args: &impl Serialize) -> Result<JsValue, String> {
    let args = serde_wasm_bindgen::to_value(args).map_err(|e| e.to_string())?;
    invoke_js(cmd, args).await.map_err(js_err)
}

async fn invoke<T: DeserializeOwned>(cmd: &str, args: &impl Serialize) -> Result<T, String> {
    let value = invoke_raw(cmd, args).await?;
    serde_wasm_bindgen::from_value(value).map_err(|e| e.to_string())
}

/// An empty argument payload (`{}`) for commands that take no parameters.
#[derive(Serialize)]
struct NoArgs {}

pub async fn load_settings() -> Result<Settings, String> {
    invoke("load_settings", &NoArgs {}).await
}

pub async fn pick_vault() -> Result<Option<String>, String> {
    invoke("pick_vault", &NoArgs {}).await
}

pub async fn read_tree(vault: String) -> Result<Vec<Entry>, String> {
    #[derive(Serialize)]
    struct Args {
        vault: String,
    }
    invoke("read_tree", &Args { vault }).await
}

pub async fn read_note(path: String) -> Result<String, String> {
    #[derive(Serialize)]
    struct Args {
        path: String,
    }
    invoke("read_note", &Args { path }).await
}

pub async fn write_note(path: String, content: String) -> Result<(), String> {
    #[derive(Serialize)]
    struct Args {
        path: String,
        content: String,
    }
    // The command returns `()`; ignore the resolved value rather than trying
    // to deserialize unit from `null`.
    invoke_raw("write_note", &Args { path, content }).await.map(|_| ())
}

pub async fn create_note(vault: String, name: String) -> Result<String, String> {
    #[derive(Serialize)]
    struct Args {
        vault: String,
        name: String,
    }
    invoke("create_note", &Args { vault, name }).await
}
