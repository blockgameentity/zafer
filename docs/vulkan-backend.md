# The Vulkan backend

Status: in progress, and Vulkan is the default backend. `victory-vulkan` has
the loader, adapters, device, swapchain and a presenter that acquires,
clears, submits and presents a real window
(`crates/victory-vulkan/src/presenter.rs`), and `victory-gpu::backend` picks
it unless `VICTORY_BACKEND=d3d12` says otherwise. So milestones 1 and 2 of
the work order below are wired up; the console's draws are still on the
D3D12 host (`crates/victory-gpu/src/host.rs`) over vkd3d-proton
(`crates/victory-shader/src/hostlib.rs`) with DXIL from
`crates/victory-shader/src/emit/`, which is what milestones 4-7 replace.
`VICTORY_BACKEND=vulkan` refuses the fallback so a Vulkan bug is never
hidden by a silent downgrade.

## Why

The console's memory model is Vulkan's:

- a GPU address is a CPU address, and memory is unified: the title writes a
  resource and the GPU reads the same bytes, no copy and no remapping
  ([`docs/linux.md`](linux.md), [`docs/architecture.md`](architecture.md));
- a read past the end of a resource returns 0 and a write is dropped, which is
  robustness, not a fault path the title can observe;
- descriptor handles are plain addresses (`descriptor.rs`: views 32 bytes,
  samplers 16, RDNA 2's own descriptor sizes).

D3D12 on Linux is vkd3d-proton: a full API translated onto Vulkan anyway. So a
console draw today walks four layers:

```
title -> D3D12.X (our HLE) -> our D3D12 objects -> vkd3d-proton -> Vulkan -> driver
```

The last two are the same work the backend does, done twice and with a second
set of address-space, robustness and descriptor rules to keep in step — and
vkd3d-proton's robust raw access chains have already faulted a host GPU once
(`emit/lower.rs`: `RAW_OFFSET_MAX`, "MMU Fault"). The backend removes the
middle layer and the vkd3d-proton dependency:

```
title -> D3D12.X (our HLE) -> our Vulkan backend -> Vulkan loader -> driver
```

D3D12 with DXIL stays for Windows and for comparing the two backends
(`docs/architecture.md`, "Host graphics: Vulkan first").

## Mapping

One row per D3D12.X concept, the Vulkan object our backend would use, and the
feature or extension it relies on. "Planned module" is where the code goes.

| D3D12.X | Vulkan | Note | Planned module |
|---------|--------|------|----------------|
| adapter, device (`device.rs`, `dxgi.rs`) | `VkInstance`, `VkPhysicalDevice`, `VkDevice` | instance extensions `VK_KHR_surface`, `VK_KHR_wayland_surface`/`_xlib_surface`/`_xcb_surface` (SDL3 already provides the surface, `linux.rs`); device features queried per adapter, not assumed | `vulkan/instance.rs`, `vulkan/device.rs` |
| command queue (`queue.rs`) | `VkQueue` + one `VkSemaphore` | timeline semaphore (`VK_KHR_timeline_semaphore`, core 1.2); the fence values the title submits and waits on are the semaphore's counter, so no per-submission event | `vulkan/queue.rs` |
| command allocator, command list (`list.rs`) | `VkCommandPool`, `VkCommandBuffer` | the console list is a recording target, not a pipeline of its own: each reset of the allocator resets deallocated buffers; recording is immediate, as the title's writes arrive as calls (our lists keep their limits at zero, `lib.rs`) | `vulkan/list.rs` |
| descriptor heap (`descriptor.rs`) | `VkDescriptorSet` or a descriptor buffer | `VK_EXT_descriptor_buffer`: the console heap is plain memory whose handles are addresses, and a descriptor buffer is exactly that addressable layout. Without it, a `VkDescriptorSet` per heap with `VK_DESCRIPTOR_BINDING_UPDATE_AFTER_BIND` | `vulkan/descriptor.rs` |
| root signature (`root.rs`, `resource.rs`, `emit/mod.rs` user data) | `VkPipelineLayout` | root constants are push constants (128 bytes is the guaranteed minimum; the title's user data can exceed it — see Risks); the console's heap-indexed descriptors need `VK_EXT_descriptor_indexing` (`runtimeDescriptorArray`, `shaderSampledImageArrayNonUniformIndexing`, `descriptorBindingPartiallyBound`, `descriptorBindingVariableDescriptorCount`) | `vulkan/root.rs` |
| PSO stream (`pipeline/`) | `VkPipeline` + `VkPipelineCache` | dynamic rendering state (`VK_KHR_dynamic_rendering`) folded into the pipeline; the PSO stream's subobjects map to the standard graphics state the console spells out. `VK_EXT_shader_object` is optional: the title makes ~60,000 pipelines in its first 20 s and draws with a few dozen (decision 0007 step 3), so shader objects would trade pipeline creation cost for per-draw binding | `vulkan/pipeline.rs` |
| render passes, render target and depth views (`resource.rs`, `primitive.rs`) | dynamic rendering (`vkCmdBeginRendering`) | `VK_KHR_dynamic_rendering` (core 1.3); attachment layouts declared per use, no render-pass objects and no subpass dependency graph | `vulkan/list.rs` |
| heaps and resources (`resource.rs`, `xmem`) | `VkDeviceMemory` + `vkAllocateMemory` | memory type chosen from the console heap's flags rather than vkd3d-proton's heuristics; `VK_EXT_memory_budget` gives the equivalent of the budget query | `vulkan/resource.rs` |
| GPU virtual memory (`D3D12ConfigureVirtualMemoryX`, `device.rs`) | buffer device address (`VK_KHR_buffer_device_address`) | the address range handed back is the process's own user space today (`device.rs` `user_space`); a buffer-device-address allocation placed at a known base, or `VK_EXT_external_memory_host` importing the title's page, ties a device address to a title pointer | `vulkan/resource.rs` |
| page-mapping copy (`CopyPageMappingsBatchX`, `queue.rs`) | `vkCmdCopyBuffer`, or aliased `VkDeviceMemory` | the batch maps pool pages at a destination, page kinds 0..3: aliasing the same `VkDeviceMemory` at several bases is the faithful form, a copy is the safe one; 64 KiB pages, the batch's page size | `vulkan/queue.rs` |
| fence (`fence.rs`) | `VkSemaphore` (timeline) | one semaphore per queue, counter compared, never reset: `vkWaitSemaphores` on the CPU and `vkQueueSubmit` `pWaitSemaphores` on the GPU read the same value | `vulkan/fence.rs` |
| query heap (`query.rs`) | `VkQueryPool` | occlusion and pipeline statistics; timed queries need `timestampComputeAndGraphics`/`timestampPeriod` divided out | `vulkan/query.rs` |
| command signature, indirect execution (`signature.rs`, `indirect.rs`) | `vkCmdDrawIndexedIndirect`, `vkCmdDrawIndirect`, `vkCmdDispatchIndirect` | `VK_EXT_multi_draw` where a console form has no 1:1 (a signature that concatenates draw arguments); the arguments still have to be clamped by hand where the console allows a count of zero and Vulkan does not | `vulkan/signature.rs` |
| predication (a signature's predication, `indirect.rs`) | `vkCmdBeginConditionalRenderingEXT` | `VK_EXT_conditional_rendering`; a console predicate on a fence value has no equivalent and must be resolved as a value | `vulkan/list.rs` |
| console resource planes / DCC | none | Vulkan exposes no DCC control: whether a render target is compressed is the driver's decision. Not a correctness difference, only bandwidth (see Risks) | `vulkan/texture.rs` |

`victory-formats` keeps owning the console's tiled layouts and detiling; the
backend consumes them as it does today.

## Robustness

The console's semantics are "an unbound descriptor and an out-of-range read
give 0, a write is dropped". The DXIL translator enforces that by hand today:
`emit/lower.rs` clamps a raw access chain's offset to `RAW_OFFSET_MAX`
(0x7FFF_FFF0) because a faulted robust chain killed a submission. With Vulkan:

- `VK_EXT_robustness2` `nullDescriptor` is the "unbound descriptor reads zero"
  rule for free: no null checks in the translated shader, and no OOB descriptor
  guarding on our side.
- `VK_EXT_robustness2` `robustBufferAccess` is "out-of-range reads give 0,
  writes dropped" for buffers, without an offset clamp travelling through the
  IR.

Given both, what becomes unnecessary: the null-view zero reads the emitter
spells out (`lower.rs`: "a null view reads zeros"), and the raw-offset clamp
`RAW_OFFSET_MAX`, once the backend reports `robustBufferAccess` and the device
honours it (test it per driver; see Risks).

What must stay:

- the loop limit (`LOOP_LIMIT`, 2^20): a shader loop over data the host has not
  mirrored is a hang, not a fault, and robustness does not bound iteration
  count;
- the clamping of indirect draw arguments: robustness bounds an access, not a
  draw count;
- every EXEC/lane and structural rule: `structure.rs` is about lane semantics,
  which no Vulkan feature subsumes.

Where a device lacks `VK_EXT_robustness2` the backend must fall back to what
the DXIL path does now, so the emitter's clamps stay behind a
`Robustness::{Extension, Emulated}` flag rather than being deleted.

## Shaders

The pipeline up to the IR does not change: DXBC container -> GFX10 -> IR
(`dxbc.rs`, `gfx10/`, `ir.rs`, `structure.rs`, `resource.rs`, `io.rs`,
`docs/shaders.md`). Only the backend changes.

| Step | DXIL (today) | SPIR-V (planned) |
|------|--------------|------------------|
| Emit | `emit/`, one shader per stage as SM 6.6 PC container | `victory-shader/src/spirv/`, one `OpEntryPoint` per stage module, `VkShaderModule` per stage |
| Descriptor binding | `ResourceDescriptorHeap[]`, `SamplerDescriptorHeap[]`, static samplers | descriptor layout derived from the console root signature (`root.rs`, `resource.rs`): a table becomes an array binding, a root descriptor a single binding, a root constant a push constant; non-uniform heap indices become `NonUniform`-decorated accesses (the waterfall trace in `resource.rs` already produces them) |
| Stage I/O | HLSL semantics from `io.rs` | `gl_VertexIndex`, `gl_InstanceIndex`, `gl_FragCoord`, `gl_Position`, `gl_FrontFacing`, and `Location`-numbered `in`/`out` pairs, from the same `io.rs` raising; interpolation qualifiers from the console's interpolation words |
| Samplers | `HostShader::static_samplers` | immutable `VkSampler` objects created from the console sampler words, or `VkSampler` in a descriptor set where the heap gives one |
| Wave size | `WaveSize` declared for compute | subgroup size control (`VK_EXT_subgroup_size_control`) where the console's 64-lane assumption needs it, otherwise the console's wave rules must already hold on any wave (`emit/lower.rs` `MaskData::Host`) |

The shader cache (`crates/victory-gpu/src/shader_cache.rs`) keys an entry by
the container's content and by the build hash (the BLAKE3 of our executable
plus the translation settings). The backend identity joins the key: the key
becomes `container hash + build hash + backend id` (Vulkan/SPIR-V or
D3D12/DXIL), so a SPIR-V entry is never served to the D3D12 backend and the
reverse. The SPIR-V entry needs its own encode/decode, or the same field added
to both; the entry's payload is a different shader binary either way.

## What stays

- `object.rs`: the vtable trick — our console objects are static, refcounted
  structs with a function table in the title's calling convention, and the
  title sees the same interfaces. Untouched by the backend choice.
- `console.rs`: which console model the title sees (Series X|S memory and
  feature differences). Untouched.
- the loader (`victory-loader`) and `victory-hle`: the title's code, its
  imports and the HLE modules are independent of which host API the GPU crate
  calls.
- `victory-formats`: tiled layouts, detiling, sizes.
- the D3D12 host (`host.rs`, `pipeline/host.rs`, `descriptor/host.rs`,
  `dxgi.rs`, `linux.rs`'s vkd3d presentation) stays as the Windows backend and
  as the comparison target.

What changes structurally: the D3D12 host becomes one of two backends behind
the same internal trait. The console-facing modules (`device.rs`, `queue.rs`,
`list.rs`, `resource.rs`, `descriptor.rs`, `root.rs`, `pipeline.rs`,
`fence.rs`, `query.rs`, `signature.rs`, `indirect.rs`) keep their exports and
their argument checking and call the trait; today they call `host.rs`
directly. That trait is the milestone-1 piece of real refactoring, and it is
what makes "two backends" affordable rather than a fork.

## Work order

Each milestone has a file list and an exit criterion someone can run. The
order is the order in which a wrong assumption is cheapest to find.

1. **Instance, device, proc loading, adapter enumeration.** Files:
   `crates/victory-gpu/src/vulkan/{mod,instance,device,loader}.rs`, plus the
   backend trait in `vulkan/mod.rs`. Load the Vulkan loader the way
   `victory-shader/src/hostlib.rs` loads vkd3d-proton today (dlopen, one
   `vkGetInstanceProcAddr`, never closed), enable nothing optional yet, and
   enumerate adapters through the existing adapter path (`dxgi.rs`).
   Exit: `victory --backend vulkan census` (or the equivalent probe run) lists
   the host's adapters with feature support, and a test with no GPU skips as
   the D3D12 tests do.
2. **Swapchain, present a cleared screen.** Files: `vulkan/{swapchain,present}.rs`,
   `linux.rs` (SDL3 surface). Present a clear through a real swapchain and a
   timeline semaphore, no title involved. *(Done: `presenter.rs`, with binary
   semaphores and one fence; `backend::present_boot_frame` proves it at
   boot.)*
   Exit: a window shows the clear colour for N frames, and a resize does not
   crash.
3. **One pipeline from a translated SPIR-V shader, one draw.** Files:
   `victory-shader/src/spirv/{mod,module,emit}.rs`,
   `vulkan/{pipeline,list,descriptor,root,resource}.rs`, `shader_cache.rs`.
   The smallest real shader: a vertex/pixel pair emitted from the IR, one
   descriptor, one push constant, one draw into the swapchain image.
   Exit: a triangle renders from a console container that the DXIL path also
   translates, and its pixels match the D3D12 result within a tolerance.
4. **Resources, heaps and the console memory model.** Files:
   `vulkan/{resource,texture}.rs`, `device.rs`. Buffer device address,
   `D3D12ConfigureVirtualMemoryX` answered from the Vulkan side, a heap whose
   descriptors are addresses.
   Exit: the title's copy paths run on Vulkan-allocated memory and a read back
   of a cleared target matches the D3D12 read back.
5. **Command lists, submission and fences.** Files: `vulkan/{list,queue,fence}.rs`,
   `list.rs`, `queue.rs`, `fence.rs`. The title's lists record into Vulkan
   command buffers and its fence values are semaphore counters.
   Exit: the menu frame sequence submits and completes with fences the title
   observes, ordinary and with intermediate waits.
6. **Robustness and descriptor indexing.** Files: `vulkan/device.rs`,
   `victory-shader/src/spirv/`, `emit/lower.rs` (the `Robustness` switch).
   Exit: the conformance cases for a null descriptor and an out-of-range read
   return 0 (not a fault, not garbage) on AMD and NVIDIA, and the shader path
   reports which clamps it kept.
7. **Pipelines from streams, indirect and queries.** Files: `vulkan/{pipeline,signature,query}.rs`,
   `signature.rs`, `query.rs`, `indirect.rs`. Pipeline cache on disk, command
   signatures, indirect draws, occlusion queries.
   Exit: the world's pipeline set is created and a pipeline cache entry is
   reused across runs.
8. **Menus render.** Files: `linux.rs`, `apps/victory`, the diagnostics capture
   path (`capture.rs`).
   Exit: the menu screens render on Vulkan and the frame time is within the
   D3D12 backend's on the same machine, with the same screens present.

Honest scope: milestones 1-3 are the cheap ones and 4-7 are where the console
semantics live. Step 8 is a target, not a promise: the world's remaining
failures (untranslated shaders, missing depth bounds, HDR and scaling) are
independent of the backend and still have to be fixed either way.

## Risks

| Risk | What it costs | What we do about it |
|------|---------------|---------------------|
| `VK_EXT_robustness2` absent or wrong on a driver | the console's out-of-range-zero reads must be emulated in the shader, as the DXIL path does now | keep the `Robustness` switch and the emitter's clamps; test each driver explicitly (matching README's one tested machine) |
| `VK_EXT_descriptor_buffer` absent (notably older drivers) | descriptor heaps fall back to `VkDescriptorSet`s and per-draw updates cost CPU time | ship both paths from the start of milestone 3 |
| `VK_EXT_descriptor_indexing` limits on older drivers | the title's directly indexed heaps are the reason the DXIL path needs SM 6.6; a device without them cannot run the world | treat it as a required feature and refuse the adapter clearly rather than degrade silently |
| Push-constant limit (128 bytes guaranteed) | the console's user data can exceed it; the overflow must go to a uniform buffer, which changes the emitted layout per shader | measure the real user-data sizes in milestone 3 and pick the split then, not later |
| DCC lost | the console compresses render targets; Vulkan gives no control, so bandwidth rises (worst on the world's targets) | measure in milestone 8; a real cost, not something to design around |
| Driver bugs, especially with dynamic rendering and non-uniform indexing | hash hangs or misrenders that are not ours | keep the D3D12 backend as the reference, and the pixel-comparison exit criteria in steps 3 and 8 |
| Two backends to keep in step | every console-facing module now has two implementations to satisfy | the shared trait plus the shared shader IR; the Windows backend is the one that must not regress |
| Vulkan feature spread (AMD, NVIDIA, Intel) | one feature set on one machine proves nothing (README's single tested GPU) | the adapter enumeration in milestone 1 reports the gaps before any pipeline runs |

## See also

- [`docs/architecture.md`](architecture.md), "Host graphics: Vulkan first"
- [`docs/linux.md`](linux.md), GPU
- [`docs/shaders.md`](shaders.md)
