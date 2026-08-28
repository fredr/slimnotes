# slimnotes

A minimal, [Obsidian](https://obsidian.md)-vault-compatible notes app. Point it
at a folder of markdown files and read/edit them with an Obsidian-style **Live
Preview**: the whole note renders as formatted markdown, and the element your
cursor (or selection) touches reveals its raw source.

- **UI:** [GPUI](https://www.gpui.rs/) — Zed's GPU-accelerated Rust UI framework.
  **No webview, no HTML/JS.** Rendered natively via Blade → Vulkan on Linux.
- **Editor:** a **from-scratch live-preview editor** built directly on GPUI as a
  custom `Element` (own `request_layout`/`prepaint`/`paint`). It is *not* built on
  a third-party text widget — the live-preview concealment needs an explicit
  source↔display mapping that no off-the-shelf component provides.
- **Vault format:** plain `.md` files in nested folders — the same layout
  Obsidian uses, so slimnotes and Obsidian can share a vault. The filesystem is
  the source of truth: external edits (git, sync clients, Obsidian) are watched
  and never silently overwritten.

## Workspace layout

```
slimnotes/
├── flake.nix              # Nix dev shell (Rust + Vulkan/Wayland/X11 libs)
├── Cargo.toml             # workspace
└── crates/
    ├── vault/             # the vault model + all filesystem access:
    │                      #   read_tree, atomic read/write, settings,
    │                      #   VaultIndex (wikilink/embed resolver), notify watcher
    ├── editor/            # the from-scratch live-preview markdown editor
    │   └── src/           #   modular: rope buffer, parser, source↔display
    │                      #   segment map, layout, the GPUI element, tables,
    │                      #   syntax highlighting
    └── app/               # the GPUI binary: sidebar file tree, open/save,
        └── src/main.rs    #   background fs, autosave, external-edit conflicts
```

## How the live-preview editor works

The whole note is one editor. Each source line is shaped with its own font size
(real heading sizes) and wrap width, inline markdown is styled per span, and the
syntax markers are concealed on every line **except** the element the cursor or
selection is touching — the Obsidian "Live Preview" behavior. A per-line
`Seg`/`Op` transform maps buffer offsets ↔ displayed offsets so the caret,
selection, and hit-testing round-trip correctly between the raw text and the
rendered view.

- **Text buffer:** a [`ropey`](https://crates.io/crates/ropey) rope with
  delta-based undo/redo (cheap edits and history, independent of note size).
- **Parsing:** one context-aware pass over [`pulldown-cmark`](https://crates.io/crates/pulldown-cmark)
  plus scanners for Obsidian extensions (`[[wikilinks]]`, `#tags`,
  `==highlights==`) that respect code-span/escape context. Memoized per edit.
- **Syntax highlighting:** [`syntect`](https://crates.io/crates/syntect) (pure-Rust
  fancy-regex engine) + [`two-face`](https://crates.io/crates/two-face)'s extended
  syntax set (~200 languages incl. TypeScript, TOML, Nix, …).

### Implemented

- Vault folder tree (hidden dirs like `.obsidian` skipped), collapse state
  persisted per vault; open/create notes; folder picker; debounced autosave;
  `Cmd/Ctrl+S`. Right-click a row to rename or delete (deletes go to the vault's
  `.trash/`), or drag a note or folder to move it.
- Note finder (`Cmd/Ctrl+P`): fuzzy match over every note path in the vault with
  the matched characters highlighted, recently opened notes when the query is
  empty, and an offer to create the note a query names. Opening a note from
  anywhere reveals it in the tree.
- Live-preview editing: headings, bold/italic/strikethrough, inline code,
  `==highlight==`, `#tag` pills, bullet/ordered lists, task checkboxes
  (click to toggle), blockquotes, horizontal rules, fenced code blocks with
  syntax highlighting, frontmatter.
- Reordering and nesting for bullet/task items, each carrying whatever is
  nested underneath: hover the gutter beside an item for a grip that drags it
  up or down its list, and press tab / shift-tab to make it a child of the item
  above or promote it back out. Enter on an empty nested item climbs out one
  level per press, ending the list at the margin.
- `[[wikilinks]]` / `[[target|alias]]` (click to open, resolved via the vault
  index) and clickable `[text](url)` links; inline images (`![](path)` and
  Obsidian `![[embed]]`).
- Tables: always rendered (never dumped as raw), edit text directly in cells,
  right-click a cell for add/delete row/column.
- Soft-wrap with hanging indent, wrap-aware vertical movement, selection,
  double-click word / triple-click line select.
- Safe persistence: atomic writes, background filesystem work, a `notify`
  watcher, and external-edit conflict detection (autosave won't clobber a file
  changed on disk).

### Planned / not yet done

- Viewport virtualization and incremental (per-block) reparse — the current
  pipeline reparses/shapes the whole note per edit, which is fine at typical note
  sizes but is the scaling ceiling.
- IME / marked-text input (dead keys, CJK), a configurable keymap, word-wise and
  page motions.
- Full-text search over note *contents* (the finder matches paths only), light
  theme.

## Versions

Pinned together because they must match:

| Crate | Version | Source |
|---|---|---|
| `gpui` | `0.2.2` | crates.io (Apache-2.0) |
| `gpui-component` | `0.5.1` | crates.io |
| `gpui-component-assets` | `0.5.1` | crates.io |

`gpui` is **pre-1.0**; expect API churn if you bump `gpui`/`gpui-component`.

## Build & run (NixOS)

The flake dev shell provides the Rust toolchain and the GPUI runtime libraries
(`vulkan-loader`, `wayland`, `libxkbcommon`, xorg libs, `fontconfig`,
`freetype`, `libGL`). It also adds `/run/opengl-driver/lib` to
`LD_LIBRARY_PATH` so Blade finds your GPU's Vulkan driver.

```sh
nix develop
cargo run -p slimnotes            # or: cargo run -p slimnotes --release
```

On first run, pick a vault with the folder button in the sidebar; the choice is
saved to `~/.config/slimnotes/settings.json`.
