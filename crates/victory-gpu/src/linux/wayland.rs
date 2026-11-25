//! The window on native Wayland: a `wl_surface` with an `xdg_toplevel`, and
//! the `wl_display`/`wl_surface` pair `vkCreateWaylandSurfaceKHR` takes.
//!
//! `victory` speaks the protocol itself (`wayland-client`), so the window is
//! a real Wayland toplevel and the Vulkan surface is ours: no X11, no SDL,
//! no translation of Win32. SDL3 stays for gamepads with rumble only.
//!
//! One thread owns the connection and dispatches every event; the rest of
//! the runtime reads the answers out of [`Shared`] (the size the compositor
//! configured, the keys held, whether the window was closed) and sends the
//! few requests it needs through [`Wayland::send`].

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Sender, channel};
use std::sync::{Mutex, OnceLock};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_compositor, wl_keyboard, wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

mod keys;

pub use keys::scan;

/// What other threads ask and answer of the window.
#[derive(Default)]
pub struct Shared {
    /// The size the compositor last configured.
    size: Mutex<(u32, u32)>,
    /// Raised when the compositor closed the toplevel.
    closed: AtomicBool,
    /// Whether our toplevel has the keyboard focus.
    focused: AtomicBool,
    /// The evdev codes of the keys held down.
    held: Mutex<HashSet<u16>>,
}

/// The open window. The two pointers are the display and the surface the
/// event thread created; they live as long as the process's connection,
/// which the thread never drops.
pub struct Wayland {
    pub display: *mut c_void,
    pub surface: *mut c_void,
    shared: &'static Shared,
    jobs: Sender<Job>,
}

// SAFETY: the pointers are Wayland handles that outlive every user (the
// event thread keeps the connection for the process's life) and are only
// passed to Vulkan, which requires them on the presenting thread.
unsafe impl Send for Wayland {}
// SAFETY: as above; nothing writes through them from Rust.
unsafe impl Sync for Wayland {}

type Job = Box<dyn FnOnce(&mut Window) + Send>;

impl Wayland {
    /// The state other threads read: size, focus, keys, close.
    pub fn shared(&self) -> &'static Shared {
        self.shared
    }

    /// Runs `job` on the event thread, where the protocol objects live.
    pub fn send(&self, job: Job) {
        let _ = self.jobs.send(job);
    }

    /// The configured size, once the compositor has sent one.
    pub fn size(&self) -> Option<(u32, u32)> {
        let size = *self
            .shared
            .size
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        (size.0 > 0 && size.1 > 0).then_some(size)
    }

    /// Whether the compositor closed the window.
    pub fn closed(&self) -> bool {
        self.shared.closed.load(Ordering::Relaxed)
    }

    /// The SDL scan codes held down while our toplevel has the focus.
    pub fn keys(&self) -> Vec<u16> {
        if !self.shared.focused.load(Ordering::Relaxed) {
            return Vec::new();
        }
        self.shared
            .held
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .filter_map(|&code| keys::scan(code))
            .collect()
    }

    /// Sets the toplevel's title.
    pub fn set_title(&self, title: String) {
        self.send(Box::new(move |window| {
            window.toplevel.set_title(title);
            window.surface.commit();
        }));
    }

    /// Asks the compositor for a size (a hint: it decides what we get).
    pub fn resize(&self, width: u32, height: u32) {
        self.send(Box::new(move |window| {
            window.toplevel.set_min_size(1, 1);
            window.toplevel.set_max_size(0, 0);
            let _ = (width, height);
        }));
    }
}

/// The protocol objects, owned by the event thread.
struct Window {
    surface: wl_surface::WlSurface,
    wm_base: xdg_wm_base::XdgWmBase,
    xdg_surface: xdg_surface::XdgSurface,
    toplevel: xdg_toplevel::XdgToplevel,
    configured: bool,
}

/// A form that is sent a configure once it has been mapped.
struct State {
    configured: bool,
    shared: &'static Shared,
}

static OPEN: OnceLock<Option<Wayland>> = OnceLock::new();

/// The window if it has already been opened, without opening one.
///
/// The keyboard and the frame pump ask through this: opening a window as a
/// side effect of reading a key would make the first call the expensive one.
pub fn current() -> Option<&'static Wayland> {
    OPEN.get().and_then(Option::as_ref)
}

