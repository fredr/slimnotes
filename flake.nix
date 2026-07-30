{
  description = "slimnotes — a minimal, Obsidian-vault-compatible notes app (Tauri + Leptos)";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-25.11";
    flake-utils.url = "github:numtide/flake-utils";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs = { self, nixpkgs, flake-utils, rust-overlay }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };

        rust = pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
        };

        # Runtime / link libraries required by Tauri's webview on Linux.
        libraries = with pkgs; [
          webkitgtk_4_1
          gtk3
          cairo
          gdk-pixbuf
          glib
          pango
          harfbuzz
          librsvg
          libsoup_3
          openssl
        ];

        tools = with pkgs; [
          rust
          trunk
          wasm-bindgen-cli
          cargo-tauri
          nodejs
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = tools ++ (with pkgs; [ pkg-config wrapGAppsHook3 ]);
          buildInputs = libraries;

          shellHook = ''
            # WebKitGTK + some GPUs need compositing disabled to render at all.
            export WEBKIT_DISABLE_COMPOSITING_MODE=1
            echo "slimnotes dev shell — trunk $(trunk --version 2>/dev/null | head -n1), $(cargo-tauri --version 2>/dev/null || cargo tauri --version 2>/dev/null)"
          '';
        };
      });
}
