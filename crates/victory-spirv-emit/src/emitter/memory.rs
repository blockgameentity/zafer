//! Memory: the console's scalar, global, buffer and image accesses.
//!
//! A console address is a host address (memory is unified), so a global
//! access is a pointer this process owns. Vulkan spells that as
//! `PhysicalStorageBuffer` — a buffer device address — which a descriptor
//! or a push constant supplies, both of which this crate declares in the
//! emitter's interface. The exact lowering is the Vulkan backend's
//! milestone 4 work; until it lands every access here reports
//! [`Error::Unsupported`] rather than guessing an address.

use victory_shader::ir::Inst;
use victory_spirv::builder::Builder;

use super::{Emitter, unsupported};
use crate::Error;

impl<'a> Emitter<'a> {
    /// One memory instruction.
    pub(crate) fn memory(&mut self, b: &mut Builder, inst: &Inst) -> Result<(), Error> {
        let _ = b;
        Err(unsupported(inst))
    }
}
