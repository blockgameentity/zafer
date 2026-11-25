//! Transcendentals, rounding and the three-input FMA family, which SPIR-V
//! core has no opcode for and `GLSL.std.450` provides.
//!
//! Each of these is one machine instruction standing for one extended
//! instruction, so the mapping is direct once the operands are floats:
//! `v_sqrt_f32` is `Sqrt`, `v_fma_f32` is `Fma`, `v_min3_f32` is two
//! `FMin`. What is *not* direct is the `f16` forms — GFX10 packs two halves
//! in a dword, and a translated dword holds one f32 — so those are
//! converted rather than approximated.

use victory_shader::gfx10::Operand;
use victory_shader::ir::Inst;
use victory_spirv::builder::Builder;
use victory_spirv::module::Id;

use super::vector::register_of;
use super::{Emitter, unsupported};
use crate::Error;

/// The `GLSL.std.450` instruction numbers this emitter uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Glsl {
    Round = 1,
    RoundEven = 2,
    Trunc = 3,
    Floor = 4,
    Ceil = 8,
    Fract = 10,
    Radians = 11,
    Degrees = 12,
    Sin = 13,
    Cos = 14,
    Tan = 15,
    Asin = 16,
    Acos = 17,
    Atan = 18,
    Sinh = 19,
    Cosh = 20,
    Tanh = 21,
    Exp = 27,
    Log = 28,
    Exp2 = 29,
    Log2 = 30,
    Sqrt = 31,
    InverseSqrt = 32,
    Fma = 50,
    Pow = 26,
    Atan2 = 25,
}

impl<'a> Emitter<'a> {
    /// One extended instruction of `ty` over `args`.
    pub(crate) fn glsl(
        &mut self,
        b: &mut Builder,
        ty: Id,
        which: Glsl,
        args: &[Id],
    ) -> Id {
        b.ext_inst(ty, self.glsl_set, which as u32, args)
    }

    /// Whether `name` is one of the families here, so the caller knows to
    /// try this before reporting a gap.
    pub(crate) fn is_trans(&self, name: &str) -> bool {
        matches!(
            name,
            "v_sqrt_f32"
                | "v_rsq_f32"
                | "v_rcp_f32"
                | "v_sin_f32"
                | "v_cos_f32"
                | "v_exp_f32"
                | "v_log_f32"
                | "v_floor_f32"
                | "v_ceil_f32"
                | "v_trunc_f32"
                | "v_rndne_f32"
                | "v_fract_f32"
                | "v_fma_f32"
                | "v_mad_f32"
                | "v_min3_f32"
                | "v_max3_f32"
                | "v_med3_f32"
                | "v_min3_i32"
                | "v_max3_i32"
                | "v_min3_u32"
                | "v_max3_u32"
                | "v_med3_i32"
                | "v_med3_u32"
                | "v_min_i32"
                | "v_max_i32"
                | "v_min_u32"
                | "v_max_u32"
                | "v_cvt_f32_i32"
                | "v_cvt_f32_u32"
                | "v_cvt_i32_f32"
                | "v_cvt_u32_f32"
                | "v_cvt_flr_i32_f32"
                | "v_cvt_rpi_i32_f32"
                | "v_cndmask_b32"
        )
    }

    /// The float an operand is, without modifiers (these forms take none).
    fn plain_f32(&mut self, b: &mut Builder, inst: &Inst, operand: Operand) -> Id {
        let f32_ = self.types.f32_;
        let bits = self.bits_of(b, inst, operand);
        b.bitcast(f32_, bits)
    }

    /// The dword bits of an operand, unmodified.
    fn plain_bits(&mut self, b: &mut Builder, inst: &Inst, operand: Operand) -> Id {
        self.bits_of(b, inst, operand)
    }

