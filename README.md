# slimnotes

A minimal, [Obsidian](https://obsidian.md)-vault-compatible notes app. Point it
at a folder of markdown files and read/edit them with an Obsidian-style **Live
Preview**: each block renders as markdown, and the block your cursor is in shows
its raw source.

- **UI:** [GPUI](https://www.gpui.rs/) — Zed's GPU-accelerated Rust UI framework.
  **No webview, no HTML/JS.** Rendered natively via Blade → Vulkan on Linux.
- **Widgets:** [gpui-component](https://github.com/longbridge/gpui-component) —
  its `TextView::markdown` renderer and Rope-backed `InputState` code editor are
  the two building blocks of the live-preview editor.
- **Vault format:** plain `.md` files in nested folders — the same layout
  Obsidian uses, so slimnotes and Obsidian can share a vault.

## Workspace layout

```
slimnotes/
├── flake.nix              # Nix dev shell (Rust + Vulkan/Wayland/X11 libs)
├── Cargo.toml             # workspace
└── crates/
    ├── core/              # shared serde types (Entry, Settings)
    └── app/               # the GPUI binary
        └── src/
            ├── main.rs    # root view: sidebar tree, blocks, click-to-edit
            └── vault.rs    # std::fs vault access + block split/join
```

## Versions

Pinned together because they must match:

| Crate | Version | Source |
|---|---|---|
| `gpui` | `0.2.2` | crates.io (Apache-2.0) |
| `gpui-component` | `0.5.1` | crates.io |
| `gpui-component-assets` | `0.5.1` | crates.io |

## Build & run (NixOS)

The flake dev shell provides the Rust toolchain and the GPUI runtime libraries
(`vulkan-loader`, `wayland`, `libxkbcommon`, xorg libs, `fontconfig`,
`freetype`, `libGL`). It also adds `/run/opengl-driver/lib` to
`LD_LIBRARY_PATH` so Blade finds your GPU's Vulkan driver.

```sh
nix develop
cargo run -p slimnotes
```

On first run the vault is read from `~/.config/slimnotes/settings.json`
(`{ "vault": "/abs/path/to/vault" }`). A native folder picker is a planned
follow-up (see below).

## How the live-preview editor works

A note is split into **blocks** on blank lines (fenced code blocks kept intact).
Every block is rendered with `TextView::markdown`, except the block you click
into, which becomes a Rope-backed, syntax-highlighted `InputState` editor showing
the raw markdown. Edits are written back to the block on `Change`; the file is
saved on `Blur` and when switching notes.

### Implemented

- Read a vault folder, show the folder/file tree (hidden dirs like `.obsidian`
  skipped), open notes.
- Live-preview editing: rendered blocks + click-a-block-to-edit.
- Auto-save on blur / note switch.

### Planned next steps

- **Open-vault folder picker** and **New note** button (backend fns
  `save_settings` / `create_note` already exist in `vault.rs`, just unwired).
- `Cmd/Ctrl+S`, keyboard nav between blocks.
- Per-line (rather than per-block) live preview, closer to Obsidian's newest
  editor.
- Dark/light theme wiring via gpui-component's theme, rename/delete, search.

## Notes

- GPUI is **pre-1.0**; expect API churn if you bump `gpui`/`gpui-component`.
- This is intentionally a small subset of Obsidian: no wikilinks, tags, embeds,
  or graph view.
