//! The window, presentation and host input on Linux.
//!
//! SDL3 (`libSDL3.so.0`, loaded at run time) gives the window, its Vulkan
//! surface, the keyboard and gamepads with rumble: one library for what
//! Win32, DXGI and XInput give on Windows. One thread of ours owns SDL: it
//! pumps events and runs what other threads ask of it ([`run`]).
//!
//! The swap chain is vkd3d-proton's own (`IDXGIVkSwapChainFactory` on the
//! command queue, the interface its DXGI uses), on a surface we make for
//! it ([`SwapChain`]); its methods are named after the DXGI ones
//! `host.rs` calls.
// COM methods keep their API names.
#![allow(non_snake_case)]

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use victory_shader::hostlib::Library;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Graphics::Direct3D12::{ID3D12CommandQueue, ID3D12Resource};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_FLIP_DISCARD,
    DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::core::{GUID, HRESULT, IUnknown, IUnknown_Vtbl, Interface};

const INIT_VIDEO: u32 = 0x20;
const INIT_GAMEPAD: u32 = 0x2000;
const WINDOW_RESIZABLE: u64 = 0x20;
const WINDOW_VULKAN: u64 = 0x1000_0000;
const EVENT_QUIT: u32 = 0x100;
const EVENT_WINDOW_CLOSE_REQUESTED: u32 = 0x210;
const EVENT_GAMEPAD_ADDED: u32 = 0x653;
const EVENT_GAMEPAD_REMOVED: u32 = 0x654;
/// `SDL_GamepadDeviceEvent::which`.
const EVENT_WHICH: usize = 16;
/// Gamepad slots, as XInput's.
pub const PADS: usize = 4;
/// `SDL_SCANCODE_COUNT`.
const SCANCODES: usize = 512;

/// The SDL3 functions we call.
struct Api {
    init: unsafe extern "C" fn(u32) -> bool,
    set_hint: unsafe extern "C" fn(*const c_char, *const c_char) -> bool,
    error: unsafe extern "C" fn() -> *const c_char,
    poll_event: unsafe extern "C" fn(*mut [u8; 128]) -> bool,
    create_window: unsafe extern "C" fn(*const c_char, c_int, c_int, u64) -> *mut c_void,
    set_window_title: unsafe extern "C" fn(*mut c_void, *const c_char) -> bool,
    create_surface: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void, *mut u64) -> bool,
    keyboard_focus: unsafe extern "C" fn() -> *mut c_void,
    keyboard_state: unsafe extern "C" fn(*mut c_int) -> *const bool,
    open_gamepad: unsafe extern "C" fn(u32) -> *mut c_void,
    close_gamepad: unsafe extern "C" fn(*mut c_void),
    gamepad_button: unsafe extern "C" fn(*mut c_void, c_int) -> bool,
    gamepad_axis: unsafe extern "C" fn(*mut c_void, c_int) -> i16,
    rumble_gamepad: unsafe extern "C" fn(*mut c_void, u16, u16, u32) -> bool,
}

impl Api {
    fn load() -> Result<Api, String> {
        let path = std::env::var_os("VICTORY_SDL3").unwrap_or_else(|| "libSDL3.so.0".into());
        let sdl = Library::open(Path::new(&path))
            .map_err(|e| format!("no SDL3 ({e}); install sdl3 or set VICTORY_SDL3"))?;
        let f = |name: &CStr| sdl.symbol(name);
        // SAFETY: each export has the signature it is given here (SDL3's
        // headers, `SDLCALL` being the C convention on x86-64 Linux).
        unsafe {
            use std::mem::transmute as t;
            Ok(Api {
                init: t(f(c"SDL_Init")?),
                set_hint: t(f(c"SDL_SetHint")?),
                error: t(f(c"SDL_GetError")?),
                poll_event: t(f(c"SDL_PollEvent")?),
                create_window: t(f(c"SDL_CreateWindow")?),
                set_window_title: t(f(c"SDL_SetWindowTitle")?),
                create_surface: t(f(c"SDL_Vulkan_CreateSurface")?),
                keyboard_focus: t(f(c"SDL_GetKeyboardFocus")?),
                keyboard_state: t(f(c"SDL_GetKeyboardState")?),
                open_gamepad: t(f(c"SDL_OpenGamepad")?),
                close_gamepad: t(f(c"SDL_CloseGamepad")?),
                gamepad_button: t(f(c"SDL_GetGamepadButton")?),
                gamepad_axis: t(f(c"SDL_GetGamepadAxis")?),
                rumble_gamepad: t(f(c"SDL_RumbleGamepad")?),
            })
        }
    }

