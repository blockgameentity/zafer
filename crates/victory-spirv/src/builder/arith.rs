//! Arithmetic, comparisons, bit operations and composites: one method per
//! instruction a translated shader emits.

use crate::module::Id;
use crate::ops::Op;

use super::Builder;

/// Declares `method` as an instruction with a result type and n operands.
macro_rules! binary {
    ($($name:ident => $op:ident,)*) => {
        impl Builder {
            $(
                #[doc = concat!("`Op", stringify!($op), "`.")]
                pub fn $name(&mut self, ty: Id, a: Id, b: Id) -> Id {
                    let id = self.module.id();
                    self.module.op(Op::$op, Some(ty), Some(id), &[a.0, b.0]);
                    id
                }
            )*
        }
    };
}

/// Declares `method` as a unary instruction.
macro_rules! unary {
    ($($name:ident => $op:ident,)*) => {
        impl Builder {
            $(
                #[doc = concat!("`Op", stringify!($op), "`.")]
                pub fn $name(&mut self, ty: Id, a: Id) -> Id {
                    let id = self.module.id();
                    self.module.op(Op::$op, Some(ty), Some(id), &[a.0]);
                    id
                }
            )*
        }
    };
}

binary! {
    iadd => IAdd,
    isub => ISub,
    imul => IMul,
    udiv => UDiv,
    sdiv => SDiv,
    umod => UMod,
    srem => SRem,
    smod => SMod,
    fadd => FAdd,
    fsub => FSub,
    fmul => FMul,
    fdiv => FDiv,
    frem => FRem,
    fmod => FMod,
    dot => Dot,
    shift_right_logical => ShiftRightLogical,
    shift_right_arithmetic => ShiftRightArithmetic,
    shift_left => ShiftLeftLogical,
    bitwise_or => BitwiseOr,
    bitwise_xor => BitwiseXor,
    bitwise_and => BitwiseAnd,
    iequal => IEqual,
    inotequal => INotEqual,
    ult => ULessThan,
    slt => SLessThan,
    ule => ULessThanEqual,
    sle => SLessThanEqual,
    ugt => UGreaterThan,
    sgt => SGreaterThan,
    uge => UGreaterThanEqual,
    sge => SGreaterThanEqual,
    ford_equal => FOrdEqual,
    ford_not_equal => FOrdNotEqual,
    ford_less_than => FOrdLessThan,
    ford_less_than_equal => FOrdLessThanEqual,
    ford_greater_than => FOrdGreaterThan,
    ford_greater_than_equal => FOrdGreaterThanEqual,
    funord_equal => FUnordEqual,
    funord_not_equal => FUnordNotEqual,
    funord_less_than => FUnordLessThan,
    funord_less_than_equal => FUnordLessThanEqual,
    funord_greater_than => FUnordGreaterThan,
    funord_greater_than_equal => FUnordGreaterThanEqual,
    logical_equal => LogicalEqual,
    logical_not_equal => LogicalNotEqual,
    logical_or => LogicalOr,
    logical_and => LogicalAnd,
    vector_times_scalar => VectorTimesScalar,
    matrix_times_vector => MatrixTimesVector,
    matrix_times_scalar => MatrixTimesScalar,
    bit_field_insert => BitFieldInsert,
    bit_field_s_extract => BitFieldSExtract,
    bit_field_u_extract => BitFieldUExtract,
}

unary! {
    fnegate => FNegate,
    snegate => SNegate,
    bitwise_not => Not,
    logical_not => LogicalNot,
    any => Any,
    all => All,
    isnan => IsNan,
    isinf => IsInf,
    isfinite => IsFinite,
    isnormal => IsNormal,
    bit_count => BitCount,
    bit_reverse => BitReverse,
    dpdx => DPdx,
    dpdy => DPdy,
    fwidth => Fwidth,
    dpdx_fine => DPdxFine,
    dpdy_fine => DPdyFine,
    fwidth_fine => FwidthFine,
    dpdx_coarse => DPdxCoarse,
    dpdy_coarse => DPdyCoarse,
    fwidth_coarse => FwidthCoarse,
    quantize_to_f16 => QuantizeToF16,
    transpose => Transpose,
}

