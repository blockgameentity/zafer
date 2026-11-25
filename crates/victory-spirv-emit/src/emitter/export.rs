//! Exports: what the program writes to the pipeline's outputs.
//!
//! A pixel shader's export of `exp mrt<n>` becomes a store into the output
//! variable at location `n`; a vertex shader's position export goes to
//! `gl_Position` and its parameters to the output locations the interface
//! numbered. The store is a store into an `Output` variable, the same in
//! every stage, so the arm only has to find which row the export names.
//!
//! The tracer already resolved the target (`Io::exports`), because which
//! `exp` target means which MRT or parameter is a property of the
//! container's signature, not of the instruction.

use victory_shader::gfx10::Fields;
use victory_shader::ir::Inst;
use victory_spirv::builder::Builder;
use victory_spirv::module::Id;

use super::{unsupported, Emitter};
use crate::Error;

impl<'a> Emitter<'a> {
    /// One export instruction.
    pub(crate) fn export(&mut self, b: &mut Builder, inst: &Inst) -> Result<(), Error> {
        let Fields::Exp { .. } = inst.fields else {
            return Err(unsupported(inst));
        };
        let export = self
            .io
            .exports
            .iter()
            .find(|e| e.site == self.site)
            .cloned()
            .ok_or_else(|| Error::Unsupported {
                offset: inst.machine.offset,
                what: "an export the tracer did not name",
            })?;
        // A `null` or unread target is a discard of the values, not a store:
        // the console writes it where it has nothing to pass on.
        let Some(row) = self.interface.output(&export.target).cloned() else {
            return Ok(());
        };
        let n = u32::try_from(row.components).unwrap_or(1).clamp(1, 4);
        let f32_ = self.types.f32_;
        let mut parts: Vec<Id> = Vec::with_capacity(n as usize);
        for k in 0..n {
            let value = export.values[k as usize];
            let id = match value {
                Some(value) => {
                    let Some(bits) = self.load_i32(b, value) else {
                        // A channel the program never wrote is undefined in
                        // SPIR-V, which is exactly what it is on the console.
                        parts.push(b.undef(f32_));
                        continue;
                    };
                    b.bitcast(f32_, bits)
                }
                None => b.undef(f32_),
            };
            parts.push(id);
        }
        let pointer = row.variable.id;
        let value = if n == 1 {
            let one = parts.pop().unwrap_or_else(|| b.undef(f32_));
            b.store(pointer, one);
            return Ok(());
        } else {
            let vec = match n {
                2 => self.types.f32_2,
                3 => self.types.f32_3,
                _ => self.types.f32_4,
            };
            b.composite_construct(vec, &parts)
        };
        b.store(pointer, value);
        Ok(())
    }
}
