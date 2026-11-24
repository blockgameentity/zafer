{
  description = "VIctory - a native Xbox Title runtime for Windows and Linux";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      inherit (nixpkgs) lib;

      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];

      pkgsFor =
        system:
        import nixpkgs {
          inherit system;
          overlays = [ (import rust-overlay) ];
        };

      forEachSystem = f: lib.genAttrs systems (system: f (pkgsFor system) system);

      # The repository's pin (rust-toolchain.toml): channel 1.98.1,
      # target x86_64-pc-windows-msvc, components rustfmt + clippy.
      toolchain = pkgs: pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;

      has = pkgs: name: builtins.hasAttr name pkgs;

      # SDL3 lives in exactly one of these two attribute names depending on nixpkgs.
      sdl3 = pkgs: if has pkgs "SDL3" then pkgs.SDL3 else pkgs.sdl3;

      # Libraries the runtime opens at run time (dlopen or the loader's search path).
      runtimeLibs =
        pkgs:
        [
          pkgs.vulkan-loader
          pkgs.libxkbcommon
          pkgs.alsa-lib
          pkgs.libpulseaudio
          pkgs.openssl
          pkgs.zlib
          pkgs.fontconfig
          pkgs.freetype
          pkgs.wayland
          pkgs.libGL
          pkgs.libxcb
        ]
        ++ lib.optional (has pkgs "SDL3" || has pkgs "sdl3") (sdl3 pkgs);

      # Build tooling: crates that link C, and vkd3d-proton (meson/ninja/glslang).
      buildTools = pkgs: [
        pkgs.pkg-config
        pkgs.cmake
        pkgs.ninja
        pkgs.meson
        pkgs.clang
        pkgs.lld
        pkgs.python3
        pkgs.wayland-protocols
        pkgs.vulkan-headers
      ];

      # cargo-xwin drives the Windows cross check; it is not in every nixpkgs.
      cargoXwin = pkgs: lib.optional (has pkgs "cargo-xwin") pkgs.cargo-xwin;

      shellHookFor = pkgs: ''
        export LD_LIBRARY_PATH="${lib.makeLibraryPath (runtimeLibs pkgs)}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
        ${lib.optionalString (has pkgs "SDL3" || has pkgs "sdl3") ''
          export SDL3="${sdl3 pkgs}/lib"
          export VICTORY_SDL3="${sdl3 pkgs}/lib/libSDL3.so.0"
        ''}
        export VKD3D_SHADER_CACHE_PATH="''${XDG_CACHE_HOME:-$HOME/.cache}/vkd3d-proton"

        echo "VIctory dev shell - Rust $(rustc --version 2>/dev/null | cut -d' ' -f2), Linux runtime deps in LD_LIBRARY_PATH"
        echo "  SDL3:            $SDL3 (also VICTORY_SDL3)"
        echo "  VK_ICD_FILENAMES / VK_LAYER_PATH: left for you to set"
        echo "  vkd3d-proton:    meson setup build --buildtype=release --prefix=\"$HOME/.local/vkd3d-proton\" && ninja -C build install"
        echo "  then:            export VICTORY_VKD3D=\"$HOME/.local/vkd3d-proton/lib\"  # libvkd3d-proton-d3d12core.so"
        echo "  optional:        export VICTORY_DXIL=~/.local/dxc/lib/libdxil.so VICTORY_DXCOMPILER=~/.local/dxc/lib/libdxcompiler.so"
        echo "  tests with a device: VICTORY_VKD3D=... cargo test -p victory-shader --features host -p victory-gpu"
        ${
          if has pkgs "cargo-xwin" then
            ''
              echo "  Windows cross check: nix develop .#windows-cross   (cargo xwin check --workspace --all-targets)"
            ''
          else
            ''
              echo "  Windows cross check: cargo-xwin is not packaged in this nixpkgs; nix develop .#windows-cross"
              echo "    installs the msvc target and hints at cargo install cargo-xwin, or use 'cargo install cargo-xwin' there"
            ''
        }
      '';

      devShellFor =
        pkgs:
        pkgs.mkShell {
          packages = [ (toolchain pkgs) ] ++ buildTools pkgs ++ runtimeLibs pkgs;
          buildInputs = runtimeLibs pkgs;
          shellHook = shellHookFor pkgs;
        };

      windowsCrossShellFor =
        pkgs:
        pkgs.mkShell {
          packages = [ (toolchain pkgs) ] ++ buildTools pkgs ++ cargoXwin pkgs;
          CARGO_BUILD_TARGET = "x86_64-pc-windows-msvc";
          shellHook = ''
            echo "VIctory Windows cross-check shell: target x86_64-pc-windows-msvc installed by rust-overlay"
            ${
              if has pkgs "cargo-xwin" then
                ''
                  echo "  cargo xwin check --workspace --all-targets"
                ''
              else
                ''
                  echo "  cargo-xwin is not packaged here: cargo install cargo-xwin, then"
                  echo "  cargo xwin check --workspace --all-targets"
                ''
            }
          '';
        };

      devEnvFor =
        pkgs:
        pkgs.buildEnv {
          name = "victory-dev-env";
          paths = [ (toolchain pkgs) ] ++ buildTools pkgs ++ runtimeLibs pkgs;
          pathsToLink = [
            "/bin"
            "/lib"
            "/include"
            "/lib/pkgconfig"
            "/share"
          ];
          ignoreCollisions = true;
        };
    in
    {
      packages = forEachSystem (
        pkgs: system: {
          default = (devEnvFor pkgs).overrideAttrs (old: {
            passthru = (old.passthru or { }) // {
              rustToolchain = toolchain pkgs;
              devShell = self.devShells.${system}.default;
              windowsCrossDevShell = self.devShells.${system}.windows-cross;
            };
          });

          rust-toolchain = toolchain pkgs;
        }
      );

      devShells = forEachSystem (
        pkgs: _: {
          default = devShellFor pkgs;
          windows-cross = windowsCrossShellFor pkgs;
        }
      );

      formatter = forEachSystem (pkgs: _: pkgs.nixfmt);

      # Light: these only force the flake and both shells to evaluate. No
      # crates are fetched and nothing is compiled, so `nix flake check`
      # runs offline once the inputs are locked.
      checks = forEachSystem (
        pkgs: system: {
          flake-evaluates = pkgs.runCommand "victory-flake-evaluates" { } ''
            echo "devShell default:      ${self.devShells.${system}.default.drvPath}" > $out
            echo "devShell windows-cross: ${self.devShells.${system}.windows-cross.drvPath}" >> $out
            echo "package default:       ${self.packages.${system}.default.drvPath}" >> $out
          '';
        }
      );
    };
}
