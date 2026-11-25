//! Vector instructions: the VALU's arithmetic, comparisons and selects.
//!
//! Every arm reads its operands from their registers and writes its results
//! back, in the IR's own numbering, so the translated module has one
//! instruction per machine instruction and the register file is the state.
//!
//! A VOP3 instruction's operands pass through their modifiers (negate,
//! absolute) before the operation, and its result through the output
//! modifier and clamp after it. The SDWA and DPP forms are refused rather
//! than approximated: a wrong lane permutation or byte selection is a wrong
//! picture with no error anywhere.

use victory_shader::gfx10::{Fields, Operand, Valu};
use victory_shader::ir::{Inst, Reg};
use victory_spirv::builder::Builder;
use victory_spirv::module::Id;

use super::{Emitter, unsupported};
use crate::Error;

impl<'a> Emitter<'a> {
    /// One VALU instruction.
    pub(crate) fn vector(&mut self, b: &mut Builder, inst: &Inst) -> Result<(), Error> {
        let Fields::Valu(v) = inst.fields.clone() else {
            return Err(unsupported(inst));
        };
        if v.dpp.is_some() || v.sdwa.is_some() {
            return Err(Error::Unsupported {
                offset: inst.machine.offset,
                what: "a DPP or SDWA vector operand",
            });
        }
        let dst = match v.dst {
            Some(operand) => register_of(operand),
            None => return Err(unsupported(inst)),
        };
        let name = inst.name();
        // The operands, as bit patterns or floats, as the arm needs them.
        let i32_ = self.types.i32_;
        let f32_ = self.types.f32_;
        let bool_ = self.types.bool_;
        let zero = b.constant_i32(0);
        let one = b.constant_i32(1);

        let result: Id = match name {
            "v_mov_b32" => self.bits_of(b, inst, v.src[0]),
            "v_add_u32" | "v_add_i32" => {
                let a = self.signed_of(b, inst, v.src[0], &v, 0);
                let c = self.signed_of(b, inst, v.src[1], &v, 1);
                b.iadd(i32_, a, c)
            }
            "v_sub_u32" | "v_sub_i32" => {
                let a = self.signed_of(b, inst, v.src[0], &v, 0);
                let c = self.signed_of(b, inst, v.src[1], &v, 1);
                b.isub(i32_, a, c)
            }
            "v_and_b32" => {
                let a = self.bits_of(b, inst, v.src[0]);
                let c = self.bits_of(b, inst, v.src[1]);
                b.bitwise_and(i32_, a, c)
            }
            "v_or_b32" => {
                let a = self.bits_of(b, inst, v.src[0]);
                let c = self.bits_of(b, inst, v.src[1]);
                b.bitwise_or(i32_, a, c)
            }
            "v_xor_b32" => {
                let a = self.bits_of(b, inst, v.src[0]);
                let c = self.bits_of(b, inst, v.src[1]);
                b.bitwise_xor(i32_, a, c)
            }
            "v_not_b32" => {
                let a = self.bits_of(b, inst, v.src[0]);
                b.bitwise_not(i32_, a)
            }
            "v_lshlrev_b32" => {
                let shift = self.bits_of(b, inst, v.src[0]);
                let value = self.bits_of(b, inst, v.src[1]);
                b.shift_left(i32_, value, shift)
            }
            "v_lshrrev_b32" => {
                let shift = self.bits_of(b, inst, v.src[0]);
                let value = self.bits_of(b, inst, v.src[1]);
                b.shift_right_logical(i32_, value, shift)
            }
            "v_ashrrev_i32" => {
                let shift = self.bits_of(b, inst, v.src[0]);
                let value = self.bits_of(b, inst, v.src[1]);
                b.shift_right_arithmetic(i32_, value, shift)
            }
            "v_add_f32" => {
                let a = self.float_of(b, inst, v.src[0], &v, 0)?;
                let c = self.float_of(b, inst, v.src[1], &v, 1)?;
                let sum = b.fadd(f32_, a, c);
                self.output_modifier(b, sum, &v)?
            }
            "v_sub_f32" => {
                let a = self.float_of(b, inst, v.src[0], &v, 0)?;
                let c = self.float_of(b, inst, v.src[1], &v, 1)?;
                let difference = b.fsub(f32_, a, c);
                self.output_modifier(b, difference, &v)?
            }
            "v_mul_f32" => {
                let a = self.float_of(b, inst, v.src[0], &v, 0)?;
                let c = self.float_of(b, inst, v.src[1], &v, 1)?;
                let product = b.fmul(f32_, a, c);
                self.output_modifier(b, product, &v)?
            }
            "v_min_f32" | "v_max_f32" => {
                let a = self.float_of(b, inst, v.src[0], &v, 0)?;
                let c = self.float_of(b, inst, v.src[1], &v, 1)?;
                let condition = if name == "v_min_f32" {
                    b.ford_less_than(bool_, a, c)
                } else {
                    b.ford_greater_than(bool_, a, c)
                };
                let picked = b.select(f32_, condition, a, c);
                self.output_modifier(b, picked, &v)?
            }
            // A compare writes a lane mask. Its value as a dword is the
            // per-lane condition, which is what a lane reading its own bit
            // sees; a mask read as data goes through a ballot.
            _ if is_compare(name) => {
                let condition = self.compare(b, inst, name, &v)?;
                b.select(i32_, condition, one, zero)
            }
            _ => return Err(unsupported(inst)),
        };
        // A register variable holds `i32`, so a float-producing arm's result
        // is bitcast back before the store (`spirv-val`: "pointer's type
        // does not match object's type").
        let result = if Self::produces_float(name) {
            b.bitcast(i32_, result)
        } else {
            result
        };
        self.store_i32(b, dst, result);
        let _ = zero;
        Ok(())
    }

