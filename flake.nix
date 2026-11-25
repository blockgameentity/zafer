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

      # Shader and D3D-on-Vulkan tooling: `spirv-val`/`spirv-dis`/`spirv-as`
      # (victory-shader's emitted modules), `glslang`/`shaderc`, `vulkaninfo`
      # and the D3D12 translation libraries. Each name is checked first: not
      # every nixpkgs carries all of them, and a shell must not break over a
      # hint tool.
      shaderTools =
        pkgs:
        lib.optionals (has pkgs "spirv-tools") [ pkgs.spirv-tools ]
        ++ lib.optionals (has pkgs "shaderc") [ pkgs.shaderc ]
        ++ lib.optionals (has pkgs "glslang") [ pkgs.glslang ]
        ++ lib.optionals (has pkgs "vulkan-tools") [ pkgs.vulkan-tools ]
        ++ lib.optionals (has pkgs "vkd3d") [ pkgs.vkd3d ]
        ++ lib.optionals (has pkgs "vkd3d-proton") [ pkgs.vkd3d-proton ]
        ++ lib.optionals (has pkgs "dxvk") [ pkgs.dxvk ];

      # Build tooling: crates that link C, and vkd3d-proton (meson/ninja/glslang).
      # `mold` is the linker the dev shell selects (`-fuse-ld=mold`) and
      # `sccache` is the compiler cache behind `RUSTC_WRAPPER`; both are for
      # interactive builds only (a Nix sandbox has neither a cache daemon nor
      # the system linker they need), so they are named here but not used by
      # the package or test derivations below.
      buildTools =
        pkgs:
        [
          pkgs.pkg-config
          pkgs.mold
          pkgs.sccache
          pkgs.cmake
          pkgs.ninja
          pkgs.meson
          pkgs.clang
          pkgs.lld
          pkgs.python3
          pkgs.wayland-protocols
          pkgs.vulkan-headers
        ]
        ++ shaderTools pkgs;

      # cargo-xwin drives the Windows cross check; it is not in every nixpkgs.
      cargoXwin = pkgs: lib.optional (has pkgs "cargo-xwin") pkgs.cargo-xwin;

      # `cargo`/`rustc` are the repository's pin, not nixpkgs' rustc.
      rustPlatformFor =
        pkgs:
        let
          rust = toolchain pkgs;
        in
        pkgs.makeRustPlatform {
          inherit (rust) version;
          cargo = rust;
          rustc = rust;
        };

      # What `packages.victory` and `checks.tests` share: the checkout, the
      # locked crates, a C toolchain (the guest-side linker) and the libraries
      # the binary is built against. `CARGO_BUILD_TARGET` is unset on purpose:
      # this is a native Linux build, not the msvc cross build.
      workspaceAttrs =
        pkgs:
        {
          src = self;
          cargoLock.lockFile = ./Cargo.lock;
          # One binary out of a workspace of members, no per-subdirectory build.
          buildAndTestSubdir = null;
          strictDeps = true;
          nativeBuildInputs = [
            pkgs.pkg-config
            pkgs.clang
            pkgs.lld
          ];
          buildInputs = runtimeLibs pkgs;
          # `crates/victory-windows/build.rs` generates its D3D12/DXGI bindings
          # from `windows-bindgen`'s own metadata, at build time, without
          # network: the crate is vendored like every other dependency.
          CARGO_BUILD_TARGET = null;
        };

      victoryFor =
        pkgs:
        (rustPlatformFor pkgs).buildRustPackage (
          (workspaceAttrs pkgs)
          // {
            pname = "victory";
            version = "0.1.0";
            cargoBuildFlags = [
              "-p"
              "victory"
            ];
            cargoCheckFlags = [
              "-p"
              "victory"
            ];
            doCheck = false;
            meta = {
              description = "A native Xbox Title runtime (Xbox system emulation) for Linux";
              mainProgram = "victory";
              license = lib.licenses.gpl3Plus;
              platforms = systems;
            };
          }
        );

      # The workspace's tests in the sandbox. They need no GPU: the device
      # tests (`victory-shader`'s `emit::valu_tests`, `victory-gpu`'s host
      # ones) skip themselves when `VICTORY_VKD3D` is unset, and none of them
      # reads the title's data.
      workspaceTestsFor =
        pkgs:
        (rustPlatformFor pkgs).buildRustPackage (
          (workspaceAttrs pkgs)
          // {
            pname = "victory-workspace-tests";
            version = "0.1.0";
            # The debug profile, one test thread at a time: the release test
            # binaries and the parallel harness both fault in `victory-hle`'s
            # UCRT stdio tests, which install guest stdio state per thread.
            cargoBuildType = null;
            doCheck = true;
            checkPhase = ''
              runHook preCheck
              cargo test --workspace -- --test-threads=1 --skip repository_passes_policy
              runHook postCheck
            '';
            # `repository_passes_policy` (xtask) shells out to `git ls-files`,
            # and a store path has no repository; CI runs it on a checkout.
          }
        );

      # What the shell says about the host D3D12 library. nixpkgs'
      # `vkd3d-proton` is the Windows (PE) build for DXVK, not the native Linux
      # `libvkd3d-proton-d3d12core.so` that `victory-shader`'s `hostlib`
      # dlopens, so the shell points at `VICTORY_VKD3D` instead of pretending
      # the package is that library.
      nixpkgsHint =
        pkgs:
        if has pkgs "vkd3d-proton" then
          "nixpkgs vkd3d-proton ${pkgs.vkd3d-proton.version} is the Windows D3D12 build; build the native one above"
        else
          "vkd3d-proton is not packaged in this nixpkgs; build the native one above";

      shellHookFor = pkgs: ''
        export LD_LIBRARY_PATH="${lib.makeLibraryPath (runtimeLibs pkgs)}''${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
        ${lib.optionalString (has pkgs "SDL3" || has pkgs "sdl3") ''
          export SDL3="${sdl3 pkgs}/lib"
          export VICTORY_SDL3="${sdl3 pkgs}/lib/libSDL3.so.0"
        ''}
        export VKD3D_SHADER_CACHE_PATH="''${XDG_CACHE_HOME:-$HOME/.cache}/vkd3d-proton"

        # sccache cannot write into a store path, so its cache lives in the
        # user cache directory; mold is the linker every cargo invocation in
        # this shell uses. Neither reaches the sandboxed derivations.
        export SCCACHE_DIR="''${XDG_CACHE_HOME:-$HOME/.cache}/sccache"
        mkdir -p "$SCCACHE_DIR"
        export RUSTC_WRAPPER=sccache
        export RUSTFLAGS="-C link-arg=-fuse-ld=mold''${RUSTFLAGS:+ $RUSTFLAGS}"

        echo "VIctory dev shell - Rust $(rustc --version 2>/dev/null | cut -d' ' -f2), Linux runtime deps in LD_LIBRARY_PATH"
        echo "  SDL3:            $SDL3 (also VICTORY_SDL3)"
        echo "  VK_ICD_FILENAMES / VK_LAYER_PATH: left for you to set"
        echo "  vkd3d-proton:    meson setup build --buildtype=release --prefix=\"$HOME/.local/vkd3d-proton\" && ninja -C build install"
        echo "  then:            export VICTORY_VKD3D=\"$HOME/.local/vkd3d-proton/lib\"  # libvkd3d-proton-d3d12core.so"
        echo "  (${nixpkgsHint pkgs})"
        echo "  build:           sccache (RUSTC_WRAPPER, cache in $SCCACHE_DIR), mold (-fuse-ld=mold)"
        echo "  shader tools:    spirv-val spirv-dis spirv-as glslangValidator vulkaninfo"
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
          victory = victoryFor pkgs;
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
            echo "package victory:       ${self.packages.${system}.victory.drvPath}" >> $out
          '';

        }
        // lib.optionalAttrs pkgs.stdenv.hostPlatform.isLinux {
          # The workspace's tests, compiled and run in the sandbox. No GPU: the
          # device tests return early without `VICTORY_VKD3D`, and nothing here
          # opens the title's data. The msvc cross build is
          # `nix develop .#windows-cross`'s job.
          tests = workspaceTestsFor pkgs;
        }
      );
    };
}
