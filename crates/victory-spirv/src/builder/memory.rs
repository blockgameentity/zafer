//! Variables and memory access: the pointer every other instruction works
//! through.

use crate::enums::StorageClass;
use crate::module::Id;
use crate::ops::Op;

use super::Builder;

impl Builder {
    /// A module-scope variable of `ty` in `space`, with an optional
    /// initializer (which a `UniformConstant` takes as an image or sampler).
    pub fn variable(&mut self, ty: Id, space: StorageClass, initializer: Option<Id>) -> Id {
        let ptr = self.pointer(space, ty);
        let id = self.module.id();
        // The storage class is an operand of its own, before the
        // initializer: a module without it is not SPIR-V.
        let mut operands = vec![space as u32];
        operands.extend(initializer.map(|i| i.0));
        self.module
            .op(Op::Variable, Some(ptr), Some(id), &operands);
        id
    }

    /// A function-scope variable, declared in `block`.
    pub fn function_variable(
        &mut self,
        block: super::Block,
        ty: Id,
        space: StorageClass,
        initializer: Option<Id>,
    ) -> Id {
        let ptr = self.pointer(space, ty);
        let id = self.module.id();
        let mut operands = vec![space as u32];
        operands.extend(initializer.map(|i| i.0));
        // A function variable lives in the body, not with the types.
        self.module
            .op_in_function(Op::Variable, Some(ptr), Some(id), &operands);
        let _ = block;
        id
    }

    /// `OpLoad` of `ty` from `pointer`.
    pub fn load(&mut self, ty: Id, pointer: Id) -> Id {
        let id = self.module.id();
        self.module.op(Op::Load, Some(ty), Some(id), &[pointer.0]);
        id
    }

    /// `OpStore` of `value` at `pointer`.
    pub fn store(&mut self, pointer: Id, value: Id) {
        self.module
            .op(Op::Store, None, None, &[pointer.0, value.0]);
    }

    /// `OpAccessChain`: `ty` is the type of what the chain designates.
    pub fn access_chain(&mut self, ty: Id, base: Id, indices: &[Id]) -> Id {
        let mut operands = vec![base.0];
        operands.extend(indices.iter().map(|i| i.0));
        let id = self.module.id();
        self.module
            .op(Op::AccessChain, Some(ty), Some(id), &operands);
        id
    }

    /// `OpInBoundsAccessChain`, which promises every index is in range.
    pub fn in_bounds_access_chain(&mut self, ty: Id, base: Id, indices: &[Id]) -> Id {
        let mut operands = vec![base.0];
        operands.extend(indices.iter().map(|i| i.0));
        let id = self.module.id();
        self.module
            .op(Op::InBoundsAccessChain, Some(ty), Some(id), &operands);
        id
    }

    /// `OpCopyMemory`.
    pub fn copy_memory(&mut self, target: Id, source: Id) {
        self.module
            .op(Op::CopyMemory, None, None, &[target.0, source.0]);
    }

    /// `OpCopyObject`, which copies a value (a load's result, say) into a
    /// new id so a store may define it separately.
    pub fn copy_object(&mut self, ty: Id, value: Id) -> Id {
        let id = self.module.id();
        self.module
            .op(Op::CopyObject, Some(ty), Some(id), &[value.0]);
        id
    }
}
