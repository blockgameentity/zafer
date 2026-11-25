//! A typed builder over [`Module`].
//!
//! `Module` appends raw operand words; a shader emitter that did that by
//! hand would spell out every instruction's word layout and every id
//! twice. This layer names each instruction once, caches the types and
//! constants a module declares (a type is emitted once however often it is
//! asked for), and hands back the result id.
//!
//! The layer is descriptive, not semantic: it checks nothing about what a
//! shader means, so the emitter above it still decides which instructions
//! a program needs. What it does guarantee is that every id it hands out is
//! unique and every type it declares exists exactly once.

mod arith;
mod atomics;
mod constants;
mod decorate;
mod function;
mod images;
mod memory;
#[cfg(test)]
mod tests;

pub use function::{Block, Function};

use crate::enums::{Dim, StorageClass};
use crate::module::{Id, Module};
use crate::ops::Op;

/// One image type, as the module declares it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ImageType {
    /// The sampled type, usually a float or integer scalar.
    pub sampled: Id,
    pub dim: Dim,
    /// 0: not a depth image, 1: depth, 2: depth with a comparison result.
    pub depth: u32,
    pub arrayed: bool,
    pub multisampled: bool,
    /// 0: unknown at compile time, 1: sampled, 2: storage.
    pub sampled_kind: u32,
    /// `VkFormat`, or zero for unknown. Kept as the SPIR-V literal.
    pub format: u32,
}

/// The types a module has declared, so each is emitted once.
#[derive(Default)]
struct Declared {
    void: Option<Id>,
    bool_: Option<Id>,
    /// By `(bits, signed)`.
    ints: Vec<(u32, bool, Id)>,
    floats: Vec<(u32, Id)>,
    vectors: Vec<(Id, u32, Id)>,
    matrices: Vec<(Id, u32, Id)>,
    arrays: Vec<(Id, u32, Id)>,
    /// By the ids of the fields, in order.
    structs: Vec<(Vec<u32>, Id)>,
    /// By `(storage class, pointee)`.
    pointers: Vec<(u32, Id, Id)>,
    /// By `(result, parameter types)`.
    functions: Vec<(Id, Vec<Id>, Id)>,
    images: Vec<(ImageType, Id)>,
    sampler: Option<Id>,
    sampled_images: Vec<(Id, Id)>,
}

/// A module under construction: its words, its ids and its declared types.
#[derive(Default)]
pub struct Builder {
    /// The instruction buffer this builder appends to.
    pub module: Module,
    declared: Declared,
    /// Every `OpConstant` emitted, by `(type, literal words)`.
    pub(super) constant_cache: Vec<(Id, Vec<u32>)>,
}

impl Builder {
    /// An empty module.
    pub fn new() -> Self {
        Builder::default()
    }

    /// The instruction buffer, for the operations this layer does not name.
    pub fn module(&mut self) -> &mut Module {
        &mut self.module
    }

    /// A fresh id (the emitter's own scratch variables and labels).
    pub fn id(&mut self) -> Id {
        self.module.id()
    }

    /// The module as a file, header and all.
    pub fn finish(self) -> Vec<u8> {
        self.module.finish()
    }

    /// `%void`.
    pub fn void(&mut self) -> Id {
        if let Some(t) = self.declared.void {
            return t;
        }
        let t = self.module.id();
        self.module.op(Op::TypeVoid, None, Some(t), &[]);
        self.declared.void = Some(t);
        t
    }

    /// `%bool`.
    pub fn bool(&mut self) -> Id {
        if let Some(t) = self.declared.bool_ {
            return t;
        }
        let t = self.module.id();
        self.module.op(Op::TypeBool, None, Some(t), &[]);
        self.declared.bool_ = Some(t);
        t
    }

    /// An integer of `bits`, `signed` or unsigned.
    pub fn int(&mut self, bits: u32, signed: bool) -> Id {
        if let Some((_, _, t)) = self
            .declared
            .ints
            .iter()
            .find(|(b, s, _)| *b == bits && *s == signed)
        {
            return *t;
        }
        let t = self.module.id();
        self.module
            .op(Op::TypeInt, None, Some(t), &[bits, u32::from(signed)]);
        self.declared.ints.push((bits, signed, t));
        t
    }

    /// A float of `bits`.
    pub fn float(&mut self, bits: u32) -> Id {
        if let Some((_, t)) = self.declared.floats.iter().find(|(b, _)| *b == bits) {
            return *t;
        }
        let t = self.module.id();
        self.module.op(Op::TypeFloat, None, Some(t), &[bits]);
        self.declared.floats.push((bits, t));
        t
    }

    /// `vec<count>` of `component`.
    pub fn vector(&mut self, component: Id, count: u32) -> Id {
        if let Some((_, _, t)) = self
            .declared
            .vectors
            .iter()
            .find(|(c, n, _)| *c == component && *n == count)
        {
            return *t;
        }
        let t = self.module.id();
        self.module
            .op(Op::TypeVector, None, Some(t), &[component.0, count]);
        self.declared.vectors.push((component, count, t));
        t
    }

