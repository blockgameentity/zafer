//! Atomics and barriers: the synchronisation a compute or pixel shader's
//! unordered writes need.

use crate::module::Id;
use crate::ops::Op;

use super::Builder;

impl Builder {
    /// `OpAtomicLoad` of `ty` from `pointer` under `scope`.
    pub fn atomic_load(&mut self, ty: Id, pointer: Id, scope: u32, semantics: u32) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::AtomicLoad,
            Some(ty),
            Some(id),
            &[pointer.0, scope, semantics],
        );
        id
    }

    /// `OpAtomicStore`.
    pub fn atomic_store(&mut self, pointer: Id, scope: u32, semantics: u32, value: Id) {
        self.module.op(
            Op::AtomicStore,
            None,
            None,
            &[pointer.0, scope, semantics, value.0],
        );
    }

    /// `OpAtomicExchange`.
    pub fn atomic_exchange(&mut self, ty: Id, pointer: Id, scope: u32, semantics: u32, value: Id) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::AtomicExchange,
            Some(ty),
            Some(id),
            &[pointer.0, scope, semantics, value.0],
        );
        id
    }

    /// `OpAtomicCompareExchange`: `value` when the memory holds `comparator`.
    pub fn atomic_compare_exchange(
        &mut self,
        ty: Id,
        pointer: Id,
        scope: u32,
        equal_semantics: u32,
        unequal_semantics: u32,
        value: Id,
        comparator: Id,
    ) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::AtomicCompareExchange,
            Some(ty),
            Some(id),
            &[
                pointer.0,
                scope,
                equal_semantics,
                unequal_semantics,
                value.0,
                comparator.0,
            ],
        );
        id
    }

    /// `OpControlBarrier` across `execution_scope`.
    pub fn control_barrier(&mut self, execution_scope: u32, memory_scope: u32, semantics: u32) {
        self.module
            .op(Op::ControlBarrier, None, None, &[execution_scope, memory_scope, semantics]);
    }

    /// `OpMemoryBarrier` within `scope`.
    pub fn memory_barrier(&mut self, scope: u32, semantics: u32) {
        self.module
            .op(Op::MemoryBarrier, None, None, &[scope, semantics]);
    }

    /// `OpGroupNonUniform<group_op>` of a single `value`.
    pub fn group_non_uniform(&mut self, ty: Id, execution: u32, group_op: u32, value: Id) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::GroupNonUniform,
            Some(ty),
            Some(id),
            &[execution, group_op, value.0],
        );
        id
    }
}

/// Declares one of the atomic read-modify-write instructions.
macro_rules! atomic_rmw {
    ($($name:ident => $op:ident,)*) => {
        impl Builder {
            $(
                #[doc = concat!("`Op", stringify!($op), "`.")]
                pub fn $name(&mut self, ty: Id, pointer: Id, scope: u32, semantics: u32, value: Id) -> Id {
                    let id = self.module.id();
                    self.module.op(
                        Op::$op,
                        Some(ty),
                        Some(id),
                        &[pointer.0, scope, semantics, value.0],
                    );
                    id
                }
            )*
        }
    };
}

atomic_rmw! {
    atomic_iadd => AtomicIAdd,
    atomic_isub => AtomicISub,
    atomic_smin => AtomicSMin,
    atomic_umin => AtomicUMin,
    atomic_smax => AtomicSMax,
    atomic_umax => AtomicUMax,
    atomic_and => AtomicAnd,
    atomic_or => AtomicOr,
    atomic_xor => AtomicXor,
}

impl Builder {
    /// `OpAtomicIIncrement`.
    pub fn atomic_iincrement(&mut self, ty: Id, pointer: Id, scope: u32, semantics: u32) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::AtomicIIncrement,
            Some(ty),
            Some(id),
            &[pointer.0, scope, semantics],
        );
        id
    }

    /// `OpAtomicIDecrement`.
    pub fn atomic_idecrement(&mut self, ty: Id, pointer: Id, scope: u32, semantics: u32) -> Id {
        let id = self.module.id();
        self.module.op(
            Op::AtomicIDecrement,
            Some(ty),
            Some(id),
            &[pointer.0, scope, semantics],
        );
        id
    }
}
