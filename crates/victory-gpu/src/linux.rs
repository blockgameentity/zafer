//! The window, presentation and host input on Linux.
//!
//! SDL3 (`libSDL3.so.0`, loaded at run time) gives the window, its Vulkan
//! surface, the keyboard and gamepads with rumble: one library for what
//! Win32, DXGI and XInput give on Windows. One thread of ours owns SDL: it
//! pumps events and runs what other threads ask of it ([`run`]).
//!
//! Presentation is split by backend: [`present`] is vkd3d-proton's swap
//! chain (the D3D12 path), and the Vulkan path asks this module for its
//! surface ([`vulkan_surface`]) and builds its own swapchain
//! (`victory_vulkan::Presenter`). [`input`] is the keyboard and the pads.

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::path::Path;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use victory_shader::hostlib::Library;

// The flag and event numbers come from `sdl3-sys`, which is compiled for
// its declarations only (`no-link`): SDL is still opened at run time as
// `libSDL3.so.0`, so a machine without it runs without a window rather
// than failing to start. What the crate replaces is every hand-copied
// constant and every byte offset into the event union.
use sdl3_sys::everything::{
    SDL_EventType, SDL_InitFlags, SDL_WINDOW_RESIZABLE, SDL_WINDOW_VULKAN,
};

mod input;
pub(crate) mod present;
pub(crate) mod wayland;

pub use input::{Pad, keys, pad, rumble};
pub(crate) use present::SwapChain;

/// Gamepad slots, as XInput's.
pub const PADS: usize = 4;

