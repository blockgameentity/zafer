//! The header: capabilities, extensions, memory model, entry points and the
//! decorations the API reads a value's meaning from.

use crate::enums::{BuiltIn, Decoration};
use crate::module::Id;
use crate::ops::{AddressingModel, Capability, ExecutionMode, ExecutionModel, MemoryModel, Op};

use super::Builder;

impl Builder {
    /// `OpCapability`.
    pub fn capability(&mut self, capability: Capability) {
        self.module
            .op(Op::Capability, None, None, &[capability as u32]);
    }

    /// `OpExtension`.
    pub fn extension(&mut self, name: &str) {
        self.module.op_string(Op::Extension, &[], name);
    }

    /// `OpExtInstImport`, whose id goes back into `OpExtInst`.
    pub fn ext_inst_import(&mut self, name: &str) -> Id {
        let id = self.module.id();
        self.module.op_string(Op::ExtInstImport, &[id.0], name);
        id
    }

    /// `OpExtInst`: `set`'s instruction `number` with `args`.
    pub fn ext_inst(&mut self, ty: Id, set: Id, number: u32, args: &[Id]) -> Id {
        let mut operands = vec![set.0, number];
        operands.extend(args.iter().map(|a| a.0));
        let id = self.module.id();
        self.module.op(Op::ExtInst, Some(ty), Some(id), &operands);
        id
    }

    /// `OpMemoryModel`.
    pub fn memory_model(&mut self, addressing: AddressingModel, model: MemoryModel) {
        self.module.op(
            Op::MemoryModel,
            None,
            None,
            &[addressing as u32, model as u32],
        );
    }

    /// `OpEntryPoint`, with the interface variables it reads and writes.
    pub fn entry_point(
        &mut self,
        model: ExecutionModel,
        function: Id,
        name: &str,
        interface: &[Id],
    ) {
        // The name's literal words come between the function and the
        // interface ids, so the operand list is built by hand rather than
        // through `op_string` (which puts the string last).
        let mut operands = vec![model as u32, function.0];
        operands.extend(crate::module::literal_string(name));
        operands.extend(interface.iter().map(|i| i.0));
        self.module.op(Op::EntryPoint, None, None, &operands);
    }

    /// `OpExecutionMode`.
    pub fn execution_mode(&mut self, entry: Id, mode: ExecutionMode, literals: &[u32]) {
        let mut operands = vec![entry.0, mode as u32];
        operands.extend_from_slice(literals);
        self.module.op(Op::ExecutionMode, None, None, &operands);
    }

    /// `OpName`.
    pub fn name(&mut self, id: Id, name: &str) {
        self.module.op_string(Op::Name, &[id.0], name);
    }

    /// `OpMemberName`.
    pub fn member_name(&mut self, ty: Id, member: u32, name: &str) {
        self.module.op_string(Op::MemberName, &[ty.0, member], name);
    }

    /// `OpDecorate`.
    pub fn decorate(&mut self, id: Id, decoration: Decoration, operands: &[u32]) {
        let mut words = vec![id.0, decoration as u32];
        words.extend_from_slice(operands);
        self.module.op(Op::Decorate, None, None, &words);
    }

    /// `OpMemberDecorate`.
    pub fn member_decorate(
        &mut self,
        ty: Id,
        member: u32,
        decoration: Decoration,
        operands: &[u32],
    ) {
        let mut words = vec![ty.0, member, decoration as u32];
        words.extend_from_slice(operands);
        self.module.op(Op::MemberDecorate, None, None, &words);
    }

    /// `BuiltIn` on `id`.
    pub fn decorate_builtin(&mut self, id: Id, builtin: BuiltIn) {
        self.decorate(id, Decoration::BuiltIn, &[builtin as u32]);
    }

    /// `Location` on `id`.
    pub fn decorate_location(&mut self, id: Id, location: u32) {
        self.decorate(id, Decoration::Location, &[location]);
    }

    /// `Offset` on member `member` of `ty`.
    pub fn decorate_offset(&mut self, ty: Id, member: u32, offset: u32) {
        self.member_decorate(ty, member, Decoration::Offset, &[offset]);
    }
}

