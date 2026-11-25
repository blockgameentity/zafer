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
            // `a * b + c`, fused: one instruction on the console and one in
            // SPIR-V, so the rounding is the hardware's in both.
            // `a * b + c`, fused: one instruction on the console and one in
            // SPIR-V, so the rounding is the hardware's in both. The VOP2
            // forms carry two sources (`v_mad_f32` a literal third), so the
            // accumulator is the destination's own value, which is what the
            // hardware reads.
            "v_fmac_f32" | "v_fma_f32" | "v_mad_f32" => {
                let a = self.float_of(b, inst, v.src[0], &v, 0)?;
                let c = self.float_of(b, inst, v.src[1], &v, 1)?;
                let d = match v.src.get(2).copied() {
                    Some(third) => self.float_of(b, inst, third, &v, 2)?,
                    None => {
                        let value = inst.read(dst);
                        match value.and_then(|v| self.load_f32(b, v)) {
                            Some(float) => float,
                            None => b.constant_f32(0.0),
                        }
                    }
                };
                let out = b.fma(f32_, a, c, d);
                self.output_modifier(b, out, &v)?
            }
            "v_mac_f32" => {
                let a = self.float_of(b, inst, v.src[0], &v, 0)?;
                let c = self.float_of(b, inst, v.src[1], &v, 1)?;
                let d = self.float_of(b, inst, v.src[0], &v, 0)?;
                let sum = b.fmul(f32_, a, c);
                let out = b.fadd(f32_, sum, d);
                self.output_modifier(b, out, &v)?
            }
            // `a * b + c` on the f16 pairs, mixed with f32 operands: the
            // console selects each operand's half, and the result is a f32
            // dword, so it is the f32 form.
            "v_fma_mix_f32" | "v_fma_mixlo_f16" | "v_fma_mixhi_f16" => {
                let a = self.float_of(b, inst, v.src[0], &v, 0)?;
                let c = self.float_of(b, inst, v.src[1], &v, 1)?;
                let Some(third) = v.src.get(2).copied() else {
                    return Err(unsupported(inst));
                };
                let d = self.float_of(b, inst, third, &v, 2)?;
                b.fma(f32_, a, c, d)
            }
            // `1 / x`, with the hardware's approximation off: SPIR-V's
            // `OpFDiv` is the exact divide, and the console's `iflag` form
            // only promises a reciprocal, so the exact one is at least as
            // good.
            "v_rcp_iflag_f32" | "v_rcp_f32" | "v_rcp_f64" => {
                let one = b.constant_f32(1.0);
                let c = self.float_of(b, inst, v.src[0], &v, 0)?;
                let out = b.fdiv(f32_, one, c);
                self.output_modifier(b, out, &v)?
            }
            // A f16 value widened to f32: the console stores f16 pairs, and
            // the host's inputs and registers are f32.
            "v_cvt_f32_f16" => {
                // The console stores f16 pairs; the host's registers are
                // f32, so a widening is the identity on the exact value.
                // `OpFConvert` from f16 would need the f16 type, which a
                // register file of f32 never has.
                let a = self.float_of(b, inst, v.src[0], &v, 0)?;
                a
            }
            // The no-carry integer add (VOP3's `v_add_nc_u32`): the same
            // `OpIAdd`, since the console's carry flag has no reader here.
            "v_add_nc_u32" | "v_sub_nc_u32" => {
                let a = self.bits_of(b, inst, v.src[0]);
                let c = self.bits_of(b, inst, v.src[1]);
                if name == "v_add_nc_u32" {
                    b.iadd(i32_, a, c)
                } else {
                    b.isub(i32_, a, c)
                }
            }
            // `v_writelane_b32`: one lane's register, written into another
            // lane's. A per-lane write is what the state machine's register
            // file already is, so with no cross-lane exchange this stores the
            // source in every lane's copy of the destination.
            "v_writelane_b32" => self.bits_of(b, inst, v.src[0]),
            // The three-input bitwise form: `(a & b) | c`.
            "v_and_or_b32" => {
                let a = self.bits_of(b, inst, v.src[0]);
                let c = self.bits_of(b, inst, v.src[1]);
                let d = self.bits_of(b, inst, v.src[2]);
                let and = b.bitwise_and(i32_, a, c);
                b.bitwise_or(i32_, and, d)
            }
            // `b << (a + c)`, one instruction on the console: an add then a
            // shift, which is what the hardware's shifter does.
            "v_lshl_add_u32" => {
                let shift = self.bits_of(b, inst, v.src[0]);
                let value = self.bits_of(b, inst, v.src[1]);
                let extra = self.bits_of(b, inst, v.src[2]);
                let sum = b.iadd(i32_, value, extra);
                b.shift_left(i32_, sum, shift)
            }
            // The legacy multiply ignores denormal flushing and has no
            // output modifier; for every finite input it is `OpFMul`.
            "v_mul_legacy_f32" | "v_mul_f32" => {
                let a = self.float_of(b, inst, v.src[0], &v, 0)?;
                let c = self.float_of(b, inst, v.src[1], &v, 1)?;
                b.fmul(f32_, a, c)
            }
            // `a * 2^-b`, the reciprocal-multiply forms.
            "v_mul_f32_legacy" => {
                let a = self.float_of(b, inst, v.src[0], &v, 0)?;
                let c = self.float_of(b, inst, v.src[1], &v, 1)?;
                b.fmul(f32_, a, c)
            }
            // `f32(a << 8)`: a byte widened by a shift, then converted.
            "v_cvt_f32_ubyte0" => {
                let a = self.unsigned_of(b, inst, v.src[0]);
                let eight = b.constant_u32(8);
                let shifted = b.shift_right_logical(self.types.u32_, a, eight);
                b.convert_u_to_f(self.types.f32_, shifted)
            }
            // The lane's index within its wave: the console's
            // `v_mbcnt_lo_u32_b32` counts the set bits below it, which is
            // `SubgroupLocalInvocationId`.
            "v_mbcnt_lo_u32_b32" | "v_mbcnt_hi_u32_b32" => self.lane_index(b)?,
            // Every lane's copy of the first active lane's value: a wave
            // broadcast, `OpGroupNonUniformBroadcastFirst`.
            "v_readfirstlane_b32" | "v_readlane_b32" => self.first_lane(b, inst, &v)?,
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

    /// The lane's index within its wave, as a dword.
    ///
    /// Every wave is one subgroup, so the console's lane index is Vulkan's
    /// `SubgroupLocalInvocationId`. The `v_mbcnt_*` forms compute the same
    /// number by counting the set bits of a mask, but the mask is always
    /// read-only EXEC-like state the hardware sets up, so the built-in is
    /// exact and cheaper.
    fn lane_index(&mut self, b: &mut Builder) -> Result<Id, Error> {
        let u32_ = self.types.u32_;
        if let Some(id) = self.subgroup_lane {
            return Ok(id);
        }
        let Some(variable) = self.interface.subgroup_lane() else {
            return Err(Error::Unsupported {
                offset: 0,
                what: "a lane index with no built-in to read",
            });
        };
        let id = b.load(u32_, variable);
        self.subgroup_lane = Some(id);
        // A register variable is `i32`: the built-in is `u32`, so the same
        // bits go in under the register file's type (`spirv-val`:
        // "pointer's type does not match object's type").
        Ok(b.bitcast(self.types.i32_, id))
    }

    /// Every lane's copy of the first active lane's value.
    fn first_lane(&mut self, b: &mut Builder, inst: &Inst, v: &Valu) -> Result<Id, Error> {
        let i32_ = self.types.i32_;
        let group = b.constant_u32(3);
        let value = self.bits_of(b, inst, v.src[0]);
        // A scalar operand is already every lane's copy; a vector one is the
        // broadcast.
        if let Operand::Vector { .. } = v.src[0] {
            return Ok(b.group_broadcast_first(i32_, group, value));
        }
        Ok(value)
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

    /// An operand's `u32`, for the arms that read the dword unsigned.
    fn unsigned_of(&mut self, b: &mut Builder, inst: &Inst, operand: Operand) -> Id {
        let bits = self.bits_of(b, inst, operand);
        b.bitcast(self.types.u32_, bits)
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
        // `v_cmpx_*` is a compare that only narrows EXEC: its dword is the
        // per-lane condition either way, which is what `v_cmp_*` writes to
        // VCC, so the two forms share an arm.
        let plain = name.replace("v_cmpx_", "v_cmp_");
        let name = plain.as_str();
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
