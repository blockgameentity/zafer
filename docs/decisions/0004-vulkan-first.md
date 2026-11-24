# 0004 — Vulkan first, SPIR-V for shaders

Date: 2026-11-23. Owner's decision.

## Status

Accepted. Supersedes nothing (decision 0003, which introduced the DXIL
translator and the shader cache, stays in force; it gains a second backend).

## Context

Our host graphics today is D3D12 with DXIL shaders, and on Linux the host
D3D12 is vkd3d-proton loaded at run time
(`crates/victory-shader/src/hostlib.rs`). A console draw therefore walks
D3D12.X -> our D3D12 -> vkd3d-proton -> Vulkan.

The console's memory model is Vulkan's: a GPU address is a CPU address,
memory is unified, and a read past the end of a resource gives 0. Our
descriptor handles are plain addresses (`crates/victory-gpu/src/descriptor.rs`).
vkd3d-proton adds a second set of address-space, robustness and descriptor
rules to keep in step, and its robust raw access chain has already faulted a
host GPU once, stalling the world load (`crates/victory-shader/src/emit/lower.rs`,
`RAW_OFFSET_MAX`).

## Decision

The default host graphics target is Vulkan with SPIR-V shaders.

- New backend: `crates/victory-gpu/src/vulkan/`; new shader emitter:
  `crates/victory-shader/src/spirv/`. Neither exists yet.
- D3D12 with DXIL stays for Windows and as the comparison backend. The
  console-facing modules keep their exports and call one internal backend
  trait; `host.rs` becomes one implementation of it.
- The backend talks to the Vulkan loader through its C API, loaded at run
  time like every other host library, rather than through a crate that
  generates bindings at build time ([docs/linux.md](../linux.md), "No Wine,
  no Windows DLL, no system DLL forwarding").
- Untouched: `object.rs`, `console.rs`, the loader, the HLE, `victory-formats`.

The mapping, the shader path, the robustness consequences and the risks are in
[docs/vulkan-backend.md](../vulkan-backend.md). The work order there is
binding in order, not in dates.

## Consequences

- The vkd3d-proton dependency is removed from the default Linux path.
- Two backends exist until the D3D12 one is retired from Linux; every
  console-facing module must satisfy both, and the shader cache key grows the
  backend identity so SPIR-V and DXIL entries never mix.
- `VK_EXT_robustness2`, `VK_EXT_descriptor_indexing` and dynamic rendering
  become hard requirements on the adapter rather than conveniences; an
  adapter without them is refused clearly.
- The emitter's hand-written robustness clamps become conditional on the
  backend's robustness support instead of being deleted, since a driver
  without `VK_EXT_robustness2` still needs them.
- Losing DCC control costs bandwidth; that is accepted as a measurement, not
  worked around.

## Numbering

Decisions 0003 and 0007 are cited throughout the code (`shader_cache.rs`,
`emit/mod.rs`, `io.rs`, `pipeline.rs`) but are not in this repository: there
was no `docs/decisions/` before this entry (`git log -- docs/decisions` is
empty). This file follows the citation style those comments use. Reconstructing
0001-0003 and 0005-0007 from the code is separate work.
