//! Interpolation: a pixel shader's inputs, as the hardware interpolates them.
//!
//! A console pixel shader interpolates an attribute in two halves: a
//! `v_interp_p1*` produces the per-vertex part and `v_interp_p2*` combines
//! it with the barycentric of the sample being shaded. Vulkan does the whole
//! thing in the rasteriser, so the pair collapses to a load of the input
//! variable: the pipeline's interpolation state chooses the barycentric, and
//! the `p1` half stores nothing that any other instruction reads.
//!
//! The tracer already worked out which attribute and which barycentric each
//! site means (`Io::interps`), because that is a property of the signature.
//! So `p1` is a number the hardware produces only to feed `p2`, and `p2` is
//! the load.

use victory_shader::gfx10::{Fields, Operand};
use victory_shader::io::Interp;
use victory_shader::ir::{Inst, Reg};
use victory_spirv::builder::Builder;

use super::{unsupported, Emitter};
use crate::Error;

impl<'a> Emitter<'a> {
    /// One interpolation instruction.
    pub(crate) fn interp(&mut self, b: &mut Builder, inst: &Inst) -> Result<(), Error> {
        let site = self.site;
        // The first half: nothing reads it except the second half, which is
        // the load, so it stores zero and the value never reaches an output.
        if inst.name().contains("_p1") {
            if let Some(dst) = dst_of(inst) {
                let zero = b.constant_i32(0);
                self.store_i32(b, dst, zero);
            }
            return Ok(());
        }
        let Some(interp) = self.io.interps.iter().find(|(at, _)| *at == site) else {
            return Err(Error::Unsupported {
                offset: inst.machine.offset,
                what: "an interpolation the tracer did not name",
            });
        };
        let attribute = match &interp.1 {
            Interp::Interpolated { attribute, .. } | Interp::Vertex { attribute, .. } => attribute,
        };
        let Some(row) = self.interface.interpolated(attribute.row, attribute.channel) else {
            return Err(Error::Unsupported {
                offset: inst.machine.offset,
                what: "an interpolation of an input the signature lacks",
            });
        };
        let dst = dst_of(inst).ok_or_else(|| unsupported(inst))?;
        // A whole `vec4` input is loaded and the channel picked out of it.
        let ty = row.variable.ty;
        let four = self.types.f32_4;
        let _ = four;
        let vector = b.load(ty, row.variable.id);
        let channel = u32::from(attribute.channel.min(3));
        let f32_ = self.types.f32_;
        let scalar = b.composite_extract(f32_, vector, channel);
        // The register file holds bit patterns, and a half-precision input
        // is a 32-bit float here: the console's f16 pair is the host's f32.
        let bits = b.bitcast(self.types.i32_, scalar);
        self.store_i32(b, dst, bits);
        Ok(())
    }
}

/// The register an interpolation writes.
fn dst_of(inst: &Inst) -> Option<Reg> {
    match &inst.fields {
        Fields::Vintrp { dst, .. } => Some(super::vector::register_of(*dst)),
        Fields::Valu(v) => match v.dst {
            Some(Operand::Vector { reg, .. }) => Some(Reg::V(reg)),
            Some(other) => Some(super::vector::register_of(other)),
            None => None,
        },
        _ => None,
    }
}
