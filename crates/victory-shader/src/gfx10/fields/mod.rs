//! Operand fields of GFX10 instructions, by encoding.
//!
//! Layouts follow AMD's RDNA ISA reference. Register ranges carry their size
//! in dwords, which depends on the opcode (read from its name) and, for lane
//! masks (VCC, EXEC, compare results), on the wave size.

use super::{Encoding, Instruction};

mod decode;
mod names;
mod types;
mod valu;

pub use types::*;
pub(crate) use names::*;
