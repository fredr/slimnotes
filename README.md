# slimnotes

A minimal, [Obsidian](https://obsidian.md)-vault-compatible notes app. Point it
at a folder of markdown files and read/edit them with an Obsidian-style **Live
Preview**: each block renders as markdown, and the block your cursor is in shows
its raw source.

- **Desktop shell:** [Tauri v2](https://tauri.app) (Rust backend, no JavaScript)
- **UI:** [Leptos](https://leptos.dev) (CSR, Rust → WASM) rendered in the webview
- **Vault format:** plain `.md` files in nested folders — the exact same layout
  Obsidian uses, so slimnotes and Obsidian can share a vault.

## Workspace layout

```
slimnotes/
├── flake.nix              # Nix dev shell with all build inputs
├── Cargo.toml             # workspace
└── crates/
    ├── core/              # shared serde types (Entry, Settings)
    ├── tauri/             # backend: vault scan + file IO, exposed as commands
    │   ├── tauri.conf.json
    │   ├── capabilities/  # Tauri v2 permission ACL
    │   └── src/main.rs
    └── ui/                # Leptos frontend
        ├── index.html     # Trunk entry
        ├── Trunk.toml
        ├── styles.css
        └── src/
            ├── app.rs     # layout, state, sidebar/tree
            ├── editor.rs  # live-preview block editor
            └── api.rs     # typed wrappers over Tauri `invoke`
```

## Prerequisites (NixOS)

Everything is provided by the flake dev shell — `webkitgtk_4_1`, `gtk3`, the Rust
toolchain (with the `wasm32-unknown-unknown` target), `trunk`, `wasm-bindgen-cli`
and `cargo-tauri`. Enter it with:

```sh
nix develop
```

## Run

From inside the dev shell:

```sh
cd crates/tauri
cargo tauri dev
```

This starts `trunk serve` (frontend at `http://localhost:1420`) and launches the
desktop window. On first run click **“Open vault…”** and pick a folder; the
choice is remembered in `~/.config/dev.encore.slimnotes/settings.json`.

To build release bundles:

```sh
cd crates/tauri
cargo tauri build
```

### Building the halves separately

```sh
cd crates/ui && trunk build        # frontend only (WASM)
cargo build -p slimnotes-tauri     # backend only
```

> Note: don't run `cargo build` at the workspace root — the `ui` crate targets
> `wasm32` and is built by Trunk, not by a host-target cargo build.

## The live-preview editor

A note is split into **blocks** on blank lines (fenced code blocks are kept
intact). Every block is rendered to HTML with
[`pulldown-cmark`](https://crates.io/crates/pulldown-cmark), except the one block
you click into, which becomes a `<textarea>` showing the raw markdown. Click
away (or press `Esc`) and it re-renders. Saving is automatic on blur, plus
`Ctrl`/`Cmd`+`S`.

### Current limitations / next steps

- Live preview is **block-level**, not per-line as in Obsidian's newest editor.
  Clicking a multi-line paragraph shows the whole paragraph's source. Moving to
  a true per-line CodeMirror-style decorator model is the main follow-up.
- Reassembling blocks normalizes runs of blank lines to a single blank line.
- No rename/delete/move from the UI yet (do it in the filesystem); no search,
  wikilinks, tags, or embeds — this is intentionally a small subset of Obsidian.

## Notes for non-NixOS

The flake handles NixOS. On other systems install the
[Tauri v2 Linux prerequisites](https://tauri.app/start/prerequisites/)
(`webkit2gtk-4.1`, etc.), plus `trunk` and the `wasm32-unknown-unknown` target,
then use the same `cargo tauri dev` / `trunk build` commands.