/// The SDL3 functions we call.
struct Api {
    init: unsafe extern "C" fn(SDL_InitFlags) -> bool,
    set_hint: unsafe extern "C" fn(*const c_char, *const c_char) -> bool,
    error: unsafe extern "C" fn() -> *const c_char,
    poll_event: unsafe extern "C" fn(*mut sdl3_sys::events::SDL_Event) -> bool,
    create_window:
        unsafe extern "C" fn(*const c_char, c_int, c_int, sdl3_sys::video::SDL_WindowFlags) -> *mut c_void,
    set_window_title: unsafe extern "C" fn(*mut c_void, *const c_char) -> bool,
    create_surface: unsafe extern "C" fn(*mut c_void, *mut c_void, *const c_void, *mut u64) -> bool,
    vulkan_extensions: unsafe extern "C" fn(*mut u32) -> *const *const c_char,
    window_pixels: unsafe extern "C" fn(*mut c_void, *mut c_int, *mut c_int) -> bool,
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
                vulkan_extensions: t(f(c"SDL_Vulkan_GetInstanceExtensions")?),
                window_pixels: t(f(c"SDL_GetWindowSizeInPixels")?),
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
        (api.init)(SDL_InitFlags::VIDEO | SDL_InitFlags::GAMEPAD) || {
            tracing::warn!(error = api.last_error(), "no SDL video; no window");
            false
        }
    };
    // SAFETY: as above.
    if !video && !unsafe { (api.init)(SDL_InitFlags::GAMEPAD) } {
        let _ = ready.send(Err(format!("SDL3 did not start: {}", api.last_error())));
        return;
    }
    let _ = ready.send(Ok(video));
    let mut state = State {
        video,
        ..Default::default()
    };
    // SAFETY: an all-zero event is SDL's own `SDL_EVENT_FIRST` shape, and
    // the union is only read through the variant the event's type names.
    let mut event: sdl3_sys::events::SDL_Event = unsafe { std::mem::zeroed() };
    loop {
        while let Ok(job) = jobs.try_recv() {
            job(&api, &mut state);
        }
        // SAFETY: room for one event, which SDL fills in.
        while unsafe { (api.poll_event)(&mut event) } {
            // SAFETY: `type` is the tag every variant starts with.
            let kind = unsafe { event.r#type };
            // SAFETY: a gamepad device event's `which` is its own field.
            let which = unsafe { event.gdevice.which };
            match SDL_EventType(kind) {
                SDL_EventType::QUIT | SDL_EventType::WINDOW_CLOSE_REQUESTED => {
                    // The runtime's SIGTERM handler ends the title as the
                    // console does (suspend, then end).
                    tracing::info!("window closed; ending the run");
                    // SAFETY: raise has no preconditions.
                    unsafe { libc::raise(libc::SIGTERM) };
                }
                SDL_EventType::GAMEPAD_ADDED => connect(&api, which.0),
                SDL_EventType::GAMEPAD_REMOVED => disconnect(&api, which.0),
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
pub(crate) fn window(width: u32, height: u32) -> Result<usize, String> {
    let sdl = sdl().ok_or("no SDL3")?;
    let made = run(move |api, state| {
        if state.window == 0 && state.video {
            // SAFETY: a NUL-terminated title; on the SDL thread.
            state.window = unsafe {
                (api.create_window)(
                    c"VIctory".as_ptr(),
                    width as c_int,
                    height as c_int,
                    SDL_WINDOW_VULKAN | SDL_WINDOW_RESIZABLE,
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

/// The instance extensions our own Wayland surface needs.
///
/// Native Wayland, not SDL: the surface is created by `victory-vulkan` with
/// `vkCreateWaylandSurfaceKHR`, so the one extension required is
/// `VK_KHR_wayland_surface` and it has to be enabled when the instance is
/// created.
pub(crate) fn wayland_instance_extensions() -> Vec<String> {
    vec!["VK_KHR_wayland_surface".to_owned()]
}

/// The instance extensions SDL needs for a surface on its own window.
///
/// The fallback for a machine where our Wayland path has no compositor: SDL
/// can make a surface on X11 or Wayland. Every one has to be enabled when
/// the instance is created, before the window can carry a `VkSurfaceKHR`.
pub(crate) fn vulkan_instance_extensions() -> Vec<String> {
    if sdl().is_none() {
        return Vec::new();
    }
    run(move |api, _| {
        let mut count = 0u32;
        // SAFETY: SDL fills the count and returns its own string array.
        let names = unsafe { (api.vulkan_extensions)(&mut count) };
        if names.is_null() {
            return Vec::new();
        }
        (0..count as usize)
            // SAFETY: `count` entries, as SDL just reported.
            .filter_map(|i| {
                let name = unsafe { *names.add(i) };
                (!name.is_null()).then(|| unsafe { CStr::from_ptr(name) }.to_string_lossy().into_owned())
            })
            .collect()
    })
    .unwrap_or_default()
}

/// A `VkSurfaceKHR` for our window on `instance`, or `None` without SDL.
///
/// SDL makes the surface on its own thread, which is why this goes through
/// [`run`]; the handle belongs to `instance` and to us until it is
/// destroyed.
pub(crate) fn vulkan_surface(instance: u64) -> Option<u64> {
    run(move |api, state| {
        if state.window == 0 {
            return None;
        }
        let mut surface = 0u64;
        // SAFETY: our window, a live instance handle, and SDL's own out
        // pointer.
        let ok = unsafe {
            (api.create_surface)(
                state.window as *mut c_void,
                instance as *mut c_void,
                std::ptr::null(),
                &mut surface,
            )
        };
        if !ok {
            tracing::warn!(error = api.last_error(), "no Vulkan surface");
            return None;
        }
        Some(surface)
    })
    .flatten()
}

/// Our window's drawable size in pixels, as the swapchain must match it.
pub(crate) fn window_pixels() -> Option<(u32, u32)> {
    run(move |api, state| {
        if state.window == 0 {
            return None;
        }
        let (mut width, mut height) = (0 as c_int, 0 as c_int);
        // SAFETY: our window and two out pointers.
        let ok = unsafe { (api.window_pixels)(state.window as *mut c_void, &mut width, &mut height) };
        (ok && width > 0 && height > 0).then_some((width as u32, height as u32))
    })
    .flatten()
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