    /// One instruction of the families above.
    pub(crate) fn trans(&mut self, b: &mut Builder, inst: &Inst) -> Result<(), Error> {
        let victory_shader::gfx10::Fields::Valu(v) = inst.fields.clone() else {
            return Err(unsupported(inst));
        };
        let name = inst.name();
        let f32_ = self.types.f32_;
        let i32_ = self.types.i32_;
        let u32_ = self.types.u32_;
        let bool_ = self.types.bool_;
        let dst = match v.dst {
            Some(operand) => register_of(operand),
            None => return Err(unsupported(inst)),
        };
        // `v_cndmask_b32` is a select on SCC, which is the one form here
        // that reads a boolean rather than a dword.
        if name == "v_cndmask_b32" {
            let bits = self.plain_bits(b, inst, v.src[2]);
            let zero = b.constant_i32(0);
            let condition = b.inotequal(self.types.bool_, bits, zero);
            let a = self.plain_bits(b, inst, v.src[0]);
            let c = self.plain_bits(b, inst, v.src[1]);
            let picked = b.select(i32_, condition, a, c);
            self.store_i32(b, dst, picked);
            return Ok(());
        }
        // An int->float conversion reads its source as an integer and is
        // exact for every 32-bit value (SPIR-V's convert is defined for
        // the whole range, so no rounding fix-up is needed).
        let result: Id = match name {
            "v_cvt_f32_i32" => {
                let bits = self.plain_bits(b, inst, v.src[0]);
                b.convert_s_to_f(f32_, bits)
            }
            "v_cvt_f32_u32" => {
                let bits = self.plain_bits(b, inst, v.src[0]);
                b.convert_u_to_f(f32_, bits)
            }
            "v_cvt_i32_f32" => {
                let value = self.plain_f32(b, inst, v.src[0]);
                b.convert_f_to_s(i32_, value)
            }
            "v_cvt_u32_f32" => {
                let value = self.plain_f32(b, inst, v.src[0]);
                // The result type of `OpConvertFToU` must be unsigned;
                // passing the signed type is what `spirv-val` reports as
                // "expected unsigned int scalar or vector type". This is a
                // 32-bit unsigned convert, so the bits match either way.
                b.convert_f_to_u(u32_, value)
            }
            // Rounds toward -inf / to nearest before converting, which is
            // what these two mnemonics mean and what a plain convert does
            // not do.
            "v_cvt_flr_i32_f32" => {
                let value = self.plain_f32(b, inst, v.src[0]);
                let floored = b.ext_inst(f32_, self.glsl_set, Glsl::Floor as u32, &[value]);
                b.convert_f_to_s(i32_, floored)
            }
            "v_cvt_rpi_i32_f32" => {
                let value = self.plain_f32(b, inst, v.src[0]);
                let rounded = b.ext_inst(f32_, self.glsl_set, Glsl::RoundEven as u32, &[value]);
                b.convert_f_to_s(i32_, rounded)
            }
            "v_sqrt_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                self.glsl(b, f32_, Glsl::Sqrt, &[a])
            }
            "v_rsq_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                self.glsl(b, f32_, Glsl::InverseSqrt, &[a])
            }
            // `1/x` is a division; `v_rcp_iflag_f32` differs only in the
            // flush it does for denormals, which SPIR-V leaves to the
            // implementation.
            "v_rcp_f32" | "v_rcp_iflag_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                let one = b.constant_f32(1.0);
                b.fdiv(f32_, one, a)
            }
            "v_sin_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                self.glsl(b, f32_, Glsl::Sin, &[a])
            }
            "v_cos_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                self.glsl(b, f32_, Glsl::Cos, &[a])
            }
            "v_exp_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                self.glsl(b, f32_, Glsl::Exp2, &[a])
            }
            "v_log_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                self.glsl(b, f32_, Glsl::Log2, &[a])
            }
            "v_floor_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                self.glsl(b, f32_, Glsl::Floor, &[a])
            }
            "v_ceil_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                self.glsl(b, f32_, Glsl::Ceil, &[a])
            }
            "v_trunc_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                self.glsl(b, f32_, Glsl::Trunc, &[a])
            }
            "v_rndne_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                self.glsl(b, f32_, Glsl::RoundEven, &[a])
            }
            "v_fract_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                self.glsl(b, f32_, Glsl::Fract, &[a])
            }
            "v_fma_f32" | "v_mad_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                let c = self.plain_f32(b, inst, v.src[1]);
                let d = self.plain_f32(b, inst, v.src[2]);
                self.glsl(b, f32_, Glsl::Fma, &[a, c, d])
            }
            "v_min3_f32" | "v_max3_f32" | "v_med3_f32" => {
                let a = self.plain_f32(b, inst, v.src[0]);
                let c = self.plain_f32(b, inst, v.src[1]);
                let d = self.plain_f32(b, inst, v.src[2]);
                self.three(b, name, a, c, d, true)
            }
            "v_min3_i32" | "v_max3_i32" | "v_med3_i32" | "v_min3_u32" | "v_max3_u32"
            | "v_med3_u32" => {
                let a = self.plain_bits(b, inst, v.src[0]);
                let c = self.plain_bits(b, inst, v.src[1]);
                let d = self.plain_bits(b, inst, v.src[2]);
                self.three(b, name, a, c, d, false)
            }
            "v_min_i32" | "v_max_i32" | "v_min_u32" | "v_max_u32" => {
                let a = self.plain_bits(b, inst, v.src[0]);
                let c = self.plain_bits(b, inst, v.src[1]);
                let signed = name.ends_with("i32");
                let less = match (name.starts_with("v_min"), signed) {
                    (true, true) => b.slt(bool_, a, c),
                    (true, false) => b.ult(bool_, a, c),
                    (false, true) => b.sgt(bool_, a, c),
                    (false, false) => b.ugt(bool_, a, c),
                };
                b.select(i32_, less, a, c)
            }
            other => {
                return Err(Error::Unsupported {
                    offset: inst.machine.offset,
                    what: other,
                });
            }
        };
        // A register variable holds a dword, so a float-producing
        // instruction's result is bitcast back to `i32` before the store:
        // storing `float` into an `int` variable is what `spirv-val`
        // reports as "pointer's type does not match object's type".
        let result = if Self::produces_float(name) {
            b.bitcast(i32_, result)
        } else {
            result
        };
        self.store_i32(b, dst, result);
        Ok(())
    }

    /// Whether `name`'s result is a float.
    ///
    /// The register file is dwords, and an instruction's result type must
    /// match the variable it lands in, so every arm that computes in
    /// `float` has to be known here. The conversions *out of* float and the
    /// half/pack forms are the exceptions: they produce integers.
    pub(super) fn produces_float(name: &str) -> bool {
        // A comparison's result is a lane mask, whatever its sources are.
        if name.starts_with("v_cmp") {
            return false;
        }
        // `v_cvt_<result>_<source>` names its result first (`v_cvt_f32_u32`
        // is a float); every other vector instruction names its result type
        // last (`v_add_f32`). A conversion's modifier (`v_cvt_flr_i32_f32`)
        // sits where the result type would, and is not `f32`, which is the
        // answer for those too.
        let tokens: Vec<&str> = name.split('_').collect();
        let result = if tokens.len() > 2 && tokens.get(1) == Some(&"cvt") {
            tokens[2]
        } else {
            tokens.last().copied().unwrap_or_default()
        };
        result == "f32"
    }

    /// `min3`/`max3`/`med3` of `a`, `c`, `d`: two two-input picks, since
    /// the extended set's three-input forms are not in the subset every
    /// driver validates. `med3` is the median, which is `max(min(a, c),
    /// min(max(a, c), d))`.
    fn three(
        &mut self,
        b: &mut Builder,
        name: &str,
        a: Id,
        c: Id,
        d: Id,
        float: bool,
    ) -> Id {
        let bool_ = self.types.bool_;
        let ty = if float { self.types.f32_ } else { self.types.i32_ };
        let signed = !name.ends_with("u32");
        let less = |b: &mut Builder, x: Id, y: Id| -> Id {
            match (float, signed) {
                (true, _) => b.ford_less_than(bool_, x, y),
                (false, true) => b.slt(bool_, x, y),
                (false, false) => b.ult(bool_, x, y),
            }
        };
        let pick = |b: &mut Builder, min: bool, x: Id, y: Id| -> Id {
            let condition = less(b, x, y);
            b.select(ty, condition, if min { x } else { y }, if min { y } else { x })
        };
        if name.contains("med3") {
            // median(a, c, d) = max(min(a, c), min(max(a, c), d))
            let low = pick(b, true, a, c);
            let high = pick(b, false, a, c);
            let middle = pick(b, true, high, d);
            return pick(b, false, low, middle);
        }
        let min = name.contains("min3");
        let first = pick(b, min, a, c);
        pick(b, min, first, d)
    }
}
