//! Implementations, one module per area of the console API.
//!
//! Adding an API: write an `extern "win64"` function named exactly like
//! the import, then list it with `hle_export!` in the module's `module()`.
//! Title code calls with the Windows x64 convention on every host;
//! `"system"` is only for callbacks we hand to the host's own APIs.
//! Run `victory census` over your titles to see what is still missing.
//!
//! Planned areas (driven by census results, not by completeness):
//! `file` (package paths), `thread` (TLS callbacks for title threads),
//! `xaudio2`, `xstore` stubs.

#[cfg(any(windows, target_os = "linux"))]
pub mod apu;
pub mod audio;
pub mod d3d12x;
pub mod display;
pub mod dsp;
#[cfg(any(windows, target_os = "linux"))]
pub mod dstorage;
pub mod files;
pub mod gameinput;
pub mod kernel;
#[cfg(any(windows, target_os = "linux"))]
pub mod lifecycle;
#[cfg(windows)]
pub mod media;
#[cfg(windows)]
pub mod net;
pub mod speech;
pub mod storage;
pub mod xasync;
pub mod xg;
pub mod xgame;
pub mod xgameruntime;
pub mod xgamesave;
pub mod xgameui;
pub mod xmem;
pub mod xnetworking;
pub mod xpackage;
pub mod xstore;
pub mod xsystem;
pub mod xuser;

use crate::HleModule;

pub(crate) fn all() -> Vec<HleModule> {
    let mut all = vec![
        kernel::module(),
        xgameruntime::module(),
        gameinput::module(),
        d3d12x::module(),
        audio::module(),
        dsp::module(),
        xg::module(),
    ];
    #[cfg(any(windows, target_os = "linux"))]
    all.push(apu::module());
    #[cfg(windows)]
    all.push(media::module());
    #[cfg(any(windows, target_os = "linux"))]
    all.push(dstorage::module());
    all.push(files::module());
    #[cfg(any(windows, target_os = "linux"))]
    all.push(lifecycle::module());
    #[cfg(windows)]
    all.push(net::module());
    all.push(xmem::module());
    #[cfg(any(windows, target_os = "linux"))]
    all.push(xsystem::module());
    all
}
