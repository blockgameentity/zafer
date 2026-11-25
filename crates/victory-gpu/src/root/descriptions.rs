//! The descriptions the title passes, and the console's own flags, which the
//! host does not know.
use super::*;

const CONSOLE_FLAGS: u32 = 0xff00_0000;

#[repr(C)]
#[derive(Clone, Copy)]
pub struct Desc {
    pub(crate) parameter_count: u32,
    pub(crate) parameters: *const c_void,
    pub(crate) sampler_count: u32,
    pub(crate) samplers: *const c_void,
    pub(crate) flags: u32,
}

/// Every version's description starts with the fields of [`Desc`].
#[repr(C)]
#[derive(Clone, Copy)]
pub struct VersionedDesc {
    pub(crate) version: u32,
    pub(crate) desc: Desc,
}

pub(crate) fn without_console_flags(desc: Desc) -> Desc {
    if desc.flags & CONSOLE_FLAGS != 0 {
        tracing::debug!(
            flags = format_args!("{:#x}", desc.flags),
            "console root signature flags dropped"
        );
    }
    Desc {
        flags: desc.flags & !CONSOLE_FLAGS,
        ..desc
    }
}