impl Builder {
    /// `OpSelect`: `condition` picks between `a` and `b`.
    pub fn select(&mut self, ty: Id, condition: Id, a: Id, b: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::Select, Some(ty), Some(id), &[condition.0, a.0, b.0]);
        id
    }

    /// `OpUMulExtended`: the 64-bit unsigned product as a struct of its two
    /// 32-bit halves.
    pub fn umul_extended(&mut self, ty: Id, a: Id, b: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::UMulExtended, Some(ty), Some(id), &[a.0, b.0]);
        id
    }

    /// `OpCompositeConstruct`.
    pub fn composite_construct(&mut self, ty: Id, parts: &[Id]) -> Id {
        let operands: Vec<u32> = parts.iter().map(|p| p.0).collect();
        let id = self.module.id();
        self.module
            .op(Op::CompositeConstruct, Some(ty), Some(id), &operands);
        id
    }

    /// `OpCompositeExtract` of member `index` (a constant id).
    pub fn composite_extract(&mut self, ty: Id, composite: Id, index: u32) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::CompositeExtract, Some(ty), Some(id), &[composite.0, index]);
        id
    }

    /// `OpCompositeInsert`: `object` into `composite` at `index`.
    pub fn composite_insert(&mut self, ty: Id, object: Id, composite: Id, index: u32) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::CompositeInsert,
            Some(ty),
            Some(id),
            &[object.0, composite.0, index],
        );
        id
    }

    /// `OpVectorShuffle`.
    pub fn vector_shuffle(&mut self, ty: Id, a: Id, b: Id, components: &[u32]) -> Id {
        let mut operands = vec![a.0, b.0];
        operands.extend_from_slice(components);
        let id = self.module.id();
        self.module
            .op(Op::VectorShuffle, Some(ty), Some(id), &operands);
        id
    }

    /// `OpVectorExtractDynamic`.
    pub fn vector_extract_dynamic(&mut self, ty: Id, vector: Id, index: Id) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::VectorExtractDynamic,
            Some(ty),
            Some(id),
            &[vector.0, index.0],
        );
        id
    }

    /// `OpVectorInsertDynamic`: `component` into `vector` at `index`.
    pub fn vector_insert_dynamic(&mut self, ty: Id, vector: Id, component: Id, index: Id) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::VectorInsertDynamic,
            Some(ty),
            Some(id),
            &[vector.0, component.0, index.0],
        );
        id
    }

    /// `OpFma`: `a * b + c`, fused to one rounding.
    pub fn fma(&mut self, ty: Id, a: Id, b: Id, c: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::Fma, Some(ty), Some(id), &[a.0, b.0, c.0]);
        id
    }

    /// `OpGroupNonUniformBroadcastFirst`: one lane's value, every lane.
    pub fn group_broadcast_first(&mut self, ty: Id, group: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::GroupNonUniformBroadcastFirst,
            Some(ty),
            Some(id),
            &[group.0, value.0],
        );
        id
    }

    /// `OpConvertUToPtr`: an address as a physical pointer.
    pub fn convert_u_to_ptr(&mut self, ty: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::ConvertUToPtr, Some(ty), Some(id), &[value.0]);
        id
    }

    /// `OpConvertPtrToU`: a physical pointer back as an address.
    pub fn convert_ptr_to_u(&mut self, ty: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::ConvertPtrToU, Some(ty), Some(id), &[value.0]);
        id
    }

    /// `OpBitcast`: the same bits under another type.
    pub fn bitcast(&mut self, ty: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module.op(Op::Bitcast, Some(ty), Some(id), &[value.0]);
        id
    }

    /// `OpConvertFToU`.
    pub fn convert_f_to_u(&mut self, ty: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::ConvertFToU, Some(ty), Some(id), &[value.0]);
        id
    }

    /// `OpConvertFToS`.
    pub fn convert_f_to_s(&mut self, ty: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::ConvertFToS, Some(ty), Some(id), &[value.0]);
        id
    }

    /// `OpConvertSToF`.
    pub fn convert_s_to_f(&mut self, ty: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::ConvertSToF, Some(ty), Some(id), &[value.0]);
        id
    }

    /// `OpConvertUToF`.
    pub fn convert_u_to_f(&mut self, ty: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::ConvertUToF, Some(ty), Some(id), &[value.0]);
        id
    }

    /// `OpUConvert`: an unsigned integer of another width.
    pub fn uconvert(&mut self, ty: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module.op(Op::UConvert, Some(ty), Some(id), &[value.0]);
        id
    }

    /// `OpSConvert`: a signed integer of another width.
    pub fn sconvert(&mut self, ty: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module.op(Op::SConvert, Some(ty), Some(id), &[value.0]);
        id
    }

    /// `OpFConvert`: a float of another width.
    pub fn fconvert(&mut self, ty: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module.op(Op::FConvert, Some(ty), Some(id), &[value.0]);
        id
    }
}