    /// A matrix of `columns` columns of type `column`.
    pub fn matrix(&mut self, column: Id, columns: u32) -> Id {
        if let Some((_, _, t)) = self
            .declared
            .matrices
            .iter()
            .find(|(c, n, _)| *c == column && *n == columns)
        {
            return *t;
        }
        let t = self.module.id();
        self.module
            .op(Op::TypeMatrix, None, Some(t), &[column.0, columns]);
        self.declared.matrices.push((column, columns, t));
        t
    }

    /// An array of `length` elements of `element`.
    pub fn array(&mut self, element: Id, length: u32) -> Id {
        if let Some((_, _, t)) = self
            .declared
            .arrays
            .iter()
            .find(|(e, n, _)| *e == element && *n == length)
        {
            return *t;
        }
        let t = self.module.id();
        let len = self.constant_u32(length);
        self.module
            .op(Op::TypeArray, None, Some(t), &[element.0, len.0]);
        self.declared.arrays.push((element, length, t));
        t
    }

    /// An array whose length the pipeline fixes (`OpTypeRuntimeArray`).
    pub fn runtime_array(&mut self, element: Id) -> Id {
        if let Some((_, _, t)) = self
            .declared
            .arrays
            .iter()
            .find(|(e, n, _)| *e == element && *n == 0)
        {
            return *t;
        }
        let t = self.module.id();
        self.module
            .op(Op::TypeRuntimeArray, None, Some(t), &[element.0]);
        self.declared.arrays.push((element, 0, t));
        t
    }

    /// A plain struct of `fields`.
    pub fn struct_of(&mut self, fields: &[Id]) -> Id {
        let key: Vec<u32> = fields.iter().map(|f| f.0).collect();
        if let Some((_, t)) = self.declared.structs.iter().find(|(f, _)| *f == key) {
            return *t;
        }
        let t = self.module.id();
        self.module.op(Op::TypeStruct, None, Some(t), &key);
        self.declared.structs.push((key, t));
        t
    }

    /// A struct with a debug name, as the header's built-in blocks have.
    pub fn named_struct(&mut self, name: &str, fields: &[Id]) -> Id {
        let t = self.struct_of(fields);
        self.name(t, name);
        t
    }

    /// `space`-pointer to `pointee`.
    pub fn pointer(&mut self, space: StorageClass, pointee: Id) -> Id {
        let key = space as u32;
        if let Some((_, _, t)) = self
            .declared
            .pointers
            .iter()
            .find(|(s, p, _)| *s == key && *p == pointee)
        {
            return *t;
        }
        let t = self.module.id();
        self.module
            .op(Op::TypePointer, None, Some(t), &[key, pointee.0]);
        self.declared.pointers.push((key, pointee, t));
        t
    }

    /// A function type: `result` from `params`.
    pub fn function_type(&mut self, result: Id, params: &[Id]) -> Id {
        if let Some((_, _, t)) = self
            .declared
            .functions
            .iter()
            .find(|(r, p, _)| *r == result && p.as_slice() == params)
        {
            return *t;
        }
        let mut operands = vec![result.0];
        operands.extend(params.iter().map(|p| p.0));
        let t = self.module.id();
        self.module.op(Op::TypeFunction, None, Some(t), &operands);
        self.declared
            .functions
            .push((result, params.to_vec(), t));
        t
    }

    /// An image type.
    pub fn image(&mut self, image: ImageType) -> Id {
        if let Some((_, t)) = self.declared.images.iter().find(|(i, _)| *i == image) {
            return *t;
        }
        let operands = [
            image.sampled.0,
            image.dim as u32,
            image.depth,
            u32::from(image.arrayed),
            u32::from(image.multisampled),
            image.sampled_kind,
            image.format,
        ];
        let t = self.module.id();
        self.module.op(Op::TypeImage, None, Some(t), &operands);
        self.declared.images.push((image, t));
        t
    }

    /// `%sampler`.
    pub fn sampler(&mut self) -> Id {
        if let Some(t) = self.declared.sampler {
            return t;
        }
        let t = self.module.id();
        self.module.op(Op::TypeSampler, None, Some(t), &[]);
        self.declared.sampler = Some(t);
        t
    }

    /// The type of a combined image and sampler of `image`.
    pub fn sampled_image_type(&mut self, image: Id) -> Id {
        if let Some((_, t)) = self.declared.sampled_images.iter().find(|(i, _)| *i == image) {
            return *t;
        }
        let t = self.module.id();
        self.module
            .op(Op::TypeSampledImage, None, Some(t), &[image.0]);
        self.declared.sampled_images.push((image, t));
        t
    }

    /// `%undef`, of type `ty`.
    pub fn undef(&mut self, ty: Id) -> Id {
        let t = self.module.id();
        self.module.op(Op::Undef, Some(ty), Some(t), &[]);
        t
    }
}