    /// Applies a VOP3 output modifier and clamp to a float result.
    fn output_modifier(&mut self, b: &mut Builder, value: Id, v: &Valu) -> Result<Id, Error> {
        let f32_ = self.types.f32_;
        let mut out = value;
        // 1: ×2, 2: ×4, 3: ÷2.
        let factor = match v.omod {
            0 => None,
            1 => Some(2.0),
            2 => Some(4.0),
            3 => Some(0.5),
            _ => unreachable!("omod is two bits"),
        };
        if let Some(factor) = factor {
            let scale = b.constant_f32(factor);
            out = b.fmul(f32_, out, scale);
        }
        if v.clamp {
            let zero = b.constant_f32(0.0);
            let one = b.constant_f32(1.0);
            let below = b.ford_less_than(self.types.bool_, out, zero);
            let above = b.ford_greater_than(self.types.bool_, out, one);
            out = b.select(f32_, below, zero, out);
            out = b.select(f32_, above, one, out);
        }
        Ok(out)
    }

    /// An operand's `i32` bits, with the VOP3 integer negation if the
    /// instruction negates it.
    fn signed_of(&mut self, b: &mut Builder, inst: &Inst, operand: Operand, v: &Valu, at: usize) -> Id {
        let bits = self.bits_of(b, inst, operand);
        if v.neg >> at & 1 == 0 {
            return bits;
        }
        // No `OpINegate`: zero minus the value is one instruction and exact
        // for two's complement.
        let zero = b.constant_i32(0);
        b.isub(self.types.i32_, zero, bits)
    }

    /// An operand's `i32` bits, with the operand's absolute value if the
    /// instruction takes one.
    pub(crate) fn bits_of(&mut self, b: &mut Builder, inst: &Inst, operand: Operand) -> Id {
        let i32_ = self.types.i32_;
        match operand {
            Operand::Literal(value) => b.constant_i32(value as i32),
            Operand::Const(code) => b.constant_i32(const_value(code)),
            other => {
                let reg = register_of(other);
                let value = inst.read(reg);
                match value.and_then(|v| self.load_i32(b, v)) {
                    Some(id) => id,
                    // A register the instruction reads that the IR did not
                    // record holds nothing: an undefined dword.
                    None => b.undef(i32_),
                }
            }
        }
    }

