//! Which host GPU backend a run uses, and the Vulkan one.
//!
//! Vulkan is the default. The D3D12 host (`host.rs`, on vkd3d-proton on
//! Linux) stays as the comparison target and as the fallback for a machine
//! whose Vulkan is unusable; `VICTORY_BACKEND=d3d12` forces it and
//! `VICTORY_BACKEND=vulkan` refuses the fallback, so a bug in the Vulkan
//! path is never hidden by a silent downgrade.
//!
//! What is wired here is the presentation path of
//! `docs/vulkan-backend.md`'s milestones 1-2: a real device, a real
//! swapchain on the SDL window, and frames we record ourselves. The
//! console's draws still go through D3D12 until milestones 4-7 land; this
//! module is what they will land on.

use std::sync::OnceLock;

use victory_vulkan::Presenter;
use victory_vulkan::device;

/// A host graphics backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Our own Vulkan device, swapchain and command buffers.
    Vulkan,
    /// The D3D12 device titles create, on host D3D12 (vkd3d-proton on
    /// Linux).
    D3d12,
}

impl Backend {
    /// What `VICTORY_BACKEND` names, if it names anything.
    ///
    /// A value we do not know is reported and ignored rather than guessed
    /// at: picking the wrong one by accident is a much worse hour than a
    /// warning.
    pub fn from_env() -> Option<Backend> {
        let value = std::env::var("VICTORY_BACKEND").ok()?;
        match value.trim().to_ascii_lowercase().as_str() {
            "" => None,
            "vulkan" | "vk" => Some(Backend::Vulkan),
            "d3d12" | "dx12" => Some(Backend::D3d12),
            other => {
                tracing::warn!(
                    value = other,
                    "VICTORY_BACKEND is not a backend (vulkan, d3d12); ignored"
                );
                None
            }
        }
    }

    /// The backend this run uses: Vulkan unless asked for D3D12.
    pub fn chosen() -> Backend {
        Self::from_env().unwrap_or(Backend::Vulkan)
    }

    /// The name a log line should use.
    pub fn name(self) -> &'static str {
        match self {
            Backend::Vulkan => "vulkan",
            Backend::D3d12 => "d3d12",
        }
    }
}

/// Why the run is not on Vulkan, when it is not.
///
/// `VICTORY_BACKEND=vulkan` asked for it explicitly, so a fallback would
/// hide the failure the caller is trying to see.
pub fn fallback_allowed() -> bool {
    Backend::from_env() != Some(Backend::Vulkan)
}

/// The Vulkan device and the window's swapchain, opened once per process.
///
/// Opening the device and a surface is the expensive part and both are tied
/// to the window's lifetime, so the presenter lives here for the run rather
/// than being rebuilt per frame.
pub struct Vulkan {
    presenter: std::sync::Mutex<Presenter>,
}

/// The presenter, kept for the run. A `Mutex` rather than a bare value
/// because the queue is used from whichever thread presents a frame, and a
/// `VkQueue` is not shared by submission.
static VULKAN: OnceLock<Option<Vulkan>> = OnceLock::new();

/// The Vulkan presentation path, or why this machine has none.
///
/// `Ok(None)` means there is nothing to try (no loader, no adapter, no
/// display, no SDL): the machine cannot run the backend, which is an answer
/// rather than an error, and the caller falls back to D3D12.
pub fn vulkan(width: u32, height: u32) -> Result<Option<&'static Vulkan>, String> {
    let opened = VULKAN.get_or_init(|| match open(width, height) {
        Ok(vulkan) => Some(vulkan),
        Err(e) => {
            tracing::warn!("Vulkan backend unavailable: {e}");
            None
        }
    });
    Ok(opened.as_ref())
}

