//! Which host GPU backend a run uses, on Windows.
//!
//! The Vulkan presenter needs a `VkSurfaceKHR`, and the only window code
//! this crate has is SDL on Linux ([`crate::linux`]); a Win32 surface
//! (`VK_KHR_win32_surface` on our own `HWND`) is not written yet. So the
//! honest answer on Windows is the D3D12 host, and it says so rather than
//! pretending to try.
//!
//! `VICTORY_BACKEND=vulkan` still fails loudly: silently running D3D12 for
//! someone who asked for Vulkan is the bug this module exists to prevent.

/// A host graphics backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    /// Our own Vulkan device, swapchain and command buffers.
    Vulkan,
    /// The D3D12 device titles create, on host D3D12.
    D3d12,
}

impl Backend {
    /// What `VICTORY_BACKEND` names, if it names anything.
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

    /// The backend this run uses: D3D12 until a Win32 Vulkan surface exists.
    pub fn chosen() -> Backend {
        Self::from_env().unwrap_or(Backend::D3d12)
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
pub fn fallback_allowed() -> bool {
    Backend::from_env() != Some(Backend::Vulkan)
}

/// Resolves the backend for this run.
pub fn start(_width: u32, _height: u32) -> Result<Backend, String> {
    match Backend::chosen() {
        Backend::D3d12 => {
            tracing::info!("backend: d3d12");
            Ok(Backend::D3d12)
        }
        Backend::Vulkan if fallback_allowed() => {
            tracing::warn!("Vulkan has no Windows surface yet; using the d3d12 backend");
            Ok(Backend::D3d12)
        }
        Backend::Vulkan => Err(
            "VICTORY_BACKEND=vulkan: the Vulkan presenter has no Windows surface yet".to_owned(),
        ),
    }
}

/// Nothing to prove on this platform yet.
pub fn present_boot_frame(_backend: Backend) -> Result<(), String> {
    Ok(())
}
