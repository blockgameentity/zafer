# 5. `ash` for Vulkan, and safe crates for the system layer

- Status: accepted
- Date: 2026-02-13
- Supersedes part of decision 0004 (which hand-wrote the Vulkan FFI).
- Relates to issue: the DX12/DXIL port being finished before deletion.

## Context

`victory-vulkan` spelled out its own `#[repr(C)]` Vulkan structures and
looked every entry point up through `vkGetInstanceProcAddr` by hand. That is
about 230 lines of asserted layouts, and every extension the backend will
need (`VK_EXT_descriptor_buffer`, `VK_EXT_robustness2`,
`VK_EXT_mesh_shader`) would add more.

We considered [vulkano], which is the blessed.rs recommendation for Vulkan.
It is the wrong tool here, and for a specific reason: vulkano's value is
proving that resource use is safe, which requires that resources be owned
values created through its allocator. The console's model is the opposite —
a GPU address *is* a CPU address, one physical page is aliased at several
addresses, descriptor heaps are plain memory a title reads and writes, and
64 KiB blocks are `mprotect`ed to catch title writes. Almost every object we
create would be `unsafe`-constructed from a raw pointer and then have
vulkano's allocator in its way. We would pay a large dependency tree and a
version-locked API to bypass it.

## Decision

Use [`ash`] (0.38). It is generated bindings and nothing else, so
`vkCreateShaderModule` stays `vkCreateShaderModule`, and the deletion of our
hand-written `types` module is the whole benefit.

For the *system* layer, prefer safe crates wherever a safe equivalent of the
raw `libc` call exists, and say why in a comment where it does not:

| Today | Becomes | Why |
|---|---|---|
| `libc::{mmap, mprotect, munmap, mincore}` in the Win32 memory emulation | [`rustix`] | typed pointers and `io::Result`, deleting most `unsafe` in a 2 050-line file |
| `libc::{dlopen, dlsym, dlerror}` | [`libloading`] | already in the tree via `ash`; no transmutes |
| `libc::mmap` of files | [`memmap2`] | ownership and `Drop` instead of raw mappings |
| hand-rolled region set | [`region`] | the `MEM_RESERVE`/`MEM_COMMIT` bookkeeping, already correct |

`libc` stays for what has no safe wrapper — the `PROCMAP_QUERY` ioctl,
`memfd`, and the signal/`ucontext` work in `victory-loader` — and each such
use carries a comment naming why.

## What this does not change

The Windows-side code that *emulates the console's NT kernel* — the syscall
table, the exception dispatcher, the unwind tables, the TEB/TLS setup — is
not replaceable by a crate. It is x86-64 Windows ABI emulation on a Linux
host, and any crate claiming to do it would be as large and as specific as
the code it replaced. `#[unsafe(naked)]` context switches and
`sigaction` handlers must stay `unsafe`; the rule there is that each block
states its invariant and that as much as possible of the surrounding logic
(frames, stack bounds, register decoding) is safe code operating on bytes.

`victory-windows` (bindgen of the D3D12/DXGI metadata for Linux) is deleted
with the DX12 backend, not replaced: once the last title runs on Vulkan
there is nothing left that wants an `ID3D12Device`.

## Consequences

- `victory-vulkan` loses its `types` module entirely and gains `ash`.
- A machine with no Vulkan loader still works: `Entry::load` fails, the
  error is a `&'static str`, and every test skips.
- Every ported module is testable without a GPU, because the policy
  (format choice, query layout, memory classes) is split from the calls
  that need a driver. That is what makes the port verifiable at all.

[vulkano]: https://crates.io/crates/vulkano
[`ash`]: https://crates.io/crates/ash
[`rustix`]: https://crates.io/crates/rustix
[`libloading`]: https://crates.io/crates/libloading
[`memmap2`]: https://crates.io/crates/memmap2
[`region`]: https://crates.io/crates/region
