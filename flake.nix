{
  description = "slimnotes — a minimal, Obsidian-vault-compatible notes app (GPUI)";

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
        rust = pkgs.rust-bin.stable.latest.default;

        # GPUI / Blade (Vulkan) runtime + link libraries on Linux. These mirror
        # the buildInputs nixpkgs uses for the Zed editor, which is built on the
        # same GPUI stack.
        libraries = with pkgs; [
          fontconfig
          freetype
          wayland
          libxkbcommon
          xorg.libxcb
          xorg.libX11
          xorg.libXext
          libGL
          vulkan-loader
          openssl
          zlib
        ];
      in
      {
        devShells.default = pkgs.mkShell {
          nativeBuildInputs = with pkgs; [ rust pkg-config cmake ];
          buildInputs = libraries;

          # Some -sys crates in the GPUI tree run bindgen.
          LIBCLANG_PATH = "${pkgs.llvmPackages.libclang.lib}/lib";

          shellHook = ''
            # Blade loads Vulkan at runtime; the actual GPU ICD lives in the
            # NixOS driver path, and libvulkan/wayland come from the store.
            export LD_LIBRARY_PATH="${pkgs.lib.makeLibraryPath libraries}:/run/opengl-driver/lib:''${LD_LIBRARY_PATH:-}"
            echo "slimnotes (GPUI) dev shell — rustc $(rustc --version | cut -d' ' -f2)"
          '';
        };
      });
}
