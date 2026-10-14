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
    # The GPUI stack below is the Linux one (Blade/Vulkan, Wayland, X11).
    flake-utils.lib.eachSystem [ "x86_64-linux" "aarch64-linux" ] (system:
      let
        overlays = [ (import rust-overlay) ];
        pkgs = import nixpkgs { inherit system overlays; };
        rust = pkgs.rust-bin.stable.latest.default;
        version = (pkgs.lib.importTOML ./Cargo.toml).workspace.package.version;

        # GPUI / Blade (Vulkan) runtime + link libraries on Linux. These mirror
        # the buildInputs nixpkgs uses for the Zed editor, which is built on the
        # same GPUI stack.
        libraries = with pkgs; [
          fontconfig
          freetype
          wayland
          libxkbcommon
          libxcb
          libx11
          libxext
          libGL
          vulkan-loader
          openssl
          zlib
        ];
      in
      {
        # Built with nixpkgs' own Rust (not the overlay toolchain), so a
        # consumer that sets `inputs.nixpkgs.follows` gets cached compilers.
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = "slimnotes";
          inherit version;
          # Only what cargo needs, so the sample vault or a local target/ never
          # reach the store.
          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [ ./Cargo.toml ./Cargo.lock ./crates ];
          };
          cargoLock.lockFile = ./Cargo.lock;

          nativeBuildInputs = with pkgs; [ pkg-config cmake rustPlatform.bindgenHook ];
          buildInputs = libraries;

          # Vulkan and Wayland are dlopen'd at runtime rather than linked, so
          # they have to be put on the RPATH by hand (as nixpkgs does for Zed).
          postFixup = ''
            patchelf --add-rpath ${pkgs.lib.makeLibraryPath libraries} $out/bin/slimnotes
          '';

          meta = {
            description = "A minimal, Obsidian-vault-compatible notes app";
            license = pkgs.lib.licenses.mit;
            mainProgram = "slimnotes";
            platforms = pkgs.lib.platforms.linux;
          };
        };

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
