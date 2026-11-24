# Shaders

The title ships console shaders; the host needs its own. `victory-shader`
does the conversion, at first use, from the bytes the title passes at
pipeline creation, wherever it loaded them from.

## The pipeline

| Step | What happens | Where |
|------|--------------|-------|
| Container | a standard DXBC container; its `XDAT` table indexes blobs after the bitcode, among them `xbop` (compiler options) and the zlib-compressed GFX10 code | `victory-shader/src/dxbc.rs`, `dxil.rs` |
| ISA | the GFX10 code is decoded to native instructions, one face per opcode; the bitcode is not used, as it may carry console-only intrinsics | `victory-shader/src/gfx10/`, `native.rs` |
| IR | `ir::build` turns a native program into SSA, one instruction per machine instruction | `victory-shader/src/ir.rs` |
| Structure | EXEC removal: lane masks become per-lane booleans, wave-wide mask tests per-lane branches, vector instructions keep a guard only where a lane can reach them with EXEC clear | `victory-shader/src/structure.rs` |
| Resources | each descriptor and scalar memory address is traced to the root parameter it comes from, including waterfall loops that read a descriptor from the first lane | `victory-shader/src/resource.rs` |
| I/O | vertex inputs and outputs, interpolation, pixel inputs and exports are raised to the host stage's signature | `victory-shader/src/io.rs` |
| Backend | the IR is emitted as SPIR-V (default target, planned: `victory-shader/src/spirv/`) or DXIL (`victory-shader/src/emit/`) | `victory-shader/src/emit/` |

Every step reports what it cannot handle rather than guessing:
`victory translate` runs each shader through all of them and lists the
failures with one example each.

## The pre-compiler

The runtime translates a shader the first time the title creates a
pipeline for it. `victory precompile` (planned, in
`apps/victory/src/precompile.rs`) does the same work ahead of time over
files, so those first uses find the result already on disk.

It fills the shader cache (`crates/victory-gpu/src/shader_cache.rs`):
entries live under the cache folder in `shaders/`, keyed by the container's
content and by the BLAKE3 of our own executable, so a rebuilt translator
never serves an old translation. Entries go through `victory-pipeline`'s
content-addressed cache, which writes them atomically; an entry that does
not read back is translated again, and failures are not stored. The backend
is part of the key, so SPIR-V and DXIL entries do not mix.

| Option | Effect |
|--------|--------|
| `[paths...]` | files or directories to scan; defaults to the configured game and test-title directories |
| `--report <file.json>` | write the tally as JSON: containers, programs, translated, per-stage emit and validate counts, and the failure reasons |
| `--fail-fast` | stop at the first program that fails to translate |
| `--exit-code` | exit non-zero when any program failed, so a build or CI step fails with it |

Without `--report` the tally goes to stdout, one line per counter.

### Over a shader dump

`VICTORY_SHADER_SAVE=<dir>` (see the README's debugging table) makes
`victory run` save each shader container the title passes, as
`<hash>.<stage>`. That directory is the best input a pre-compile can have:
it holds the containers the title actually used, named and deduplicated.

```powershell
# 1. collect, from a diagnostics build
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
set VICTORY_SHADER_SAVE=shaders
target\diag\victory.exe run <game dir>\<the title's exe> --execute
```

```sh
# 2. see what was collected, and what the translator makes of it
victory shader-info shaders --verbose
victory translate shaders --list --jobs 16
```

```sh
# 3. pre-compile it into the cache the title's first uses will read
victory precompile shaders --report precompile.json --exit-code
```

```powershell
# 4. then start the title; cache hits and misses are logged at debug
set VICTORY_LOG=victory_gpu::shader_cache=debug
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

The same cache is reused by later runs, so steps 3 and 4 only need
repeating after a rebuild of the translator: the build fingerprint in the
key changes and old entries are ignored.