    /// An operand's `f32`, with its negate and absolute modifiers.
    fn float_of(
        &mut self,
        b: &mut Builder,
        inst: &Inst,
        operand: Operand,
        v: &Valu,
        at: usize,
    ) -> Result<Id, Error> {
        let f32_ = self.types.f32_;
        let bits = self.bits_of(b, inst, operand);
        let mut out = b.bitcast(f32_, bits);
        if v.neg >> at & 1 != 0 {
            out = b.fnegate(f32_, out);
        }
        if v.abs >> at & 1 != 0 {
            // `|x|` is `max(x, -x)`, which is a select on the comparison.
            let negated = b.fnegate(f32_, out);
            let condition = b.ford_greater_than(self.types.bool_, out, negated);
            out = b.select(f32_, condition, out, negated);
        }
        Ok(out)
    }

    /// A compare's per-lane condition.
    fn compare(&mut self, b: &mut Builder, inst: &Inst, name: &str, v: &Valu) -> Result<Id, Error> {
        let bool_ = self.types.bool_;
        let float = name.contains("_f32");
        if float {
            let a = self.float_of(b, inst, v.src[0], &v, 0)?;
            let c = self.float_of(b, inst, v.src[1], &v, 1)?;
            let condition = match name {
                "v_cmp_eq_f32" => b.ford_equal(bool_, a, c),
                "v_cmp_ne_u32" | "v_cmp_ne_i32" => b.ford_not_equal(bool_, a, c),
                "v_cmp_lt_f32" => b.ford_less_than(bool_, a, c),
                "v_cmp_le_f32" => b.ford_less_than_equal(bool_, a, c),
                "v_cmp_gt_f32" => b.ford_greater_than(bool_, a, c),
                "v_cmp_ge_f32" => b.ford_greater_than_equal(bool_, a, c),
                other => return Err(unsupported_name(inst, other)),
            };
            return Ok(condition);
        }
        let a = self.bits_of(b, inst, v.src[0]);
        let c = self.bits_of(b, inst, v.src[1]);
        let condition = match name {
            "v_cmp_eq_i32" | "v_cmp_eq_u32" => b.iequal(bool_, a, c),
            "v_cmp_ne_i32" | "v_cmp_ne_u32" => b.inotequal(bool_, a, c),
            "v_cmp_lt_i32" => b.slt(bool_, a, c),
            "v_cmp_gt_i32" => b.sgt(bool_, a, c),
            "v_cmp_le_i32" => b.sle(bool_, a, c),
            "v_cmp_ge_i32" => b.sge(bool_, a, c),
            "v_cmp_lt_u32" => b.ult(bool_, a, c),
            "v_cmp_gt_u32" => b.ugt(bool_, a, c),
            "v_cmp_le_u32" => b.ule(bool_, a, c),
            "v_cmp_ge_u32" => b.uge(bool_, a, c),
            other => return Err(unsupported_name(inst, other)),
        };
        Ok(condition)
    }
}

/// Whether an instruction name is a compare.
fn is_compare(name: &str) -> bool {
    name.starts_with("v_cmp_")
}

/// The register an operand names.
pub(crate) fn register_of(operand: Operand) -> Reg {
    match operand {
        Operand::Vector { reg, .. } => Reg::V(reg),
        Operand::Scalar { code, .. } => match code {
            106 | 107 => Reg::S(victory_shader::ir::VCC),
            126 | 127 => Reg::S(victory_shader::ir::EXEC),
            124 => Reg::S(victory_shader::ir::M0),
            code => Reg::S(code),
        },
        other => {
            let _ = other;
            Reg::S(125)
        }
    }
}

/// An inline-constant operand's value.
fn const_value(code: u8) -> i32 {
    match code {
        128..=192 => i32::from(code - 128),
        193..=208 => -i32::from(code - 192),
        _ => 0,
    }
}

/// An error naming a compare the emitter does not translate.
fn unsupported_name(inst: &Inst, name: &str) -> Error {
    let _ = name;
    unsupported(inst)
}
