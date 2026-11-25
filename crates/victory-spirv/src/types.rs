//! The types and constants a module uses, built once each.

use crate::module::{Id, Module};
use crate::enums::{Dim, StorageClass};
use crate::ops::Op;

/// The types and constants a module uses, built once each.
#[derive(Default)]
pub struct Types {
    pub void: Option<Id>,
    pub bool_: Option<Id>,
    /// By bit width: 8, 16, 32, 64.
    pub ints: Vec<(u32, Id)>,
    pub floats: Vec<(u32, Id)>,
    pub vectors: Vec<(Id, u32, Id)>,
    pub pointers: Vec<(u32, Id, Id)>,
    pub arrays: Vec<(Id, u32, Id)>,
    pub structs: Vec<(Id, Vec<u32>)>,
    pub functions: Vec<(Id, Vec<Id>, Id)>,
    pub images: Vec<(Id, Id, Dim, bool)>,
    pub sampled_images: Vec<(Id, Id)>,
    pub samplers: Option<Id>,
}

impl Types {
    /// `%void`.
    pub fn void(&mut self, m: &mut Module) -> Id {
        if let Some(t) = self.void {
            return t;
        }
        let t = m.id();
        m.op(Op::TypeVoid, None, Some(t), &[]);
        self.void = Some(t);
        t
    }

    /// `%bool`.
    pub fn bool_(&mut self, m: &mut Module) -> Id {
        if let Some(t) = self.bool_ {
            return t;
        }
        let t = m.id();
        m.op(Op::TypeBool, None, Some(t), &[]);
        self.bool_ = Some(t);
        t
    }

    /// An integer type of `bits`, signedness `0` (unsigned) or `1`.
    pub fn int(&mut self, m: &mut Module, bits: u32) -> Id {
        if let Some((_, t)) = self.ints.iter().find(|(b, _)| *b == bits) {
            return *t;
        }
        let t = m.id();
        m.op(Op::TypeInt, None, Some(t), &[bits, 0]);
        self.ints.push((bits, t));
        t
    }

    /// A float type of `bits`.
    pub fn float(&mut self, m: &mut Module, bits: u32) -> Id {
        if let Some((_, t)) = self.floats.iter().find(|(b, _)| *b == bits) {
            return *t;
        }
        let t = m.id();
        m.op(Op::TypeFloat, None, Some(t), &[bits]);
        self.floats.push((bits, t));
        t
    }

    /// `vecN` of `scalar`.
    pub fn vector(&mut self, m: &mut Module, scalar: Id, n: u32) -> Id {
        if let Some((_, _, t)) = self.vectors.iter().find(|(s, c, _)| *s == scalar && *c == n) {
            return *t;
        }
        let t = m.id();
        m.op(Op::TypeVector, None, Some(t), &[scalar.0, n]);
        self.vectors.push((scalar, n, t));
        t
    }

    /// A pointer to `to` in `space`.
    pub fn pointer(&mut self, m: &mut Module, to: Id, space: StorageClass) -> Id {
        let space = space as u32;
        if let Some((_, _, t)) = self
            .pointers
            .iter()
            .find(|(s, p, _)| *s == space && *p == to)
        {
            return *t;
        }
        let t = m.id();
        m.op(Op::TypePointer, None, Some(t), &[space, to.0]);
        self.pointers.push((space, to, t));
        t
    }

    /// An array of `n` `of` (a runtime array when `n` is zero).
    pub fn array(&mut self, m: &mut Module, of: Id, n: u32) -> Id {
        if let Some((_, _, t)) = self.arrays.iter().find(|(o, c, _)| *o == of && *c == n) {
            return *t;
        }
        let t = m.id();
        if n == 0 {
            m.op(Op::TypeRuntimeArray, None, Some(t), &[of.0]);
        } else {
            let len = self.u32_const(m, n);
            m.op(Op::TypeArray, None, Some(t), &[of.0, len.0]);
        }
        self.arrays.push((of, n, t));
        t
    }

    /// A struct of `fields`.
    pub fn struct_(&mut self, m: &mut Module, fields: &[Id]) -> Id {
        let ids: Vec<u32> = fields.iter().map(|f| f.0).collect();
        if let Some((t, _)) = self.structs.iter().find(|(_, f)| *f == ids) {
            return *t;
        }
        let t = m.id();
        m.op(Op::TypeStruct, None, Some(t), &ids);
        self.structs.push((t, ids));
        t
    }

    /// A function type: `ret` and parameter types.
    pub fn function(&mut self, m: &mut Module, ret: Id, params: &[Id]) -> Id {
        if let Some((_, _, t)) = self
            .functions
            .iter()
            .find(|(r, p, _)| *r == ret && p.as_slice() == params)
        {
            return *t;
        }
        let mut operands = vec![ret.0];
        operands.extend(params.iter().map(|p| p.0));
        let t = m.id();
        m.op(Op::TypeFunction, None, Some(t), &operands);
        self.functions.push((ret, params.to_vec(), t));
        t
    }

    /// `u32`'s type.
    pub fn u32(&mut self, m: &mut Module) -> Id {
        self.int(m, 32)
    }

    /// `f32`'s type.
    pub fn f32(&mut self, m: &mut Module) -> Id {
        self.float(m, 32)
    }

    /// The `u32` constant `v`.
    pub fn u32_const(&mut self, m: &mut Module, v: u32) -> Id {
        let ty = self.u32(m);
        let id = m.id();
        m.op(Op::Constant, Some(ty), Some(id), &[v]);
        id
    }

    /// The `i32` constant `v`.
    pub fn i32_const(&mut self, m: &mut Module, v: i32) -> Id {
        let ty = self.int(m, 32);
        let id = m.id();
        m.op(Op::Constant, Some(ty), Some(id), &[v as u32]);
        id
    }

    /// The `f32` constant with these bits.
    pub fn f32_const(&mut self, m: &mut Module, bits: u32) -> Id {
        let ty = self.f32(m);
        let id = m.id();
        m.op(Op::Constant, Some(ty), Some(id), &[bits]);
        id
    }

    /// A vector constant of `ty` from `components`.
    pub fn const_composite(&mut self, m: &mut Module, ty: Id, components: &[Id]) -> Id {
        let id = m.id();
        let operands: Vec<u32> = components.iter().map(|c| c.0).collect();
        m.op(Op::ConstantComposite, Some(ty), Some(id), &operands);
        id
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::parse::parse;

    #[test]
    fn types_are_built_once_each() {
        let mut m = Module::new();
        let mut t = Types::default();
        let a = t.u32(&mut m);
        let b = t.u32(&mut m);
        assert_eq!(a, b);
        let v1 = t.vector(&mut m, a, 4);
        let v2 = t.vector(&mut m, a, 4);
        assert_eq!(v1, v2);
        let p1 = t.pointer(&mut m, v1, StorageClass::Input);
        let p2 = t.pointer(&mut m, v1, StorageClass::Input);
        let p3 = t.pointer(&mut m, v1, StorageClass::Output);
        assert_eq!(p1, p2);
        assert_ne!(p1, p3);
        // Each type appears once in the instruction stream.
        let count = parse(&m.finish())
            .unwrap()
            .iter()
            .filter(|i| i.op == Op::TypePointer as u32)
            .count();
        assert_eq!(count, 2);
    }
}
