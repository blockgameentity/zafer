//! Exports: what the program writes to the pipeline's outputs.
//!
//! A pixel shader's export of `exp mrt<n>` becomes a store into the output
//! variable at location `n`; a vertex shader's position export goes to
//! `gl_Position` and its parameters to the output locations the interface
//! numbered. The store is a store into a `Output` variable, the same in
//! every stage.

use victory_shader::ir::Inst;
use victory_spirv::builder::Builder;

use super::{Emitter, unsupported};
use crate::Error;

impl<'a> Emitter<'a> {
    /// One export instruction.
    pub(crate) fn export(&mut self, b: &mut Builder, inst: &Inst) -> Result<(), Error> {
        let _ = b;
        Err(unsupported(inst))
    }
}