/// Opens the device and the window's swapchain.
fn open(width: u32, height: u32) -> Result<Vulkan, String> {
    // The window has to exist first: SDL answers with the instance
    // extensions the surface needs, and those must be enabled when the
    // instance is created.
    let (width, height) = match crate::linux::window(width, height) {
        Ok(_) => crate::linux::window_pixels().unwrap_or((width.max(1), height.max(1))),
        Err(e) => return Err(e),
    };
    let names = crate::linux::vulkan_instance_extensions();
    if names.is_empty() {
        return Err("SDL named no Vulkan instance extensions".to_owned());
    }
    let owned: Vec<std::ffi::CString> = names
        .iter()
        .filter_map(|n| std::ffi::CString::new(n.as_bytes()).ok())
        .collect();
    let required: Vec<&std::ffi::CStr> = owned.iter().map(|n| n.as_c_str()).collect();
    let Some(device) = device::create(c"victory", &required)? else {
        return Err("no Vulkan adapter can run the backend".to_owned());
    };
    tracing::info!(adapter = %device.name(), "Vulkan device created");
    let instance = device.instance_handle();
    let surface = crate::linux::vulkan_surface(instance)
        .ok_or_else(|| "SDL made no Vulkan surface for the window".to_owned())?;
    let presenter = Presenter::open(device, surface, width, height)?;
    Ok(Vulkan {
        presenter: std::sync::Mutex::new(presenter),
    })
}

impl Vulkan {
    /// The pixel format the swapchain uses, for a log line.
    ///
    /// The `ash` type stays inside `victory-vulkan`: the caller only ever
    /// logs this.
    pub fn format(&self) -> String {
        let presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        format!("{:?}", presenter.format().format)
    }

    /// Presents `frames` clears of `color`, rebuilding the swapchain when
    /// the window changes size. Returns the number actually presented.
    pub fn show(&self, frames: u32, color: [f32; 4]) -> Result<u32, String> {
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        let mut shown = 0;
        for _ in 0..frames {
            if let Some((width, height)) = crate::linux::window_pixels() {
                presenter.resize(width, height)?;
            }
            presenter.frame(color)?;
            shown += 1;
        }
        Ok(shown)
    }
}

/// The clear colour the Vulkan path presents while the console's draws are
/// not on it yet: the same one the D3D12 host clears to.
pub const CLEAR: [f32; 4] = [0.0, 0.0, 0.0, 1.0];

/// Resolves the backend for this run and opens its presentation path.
///
/// Vulkan is the default, so a boot with no `VICTORY_BACKEND` set tries
/// Vulkan first. It is only downgraded to D3D12 when the machine cannot run
/// it *and* nobody asked for Vulkan by name: `VICTORY_BACKEND=vulkan` turns
/// the downgrade into an error, because a silent fallback is exactly what
/// hides the bug someone set the variable to look at.
pub fn start(width: u32, height: u32) -> Result<Backend, String> {
    match Backend::chosen() {
        Backend::D3d12 => {
            tracing::info!("backend: d3d12 (VICTORY_BACKEND)");
            Ok(Backend::D3d12)
        }
        Backend::Vulkan => match vulkan(width, height) {
            Ok(Some(_)) => {
                tracing::info!("backend: vulkan");
                Ok(Backend::Vulkan)
            }
            Ok(None) => fallback("Vulkan is installed but this machine cannot present on it"),
            Err(e) => fallback(&e),
        },
    }
}

/// The downgrade to D3D12, or the error when Vulkan was asked for.
fn fallback(why: &str) -> Result<Backend, String> {
    if fallback_allowed() {
        tracing::warn!("{why}; using the d3d12 backend (VICTORY_BACKEND=vulkan refuses this)");
        Ok(Backend::D3d12)
    } else {
        Err(format!("{why}, and VICTORY_BACKEND=vulkan forbids falling back"))
    }
}

/// Presents one cleared frame on `backend`, when it is Vulkan.
///
/// Called at boot so the window and the swapchain are proven before the
/// title's own frames arrive: a black frame on screen is the cheapest
/// possible proof that milestones 1 and 2 of `docs/vulkan-backend.md` work
/// on this machine.
pub fn present_boot_frame(backend: Backend) -> Result<(), String> {
    if backend != Backend::Vulkan {
        return Ok(());
    }
    let Some(Some(vulkan)) = VULKAN.get() else {
        return Ok(());
    };
    tracing::info!(format = %vulkan.format(), "presenting the first Vulkan frame");
    vulkan.show(1, CLEAR)?;
    Ok(())
}
