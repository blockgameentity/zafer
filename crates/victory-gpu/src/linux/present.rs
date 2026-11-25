//! Presentation on Linux: vkd3d-proton's own swap chain over an SDL window.
//!
//! This is the D3D12 backend's presentation path (the fallback one): the
//! swap chain is vkd3d-proton's (`IDXGIVkSwapChainFactory` on the command
//! queue, the interface its DXGI uses), on a surface our window provides
//! through `IDXGIVkSurfaceFactory`. The methods are named after the DXGI
//! ones `host.rs` calls.
//!
//! The Vulkan backend presents through `victory_vulkan::Presenter` instead
//! and does not come through here.
// COM methods keep their API names.
#![allow(non_snake_case)]

use std::ffi::c_void;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Resource};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

use super::{run, window};

/// `IDXGIVkSwapChainFactory`: vkd3d-proton's command queues make their
/// swap chains through it.
#[windows_core::interface("e7d6c3ca-23a0-4e08-9f2f-ea5231df6633")]
unsafe trait IDXGIVkSwapChainFactory: IUnknown {
    fn CreateSwapChain(
        &self,
        surfaces: *mut SurfaceFactory,
        desc: *const DXGI_SWAP_CHAIN_DESC1,
        out: *mut *mut c_void,
    ) -> HRESULT;
}

/// `IDXGIVkSwapChain`, as far as we call it.
#[windows_core::interface("e4a9059e-b569-46ab-8de7-501bd2bc7f7a")]
unsafe trait IDXGIVkSwapChain: IUnknown {
    fn GetDesc(&self, desc: *mut DXGI_SWAP_CHAIN_DESC1) -> HRESULT;
    fn GetAdapter(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT;
    fn GetDevice(&self, iid: *const GUID, out: *mut *mut c_void) -> HRESULT;
    fn GetImage(&self, index: u32, iid: *const GUID, out: *mut *mut c_void) -> HRESULT;
    fn GetImageIndex(&self) -> u32;
    fn GetFrameLatency(&self) -> u32;
    fn GetFrameLatencyEvent(&self) -> HANDLE;
    fn ChangeProperties(
        &self,
        desc: *const DXGI_SWAP_CHAIN_DESC1,
        node_masks: *const u32,
        queues: *const *mut c_void,
    ) -> HRESULT;
    fn SetPresentRegion(&self, region: *const c_void) -> HRESULT;
    fn SetGammaControl(&self, count: u32, points: *const c_void) -> HRESULT;
    fn SetFrameLatency(&self, latency: u32) -> HRESULT;
    fn Present(&self, interval: u32, flags: u32, parameters: *const c_void) -> HRESULT;
}

/// `IDXGIVkSurfaceFactory` for our window: the swap chain asks it for a
/// Vulkan surface on vkd3d-proton's instance. Lives as long as the process.
#[repr(C)]
struct SurfaceFactory {
    vtable: &'static SurfaceVtable,
    window: usize,
}

#[repr(C)]
struct SurfaceVtable {
    query: unsafe extern "system" fn(*mut SurfaceFactory, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut SurfaceFactory) -> u32,
    release: unsafe extern "system" fn(*mut SurfaceFactory) -> u32,
    create_surface:
        unsafe extern "system" fn(*mut SurfaceFactory, *mut c_void, *mut c_void, *mut u64) -> i32,
}

const SURFACE_FACTORY: GUID = GUID::from_u128(0x1e7895a1_1bc3_4f9c_a670_290a4bc9581a);

unsafe extern "system" fn surface_query(
    this: *mut SurfaceFactory,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    // SAFETY: vkd3d-proton passes valid pointers.
    unsafe {
        if *iid == SURFACE_FACTORY || *iid == IUnknown::IID {
            *out = this.cast();
            HRESULT(0)
        } else {
            *out = std::ptr::null_mut();
            windows::Win32::Foundation::E_NOINTERFACE
        }
    }
}

unsafe extern "system" fn surface_ref(_: *mut SurfaceFactory) -> u32 {
    1
}

/// `VkResult`: 0 or `VK_ERROR_INITIALIZATION_FAILED`.
unsafe extern "system" fn create_surface(
    this: *mut SurfaceFactory,
    instance: *mut c_void,
    _adapter: *mut c_void,
    surface: *mut u64,
) -> i32 {
    // SAFETY: our factory.
    let window = unsafe { (*this).window };
    let (instance, out) = (instance as usize, surface as usize);
    let made = run(move |api, _| {
        // SAFETY: our window, vkd3d-proton's instance, its out pointer
        // (it waits for us).
        let ok = unsafe {
            (api.create_surface)(
                window as *mut c_void,
                instance as *mut c_void,
                std::ptr::null(),
                out as *mut u64,
            )
        };
        if !ok {
            tracing::warn!(error = api.last_error(), "no Vulkan surface");
        }
        ok
    });
    if made == Some(true) { 0 } else { -3 }
}

static SURFACE_VTABLE: SurfaceVtable = SurfaceVtable {
    query: surface_query,
    add_ref: surface_ref,
    release: surface_ref,
    create_surface,
};

/// vkd3d-proton's swap chain on our window.
pub(crate) struct SwapChain {
    chain: IDXGIVkSwapChain,
    desc: DXGI_SWAP_CHAIN_DESC1,
}

impl SwapChain {
    /// A swap chain on `queue` for a window of this size.
    pub(crate) fn new(
        queue: &ID3D12CommandQueue,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        buffers: u32,
    ) -> windows::core::Result<SwapChain> {
        let fail = |e: String| {
            tracing::warn!("{e}");
            windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL)
        };
        let window = window(width, height).map_err(fail)?;
        let factory: IDXGIVkSwapChainFactory = queue.cast()?;
        let surfaces = Box::leak(Box::new(SurfaceFactory {
            vtable: &SURFACE_VTABLE,
            window,
        }));
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: format,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: buffers,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
            ..Default::default()
        };
        let mut chain = std::ptr::null_mut();
        // SAFETY: our factory object and description; one reference ours.
        unsafe {
            factory.CreateSwapChain(surfaces, &desc, &mut chain).ok()?;
            Ok(SwapChain {
                chain: IDXGIVkSwapChain::from_raw(chain),
                desc,
            })
        }
    }

    pub(crate) unsafe fn GetCurrentBackBufferIndex(&self) -> u32 {
        // SAFETY: a live swap chain.
        unsafe { self.chain.GetImageIndex() }
    }

    pub(crate) unsafe fn GetBuffer<T: Interface>(&self, index: u32) -> windows::core::Result<T> {
        debug_assert_eq!(T::IID, ID3D12Resource::IID);
        let mut out = std::ptr::null_mut();
        // SAFETY: a live swap chain; one reference ours.
        unsafe {
            self.chain.GetImage(index, &T::IID, &mut out).ok()?;
            Ok(T::from_raw(out))
        }
    }

    pub(crate) unsafe fn ResizeBuffers(
        &mut self,
        buffers: u32,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
        _: DXGI_SWAP_CHAIN_FLAG,
    ) -> windows::core::Result<()> {
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: format,
            BufferCount: buffers,
            ..self.desc
        };
        // SAFETY: no back buffer is held (the caller's promise).
        unsafe {
            self.chain
                .ChangeProperties(&desc, std::ptr::null(), std::ptr::null())
                .ok()?
        };
        self.desc = desc;
        Ok(())
    }

    pub(crate) unsafe fn Present(&self, interval: u32, flags: DXGI_PRESENT) -> HRESULT {
        // SAFETY: a live swap chain.
        unsafe { self.chain.Present(interval, flags.0, std::ptr::null()) }
    }
}
