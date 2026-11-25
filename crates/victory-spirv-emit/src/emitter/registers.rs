//! The register file: a function variable per register dword, and the loads
//! and stores that read and write it.
//!
//! A register is a place whose content changes, which is exactly what a
//! SPIR-V function variable is, so the emitter never has to reproduce the
//! IR's SSA form (phis, block order, dominance) in SPIR-V's, which is where a
//! translator of this kind goes wrong. A load after a store sees the stored
//! value because the variable is one location.

use victory_shader::ir::{Reg, Value};
use victory_spirv::builder::Builder;
use victory_spirv::module::Id;

use super::Emitter;

impl<'a> Emitter<'a> {

    pub(crate) fn var_of(&mut self, b: &mut Builder, reg: Reg) -> Id {
        if let Some(&id) = self.vars.get(&reg) {
            return id;
        }
        let ty = match reg {
            Reg::Scc => self.types.bool_,
            _ => self.types.i32_,
        };
        let id = self.new_variable(b, ty);
        self.vars.insert(reg, id);
        id
    }

    /// The register a value lives in.
    pub(crate) fn reg_of(&self, value: Value) -> Option<Reg> {
        self.regs.get(&value).copied()
    }

    /// The `i32` a value holds.
    pub(crate) fn load_i32(&mut self, b: &mut Builder, value: Value) -> Option<Id> {
        let reg = self.reg_of(value)?;
        let var = self.var_of(b, reg);
        if reg == Reg::Scc {
            let bit = b.load(self.types.bool_, var);
            let one = b.constant_i32(1);
            let zero = b.constant_i32(0);
            return Some(b.select(self.types.i32_, bit, one, zero));
        }
        Some(b.load(self.types.i32_, var))
    }

    /// The `f32` a value holds, from its dword's bit pattern.
    pub(crate) fn load_f32(&mut self, b: &mut Builder, value: Value) -> Option<Id> {
        let bits = self.load_i32(b, value)?;
        Some(b.bitcast(self.types.f32_, bits))
    }

    /// The `bool` a value holds (SCC, or a dword's nonzero-ness).
    pub(crate) fn load_bit(&mut self, b: &mut Builder, value: Value) -> Option<Id> {
        let reg = self.reg_of(value)?;
        let var = self.var_of(b, reg);
        if reg == Reg::Scc {
            return Some(b.load(self.types.bool_, var));
        }
        let bits = b.load(self.types.i32_, var);
        let zero = b.constant_i32(0);
        Some(b.inotequal(self.types.bool_, bits, zero))
    }

    /// Writes a dword into a register.
    ///
    /// A register variable holds `i32` (or `bool` for SCC), so a value of
    /// any other type is bitcast before the store: Vulkan requires the
    /// stored object's type to match the pointer's, and a float result
    /// reaching a dword variable is what `spirv-val` reports as "pointer's
    /// type does not match object's type". Bitcasting here rather than at
    /// each arm means no future arm can forget it, and a store to a bool
    /// variable still goes through [`Self::store_bit`].
    pub(crate) fn store_i32(&mut self, b: &mut Builder, reg: Reg, value: Id) {
        let var = self.var_of(b, reg);
        if reg == Reg::Scc {
            let zero = b.constant_i32(0);
            let bit = b.inotequal(self.types.bool_, value, zero);
            b.store(var, bit);
            return;
        }
        // The variable is `i32`; an arm that produced `u32` (or any other
        // 32-bit type) is the same dword with a different name, so it is
        // bitcast rather than left for `spirv-val` to reject. Done here
        // because a register holds bits: no arm has to remember.
        let value = self.to_dword(b, value);
        b.store(var, value);
    }

    /// `value` as an `i32` dword: bitcast when its result type is another
    /// 32-bit type, unchanged when it already is (or is unknown).
    pub(crate) fn to_dword(&mut self, b: &mut Builder, value: Id) -> Id {
        match b.type_of(value) {
            Some(ty) if ty != self.types.i32_ => b.bitcast(self.types.i32_, value),
            _ => value,
        }
    }

    /// Writes a `bool` into a register.
    pub(crate) fn store_bit(&mut self, b: &mut Builder, reg: Reg, value: Id) {
        let var = self.var_of(b, reg);
        if reg == Reg::Scc {
            // An SCC variable is `bool`; a caller holding a dword reaches
            // `store_i32`, which compares it. Here the value must already be
            // a bool, and a comparison of an unknown one is not a bool.
            let value = match b.type_of(value) {
                Some(ty) if ty != self.types.bool_ => {
                    let zero = b.constant_i32(0);
                    b.inotequal(self.types.bool_, value, zero)
                }
                _ => value,
            };
            b.store(var, value);
            return;
        }
        let one = b.constant_i32(1);
        let zero = b.constant_i32(0);
        let bits = b.select(self.types.i32_, value, one, zero);
        b.store(var, bits);
    }
}