    fn last_error(&self) -> String {
        // SAFETY: SDL returns a NUL-terminated message, maybe empty.
        unsafe { CStr::from_ptr((self.error)()) }
            .to_string_lossy()
            .into_owned()
    }
}

/// What other threads ask of the SDL thread.
type Job = Box<dyn FnOnce(&Api, &mut State) + Send>;

/// The SDL thread's own state.
#[derive(Default)]
struct State {
    /// Our window, if made.
    window: usize,
    /// Whether SDL video is up (a display was found).
    video: bool,
}

struct Sdl {
    api: Api,
    jobs: Mutex<Sender<Job>>,
    /// Open gamepads by slot: (instance id, `SDL_Gamepad`).
    pads: Mutex<[Option<(u32, usize)>; PADS]>,
    /// The window made for presenting (0 before).
    window: std::sync::atomic::AtomicUsize,
}

// SAFETY: SDL's gamepad and keyboard-state calls lock internally; window
// calls go through the SDL thread (`run`).
unsafe impl Sync for Sdl {}
// SAFETY: as above.
unsafe impl Send for Sdl {}

/// SDL, started on first use; `None` without SDL3.
fn sdl() -> Option<&'static Sdl> {
    static SDL: OnceLock<Option<Sdl>> = OnceLock::new();
    SDL.get_or_init(|| match start() {
        Ok(sdl) => Some(sdl),
        Err(e) => {
            tracing::warn!("{e}; no window or host input");
            None
        }
    })
    .as_ref()
}

fn start() -> Result<Sdl, String> {
    let api = Api::load()?;
    let (jobs, receiver) = channel::<Job>();
    let (ready, started) = channel::<Result<bool, String>>();
    std::thread::Builder::new()
        .name("victory-window".into())
        .spawn(move || pump(receiver, ready))
        .map_err(|e| e.to_string())?;
    let video = started.recv().map_err(|e| e.to_string())??;
    tracing::info!(video, "SDL3 started");
    Ok(Sdl {
        api,
        jobs: Mutex::new(jobs),
        pads: Mutex::new([None; PADS]),
        window: 0.into(),
    })
}

/// The SDL thread: initializes SDL, then pumps events and runs jobs.
fn pump(jobs: Receiver<Job>, ready: Sender<Result<bool, String>>) {
    let api = match Api::load() {
        Ok(api) => api,
        Err(e) => {
            let _ = ready.send(Err(e));
            return;
        }
    };
    // SAFETY: NUL-terminated strings; SDL calls on its own thread.
    let video = unsafe {
        // Pads work while another window has focus, as XInput's do.
        (api.set_hint)(
            c"SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS".as_ptr(),
            c"1".as_ptr(),
        );
        (api.init)(INIT_VIDEO | INIT_GAMEPAD) || {
            tracing::warn!(error = api.last_error(), "no SDL video; no window");
            false
        }
    };
    // SAFETY: as above.
    if !video && !unsafe { (api.init)(INIT_GAMEPAD) } {
        let _ = ready.send(Err(format!("SDL3 did not start: {}", api.last_error())));
        return;
    }
    let _ = ready.send(Ok(video));
    let mut state = State {
        video,
        ..Default::default()
    };
    let mut event = [0u8; 128];
    loop {
        while let Ok(job) = jobs.try_recv() {
            job(&api, &mut state);
        }
        // SAFETY: room for one event.
        while unsafe { (api.poll_event)(&mut event) } {
            let kind = u32::from_ne_bytes(event[..4].try_into().unwrap());
            let which = u32::from_ne_bytes(event[EVENT_WHICH..EVENT_WHICH + 4].try_into().unwrap());
            match kind {
                EVENT_QUIT | EVENT_WINDOW_CLOSE_REQUESTED => {
                    // The runtime's SIGTERM handler ends the title as the
                    // console does (suspend, then end).
                    tracing::info!("window closed; ending the run");
                    // SAFETY: raise has no preconditions.
                    unsafe { libc::raise(libc::SIGTERM) };
                }
                EVENT_GAMEPAD_ADDED => connect(&api, which),
                EVENT_GAMEPAD_REMOVED => disconnect(&api, which),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(4));
    }
}

fn connect(api: &Api, id: u32) {
    let Some(sdl) = sdl() else { return };
    let mut pads = sdl.pads.lock().unwrap_or_else(|e| e.into_inner());
    if pads.iter().flatten().any(|&(open, _)| open == id) {
        return;
    }
    let Some(slot) = pads.iter().position(Option::is_none) else {
        return;
    };
    // SAFETY: an id from SDL's added event.
    let pad = unsafe { (api.open_gamepad)(id) };
    if !pad.is_null() {
        tracing::info!(slot, "gamepad connected");
        pads[slot] = Some((id, pad as usize));
    }
}

fn disconnect(api: &Api, id: u32) {
    let Some(sdl) = sdl() else { return };
    let mut pads = sdl.pads.lock().unwrap_or_else(|e| e.into_inner());
    for (slot, pad) in pads.iter_mut().enumerate() {
        if let Some((open, handle)) = *pad
            && open == id
        {
            tracing::info!(slot, "gamepad disconnected");
            // SAFETY: a gamepad we opened.
            unsafe { (api.close_gamepad)(handle as *mut c_void) };
            *pad = None;
        }
    }
}

/// Runs `job` on the SDL thread and waits for its answer.
fn run<T: Send + 'static>(job: impl FnOnce(&Api, &mut State) -> T + Send + 'static) -> Option<T> {
    let sdl = sdl()?;
    let (answer, answered) = channel();
    let job: Job = Box::new(move |api, state| {
        let _ = answer.send(job(api, state));
    });
    sdl.jobs
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .send(job)
        .ok()?;
    answered.recv().ok()
}