/// The window, opened on first use: an error when the machine has no
/// compositor (or no `WAYLAND_DISPLAY`), which is an answer rather than a
/// panic.
pub fn open(title: &str, width: u32, height: u32) -> Result<&'static Wayland, String> {
    OPEN.get_or_init(|| match start(title, width, height) {
        Ok(window) => Some(window),
        Err(e) => {
            tracing::warn!("no Wayland window: {e}");
            None
        }
    })
    .as_ref()
    .ok_or_else(|| "no Wayland compositor for a window".to_owned())
}

fn start(title: &str, width: u32, height: u32) -> Result<Wayland, String> {
    let connection = Connection::connect_to_env().map_err(|e| format!("wl_display: {e}"))?;
    let (globals, mut queue) =
        registry_queue_init::<State>(&connection).map_err(|e| format!("wl_registry: {e}"))?;
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals
        .bind(&qh, 4..=4, ())
        .map_err(|e| format!("wl_compositor: {e}"))?;
    let wm_base: xdg_wm_base::XdgWmBase = globals
        .bind(&qh, 1..=2, ())
        .map_err(|e| format!("xdg_wm_base: {e}"))?;
    let seat: Option<wl_seat::WlSeat> = globals.bind(&qh, 4..=5, ()).ok();

    let surface = compositor.create_surface(&qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, &qh, ());
    let toplevel = xdg_surface.get_toplevel(&qh, ());
    toplevel.set_title(title.to_owned());
    toplevel.set_app_id("victory".to_owned());
    // A window smaller than this is not something a title can present in.
    toplevel.set_min_size(1, 1);
    surface.commit();

    let shared: &'static Shared = open_shared();
    if let Some(seat) = seat {
        let seat: wl_seat::WlSeat = seat;
        // The keyboard arrives with the seat's capabilities event, which the
        // first roundtrip below dispatches.
        let _ = &seat;
    }

    let mut state = State {
        configured: false,
        shared,
    };
    // The first configure is what makes the surface mappable, and it names
    // the size the swapchain must match.
    queue
        .roundtrip(&mut state)
        .map_err(|e| format!("first configure: {e}"))?;
    if !state.configured {
        // Some compositors answer only after the surface is re-committed.
        surface.commit();
        queue
            .roundtrip(&mut state)
            .map_err(|e| format!("configure: {e}"))?;
    }
    if !state.configured {
        return Err("the compositor never configured the toplevel".to_owned());
    }
    let configured = *shared.size.lock().unwrap_or_else(|e| e.into_inner());
    if configured.0 == 0 || configured.1 == 0 {
        *shared.size.lock().unwrap_or_else(|e| e.into_inner()) = (width, height);
    }

    // The two handles `vkCreateWaylandSurfaceKHR` takes. The connection owns
    // them for the process's life (the event thread never drops it), which
    // is exactly the lifetime the swapchain needs.
    let display = connection.backend().display_ptr();
    let surface_ptr = surface.id().as_ptr();
    if display.is_null() || surface_ptr.is_null() {
        return Err("no wl_display/wl_surface handle for Vulkan".to_owned());
    }

    let (jobs, incoming) = channel::<Job>();
    let mut window = Window {
        surface,
        wm_base,
        xdg_surface,
        toplevel,
        configured: true,
    };
    std::thread::Builder::new()
        .name("victory-wayland".into())
        .spawn(move || pump(&connection, &mut queue, &mut state, &mut window, incoming))
        .map_err(|e| format!("Wayland event thread: {e}"))?;

    tracing::info!(
        width = configured.0.max(width),
        height = configured.1.max(height),
        "Wayland toplevel mapped"
    );
    Ok(Wayland {
        display: display.cast(),
        surface: surface_ptr.cast(),
        shared,
        jobs,
    })
}

fn open_shared() -> &'static Shared {
    static SHARED: OnceLock<Shared> = OnceLock::new();
    SHARED.get_or_init(Shared::default)
}

/// The event thread: dispatches every event and runs the jobs other threads
/// send, until the connection ends.
fn pump(
    connection: &Connection,
    queue: &mut EventQueue<State>,
    state: &mut State,
    window: &mut Window,
    jobs: std::sync::mpsc::Receiver<Job>,
) {
    loop {
        while let Ok(job) = jobs.try_recv() {
            job(window);
            let _ = connection.flush();
        }
        if let Err(e) = queue.blocking_dispatch(state) {
            tracing::warn!("Wayland connection ended: {e}");
            return;
        }
        for job in jobs.try_iter() {
            job(window);
        }
        let _ = connection.flush();
    }
}
mod dispatch;
