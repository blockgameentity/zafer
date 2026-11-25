//! Scalar instructions: the SALU and its immediate forms.
//!
//! A scalar instruction runs once per wave, so every lane holds the same
//! value; that is one `i32` in one function variable, exactly as a vector
//! instruction's is per lane. Nothing here needs to know the wave's size.

use victory_shader::gfx10::Fields;
use victory_shader::ir::Inst;
use victory_spirv::builder::Builder;
use victory_spirv::module::Id;

use super::{Emitter, unsupported};
use crate::Error;

impl<'a> Emitter<'a> {
    /// One scalar instruction.
    pub(crate) fn scalar(&mut self, b: &mut Builder, inst: &Inst) -> Result<(), Error> {
        let Fields::Sop { dst, src, imm } = &inst.fields else {
            return Err(unsupported(inst));
        };
        let _ = imm;
        let name = inst.name();
        let dst = match dst {
            Some(operand) => super::vector::register_of(*operand),
            None => return Err(unsupported(inst)),
        };
        let a = match src.first() {
            Some(operand) => self.scalar_operand(b, inst, *operand),
            None => return Err(unsupported(inst)),
        };
        let i32_ = self.types.i32_;
        let result = match name {
            "s_mov_b32" | "s_mov_b64" => a,
            "s_add_u32" | "s_add_i32" => {
                let c = self.scalar_operand(b, inst, src[1]);
                b.iadd(i32_, a, c)
            }
            "s_sub_u32" | "s_sub_i32" => {
                let c = self.scalar_operand(b, inst, src[1]);
                b.isub(i32_, a, c)
            }
            "s_and_b32" | "s_and_b64" => {
                let c = self.scalar_operand(b, inst, src[1]);
                b.bitwise_and(i32_, a, c)
            }
            "s_or_b32" | "s_or_b64" => {
                let c = self.scalar_operand(b, inst, src[1]);
                b.bitwise_or(i32_, a, c)
            }
            "s_xor_b32" | "s_xor_b64" => {
                let c = self.scalar_operand(b, inst, src[1]);
                b.bitwise_xor(i32_, a, c)
            }
            "s_not_b32" | "s_not_b64" => b.bitwise_not(i32_, a),
            _ => return Err(unsupported(inst)),
        };
        self.store_i32(b, dst, result);
        Ok(())
    }

    /// The `i32` a scalar operand holds.
    fn scalar_operand(
        &mut self,
        b: &mut Builder,
        inst: &Inst,
        operand: victory_shader::gfx10::Operand,
    ) -> Id {
        match operand {
            victory_shader::gfx10::Operand::Literal(value) => b.constant_i32(value as i32),
            victory_shader::gfx10::Operand::Const(code) => {
                let value = match code {
                    128..=192 => i32::from(code - 128),
                    193..=208 => -i32::from(code - 192),
                    _ => 0,
                };
                b.constant_i32(value)
            }
            other => {
                let reg = super::vector::register_of(other);
                let value = inst.read(reg);
                match value.and_then(|v| self.load_i32(b, v)) {
                    Some(id) => id,
                    None => b.constant_i32(0),
                }
            }
        }
    }
}