/// Our window of the given size, made on first call; `None` without a
/// display.
fn window(width: u32, height: u32) -> Result<usize, String> {
    let sdl = sdl().ok_or("no SDL3")?;
    let made = run(move |api, state| {
        if state.window == 0 && state.video {
            // SAFETY: a NUL-terminated title; on the SDL thread.
            state.window = unsafe {
                (api.create_window)(
                    c"VIctory".as_ptr(),
                    width as c_int,
                    height as c_int,
                    WINDOW_VULKAN | WINDOW_RESIZABLE,
                )
            } as usize;
            if state.window == 0 {
                return Err(format!("no window: {}", api.last_error()));
            }
        }
        if state.window == 0 {
            return Err("no display for a window".into());
        }
        Ok(state.window)
    })
    .ok_or("SDL thread gone")??;
    sdl.window.store(made, std::sync::atomic::Ordering::Relaxed);
    Ok(made)
}

/// Sets our window's caption; never waits for the SDL thread.
pub(crate) fn caption(text: String) {
    let Some(sdl) = sdl() else { return };
    let Ok(text) = CString::new(text) else { return };
    let job: Job = Box::new(move |api, state| {
        if state.window != 0 {
            // SAFETY: our window and a NUL-terminated text.
            unsafe { (api.set_window_title)(state.window as *mut c_void, text.as_ptr()) };
        }
    });
    let _ = sdl.jobs.lock().unwrap_or_else(|e| e.into_inner()).send(job);
}

/// A host gamepad as SDL3 reports it: button `i` (`SDL_GamepadButton`) is
/// bit `i`; axes by `SDL_GamepadAxis` (sticks with down positive, triggers
/// 0 to 32767).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Pad {
    pub buttons: u32,
    pub axes: [i16; 6],
}

/// The gamepad in `slot`, `None` when the slot is empty (or no SDL3).
pub fn pad(slot: usize) -> Option<Pad> {
    let sdl = sdl()?;
    let handle = sdl
        .pads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(slot)
        .copied()??
        .1;
    let pad = handle as *mut c_void;
    let mut state = Pad::default();
    // SAFETY: an open gamepad; SDL locks its joysticks for these reads.
    unsafe {
        for button in 0..15 {
            if (sdl.api.gamepad_button)(pad, button) {
                state.buttons |= 1 << button;
            }
        }
        for (axis, value) in state.axes.iter_mut().enumerate() {
            *value = (sdl.api.gamepad_axis)(pad, axis as c_int);
        }
    }
    Some(state)
}

/// Sets `slot`'s motor speeds (low frequency, high frequency) until the
/// next call.
pub fn rumble(slot: usize, low: u16, high: u16) {
    let Some(sdl) = sdl() else { return };
    let Some(Some((_, handle))) = sdl
        .pads
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .get(slot)
        .copied()
    else {
        return;
    };
    // SAFETY: an open gamepad. SDL needs a duration; the title renews it.
    unsafe { (sdl.api.rumble_gamepad)(handle as *mut c_void, low, high, 1000) };
}

/// SDL scan codes (USB HID usages) held down while our window has the
/// keyboard focus.
pub fn keys() -> Vec<u16> {
    let Some(sdl) = sdl() else { return Vec::new() };
    let ours = sdl.window.load(std::sync::atomic::Ordering::Relaxed);
    // SAFETY: SDL's own state arrays, valid for its lifetime.
    unsafe {
        if ours == 0 || (sdl.api.keyboard_focus)() as usize != ours {
            return Vec::new();
        }
        let mut count = 0;
        let state = (sdl.api.keyboard_state)(&mut count);
        let count = (count.max(0) as usize).min(SCANCODES);
        (0..count as u16)
            .filter(|&i| *state.add(usize::from(i)))
            .collect()
    }
}

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
