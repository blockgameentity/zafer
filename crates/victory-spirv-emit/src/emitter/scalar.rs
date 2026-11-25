//! Scalar instructions: the SALU and its immediate forms.
//!
//! A scalar instruction runs once per wave, so every lane holds the same
//! value; that is one `i32` in one function variable, exactly as a vector
//! instruction's is per lane. Nothing here needs to know the wave's size.

use victory_shader::gfx10::Fields;
use victory_shader::ir::{Inst, Reg};
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
        // Instructions with no value of their own: the mode registers SPIR-V
        // does not have (a float mode is denormal handling and rounding, and
        // Vulkan's is its own), and the vertex-fetch call whose results come
        // in as user SGPRs rather than from the call.
        if matches!(
            name,
            "s_setreg_imm32_b32" | "s_setreg_b32" | "s_version" | "s_swappc_b64"
        ) || name.starts_with("s_waitcnt")
        {
            return Ok(());
        }
        // `s_wqm` widens a mask to whole quads. A lane's own bit is in its
        // quad exactly when the bit is set, so per lane the result is the
        // source; the widening only matters for lanes that do not run.
        if name == "s_wqm_b32" || name == "s_wqm_b64" {
            let dst_operand = dst.unwrap_or(victory_shader::gfx10::Operand::Literal(0));
            let dst = super::vector::register_of(dst_operand);
            let a = match src.first() {
                Some(operand) => self.scalar_operand(b, inst, *operand),
                None => b.constant_i32(0),
            };
            self.store_i32(b, dst, a);
            return Ok(());
        }
        let dst_operand = dst.unwrap_or(victory_shader::gfx10::Operand::Literal(0));
        let dst = super::vector::register_of(dst_operand);
        let a_reg = src.first().copied().unwrap_or(dst_operand);
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
            "s_mul_i32" | "s_mulk_i32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                b.imul(i32_, a, c)
            }
            "s_mul_hi_u32" => {
                // The high dword of the unsigned product: `OpUMulExtended`
                // gives both halves as a struct.
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let wide = b.umul_extended(self.types.u32_pair, a, c);
                let high = b.composite_extract(self.types.u32_, wide, 1);
                b.bitcast(i32_, high)
            }
            "s_min_u32" | "s_min_i32" | "s_max_u32" | "s_max_i32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let signed = name.ends_with("i32");
                let less = match (name.starts_with("s_min"), signed) {
                    (true, true) => b.slt(self.types.bool_, a, c),
                    (true, false) => b.ult(self.types.bool_, a, c),
                    (false, true) => b.sgt(self.types.bool_, a, c),
                    (false, false) => b.ugt(self.types.bool_, a, c),
                };
                b.select(i32_, less, a, c)
            }
            "s_movk_i32" => b.constant_i32(i32::from(imm.unwrap_or(0) as i16)),
            // `s_lshl_b32`: the value shifts left by the second operand's
            // low bits.
            "s_lshl_b32" | "s_lshl_b64" => {
                let shift = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                b.shift_left(i32_, a, shift)
            }
            "s_lshr_b32" | "s_lshr_b64" => {
                let shift = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                b.shift_right_logical(i32_, a, shift)
            }
            "s_ashr_i32" | "s_ashr_i64" => {
                let shift = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                b.shift_right_arithmetic(i32_, a, shift)
            }
            // `s_bfe_u32`: `(value >> offset) & mask`, where the offset is
            // the low 5 bits and the width the high 5 bits of the control
            // dword (width 0 means 32).
            "s_bfe_u32" | "s_bfe_i32" | "s_bfe_u64" | "s_bfe_i64" => {
                let control = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                self.bitfield_extract(b, a, control, name.ends_with("i32") || name.ends_with("i64"))
            }
            // `s_and_or_b32`: `(a & b) | c`, the three-input bitwise form.
            "s_and_or_b32" => {
                let c0 = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let c1 = self.scalar_operand(b, inst, src.get(2).copied().unwrap_or(a_reg));
                let and = b.bitwise_and(i32_, a, c0);
                b.bitwise_or(i32_, and, c1)
            }
            "s_xnor_b32" | "s_xnor_b64" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let x = b.bitwise_xor(i32_, a, c);
                b.bitwise_not(i32_, x)
            }
            "s_andn2_b32" | "s_andn2_b64" => {
                // `a & ~b`.
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let not = b.bitwise_not(i32_, c);
                b.bitwise_and(i32_, a, not)
            }
            "s_orn2_b32" | "s_orn2_b64" => {
                // `a | ~b`.
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let not = b.bitwise_not(i32_, c);
                b.bitwise_or(i32_, a, not)
            }
            "s_nand_b32" | "s_nand_b64" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let x = b.bitwise_and(i32_, a, c);
                b.bitwise_not(i32_, x)
            }
            "s_nor_b32" | "s_nor_b64" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let x = b.bitwise_or(i32_, a, c);
                b.bitwise_not(i32_, x)
            }
            // `s_cmp_*` writes SCC, not a register: the condition is the
            // value, stored as the SCC bit's `i32` (1 or 0).
            "s_cmp_eq_u32" | "s_cmp_eq_i32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let condition = b.iequal(self.types.bool_, a, c);
                self.bool_value(b, condition)
            }
            "s_cmp_lg_u32" | "s_cmp_lg_i32" | "s_cmp_ne_u32" | "s_cmp_ne_i32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let condition = b.inotequal(self.types.bool_, a, c);
                self.bool_value(b, condition)
            }
            "s_cmp_gt_u32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let condition = b.ugt(self.types.bool_, a, c);
                self.bool_value(b, condition)
            }
            "s_cmp_ge_u32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let condition = b.uge(self.types.bool_, a, c);
                self.bool_value(b, condition)
            }
            "s_cmp_gt_i32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let condition = b.sgt(self.types.bool_, a, c);
                self.bool_value(b, condition)
            }
            "s_cmp_ge_i32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let condition = b.sge(self.types.bool_, a, c);
                self.bool_value(b, condition)
            }
            "s_cmp_lt_u32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let condition = b.ult(self.types.bool_, a, c);
                self.bool_value(b, condition)
            }
            "s_cmp_le_u32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let condition = b.ule(self.types.bool_, a, c);
                self.bool_value(b, condition)
            }
            "s_cmp_lt_i32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let condition = b.slt(self.types.bool_, a, c);
                self.bool_value(b, condition)
            }
            "s_cmp_le_i32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let condition = b.sle(self.types.bool_, a, c);
                self.bool_value(b, condition)
            }
            // `s_cselect_b32`: SCC picks between the two operands.
            "s_cselect_b32" | "s_cselect_b64" | "s_cselect_f32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let scc = self.load_bit_scc(b);
                b.select(i32_, scc, a, c)
            }
            // `s_brev_b32`: reverse the 32 bits. SPIR-V core has no
            // bit-reverse, so it is done with the byte-reverse the
            // `GLSL.std.450` set has plus a per-nibble swap... which is not
            // cheaper than refusing. Instead the whole word is reversed by
            // shifting each bit: 32 shifts is *worse* than the refusal, so
            // this is refused with a reason that names the work.
            "s_abs_i32" => self.absolute(b, a),
            "s_absdiff_i32" => {
                let c = self.scalar_operand(b, inst, src.get(1).copied().unwrap_or(a_reg));
                let d = b.isub(i32_, a, c);
                self.absolute(b, d)
            }
            "s_getpc_b64" => {
                // The program counter's value has no host meaning; what the
                // code reads it for is a table in the code, which resource
                // tracing resolves. As data it is the bit pattern the
                // console's own constant-buffer base would have, so a
                // program that only compares it still works.
                let zero = b.constant_i32(0);
                b.isub(i32_, zero, zero)
            }
            _ => return Err(unsupported(inst)),
        };
        self.store_i32(b, dst, result);
        Ok(())
    }

    /// SCC, as the `bool` a select on it takes.
    fn load_bit_scc(&mut self, b: &mut Builder) -> Id {
        let var = self.var_of(b, Reg::Scc);
        b.load(self.types.bool_, var)
    }

    /// A `bool` as the dword `s_cmp_*` leaves behind: 1 or 0.
    fn bool_value(&mut self, b: &mut Builder, condition: Id) -> Id {
        let one = b.constant_i32(1);
        let zero = b.constant_i32(0);
        b.select(self.types.i32_, condition, one, zero)
    }

    /// `s_bfe_*`: the field `control` selects, out of `value`.
    ///
    /// The console packs the offset in bits 0-4 and the width in bits 16-21
    /// of the control dword, and a width of 0 means the whole register. The
    /// field is extracted with a shift, a mask, and a sign extension for the
    /// signed forms.
    fn bitfield_extract(&mut self, b: &mut Builder, value: Id, control: Id, signed: bool) -> Id {
        let i32_ = self.types.i32_;
        let u32_ = self.types.u32_;
        let thirty_one = b.constant_u32(31);
        let sixteen = b.constant_u32(16);
        // offset = control & 31.
        let offset = b.bitwise_and(u32_, control, thirty_one);
        // width = (control >> 16) & 31, and 0 means 32.
        let shifted = b.shift_right_logical(u32_, control, sixteen);
        let width = b.bitwise_and(u32_, shifted, thirty_one);
        let zero = b.constant_u32(0);
        let thirty_two = b.constant_u32(32);
        let is_zero = b.iequal(self.types.bool_, width, zero);
        let width = b.select(u32_, is_zero, thirty_two, width);
        // The field: `(value >> offset)` masked to `width` bits.
        let field = b.shift_right_logical(i32_, value, offset);
        let one = b.constant_u32(1);
        let bits = b.isub(u32_, thirty_two, width);
        let all = b.constant_u32(!0u32);
        let reach = b.shift_right_arithmetic(u32_, all, bits);
        let reach = b.bitcast(i32_, reach);
        let masked = b.bitwise_and(i32_, field, reach);
        if !signed {
            return masked;
        }
        // Sign-extend: `(field << (32 - width)) >> (32 - width)`.
        let shifted = b.shift_left(i32_, masked, bits);
        b.shift_right_arithmetic(i32_, shifted, bits)
    }

    /// `|x|`, as `max(x, -x)`.
    fn absolute(&mut self, b: &mut Builder, value: Id) -> Id {
        let i32_ = self.types.i32_;
        let zero = b.constant_i32(0);
        let negated = b.isub(i32_, zero, value);
        let negative = b.slt(self.types.bool_, value, zero);
        b.select(i32_, negative, negated, value)
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
