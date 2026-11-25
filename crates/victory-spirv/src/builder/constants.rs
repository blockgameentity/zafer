//! Constants: one id per distinct value, as a module needs them.

use crate::module::Id;
use crate::ops::Op;

use super::Builder;

impl Builder {
    /// The unsigned 32-bit constant `value`.
    pub fn constant_u32(&mut self, value: u32) -> Id {
        let ty = self.int(32, false);
        self.constant_typed(ty, &[value])
    }

    /// The signed 32-bit constant `value`.
    pub fn constant_i32(&mut self, value: i32) -> Id {
        let ty = self.int(32, true);
        self.constant_typed(ty, &[value as u32])
    }

    /// The unsigned 64-bit constant `value`, split into its two words.
    pub fn constant_u64(&mut self, value: u64) -> Id {
        let ty = self.int(64, false);
        let words = [(value & 0xFFFF_FFFF) as u32, (value >> 32) as u32];
        self.constant_typed(ty, &words)
    }

    /// The signed 64-bit constant `value`.
    pub fn constant_i64(&mut self, value: i64) -> Id {
        let ty = self.int(64, true);
        let bits = value as u64;
        let words = [(bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32];
        self.constant_typed(ty, &words)
    }

    /// The 32-bit float whose bits are `bits` (taken as `value.to_bits()`).
    pub fn constant_f32(&mut self, value: f32) -> Id {
        let ty = self.float(32);
        self.constant_typed(ty, &[value.to_bits()])
    }

    /// The 64-bit float whose bits are `value.to_bits()`.
    pub fn constant_f64(&mut self, value: f64) -> Id {
        let ty = self.float(64);
        let bits = value.to_bits();
        let words = [(bits & 0xFFFF_FFFF) as u32, (bits >> 32) as u32];
        self.constant_typed(ty, &words)
    }

    /// `true` and `false`.
    pub fn constant_bool(&mut self, value: bool) -> Id {
        let ty = self.bool();
        let op = if value {
            Op::ConstantTrue
        } else {
            Op::ConstantFalse
        };
        let id = self.module.id();
        self.module.op(op, Some(ty), Some(id), &[]);
        id
    }

    /// The zero of `ty` (`OpConstantNull`).
    pub fn null_constant(&mut self, ty: Id) -> Id {
        let id = self.module.id();
        self.module.op(Op::ConstantNull, Some(ty), Some(id), &[]);
        id
    }

    /// A composite constant of type `ty` from `parts`.
    pub fn constant_composite(&mut self, ty: Id, parts: &[Id]) -> Id {
        let operands: Vec<u32> = parts.iter().map(|p| p.0).collect();
        let id = self.module.id();
        self.module
            .op(Op::ConstantComposite, Some(ty), Some(id), &operands);
        id
    }

    /// A specialization constant: `value` defaults, `spec` names it.
    pub fn spec_constant_u32(&mut self, value: u32, spec: u32) -> Id {
        let ty = self.int(32, false);
        let id = self.module.id();
        self.module
            .op(Op::SpecConstant, Some(ty), Some(id), &[value]);
        self.decorate(id, crate::enums::Decoration::SpecId, &[spec]);
        id
    }

    /// An `OpConstant` of `ty` whose literal words are `words`. Cached by
    /// `(type, words)`, so asking twice emits one constant.
    fn constant_typed(&mut self, ty: Id, words: &[u32]) -> Id {
        let mut key = vec![ty.0];
        key.extend_from_slice(words);
        if let Some(existing) = self.constant_cache.iter().find(|(_, k)| *k == key) {
            return existing.0;
        }
        let id = self.module.id();
        self.module.op(Op::Constant, Some(ty), Some(id), words);
        self.constant_cache.push((id, key));
        id
    }
}
