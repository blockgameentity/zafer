//! The word buffer: an id allocator and the instructions written so far.

use crate::ops::Op;
use crate::{MAGIC, VERSION};

/// A result id.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Id(pub u32);

/// A growable SPIR-V instruction buffer and its id allocator.
///
/// SPIR-V fixes the order of its sections: capabilities and extensions, the
/// memory model, entry points, execution modes, debug names, decorations,
/// types and constants, then functions. A shader is simplest to emit in
/// program order, not in section order, so each instruction goes into the
/// buffer of its section and the file is assembled in [`Module::finish`].
pub struct Module {
    /// One buffer per section, indexed by [`Section`].
    sections: [Vec<u32>; Section::COUNT],
    /// One past the largest id handed out.
    bound: u32,
}

impl Default for Module {
    /// An empty module. Id 0 is never used (the specification reserves it),
    /// so a defaulted module starts its allocator at 1 like [`Module::new`].
    fn default() -> Self {
        Module {
            sections: Default::default(),
            bound: 1,
        }
    }
}

/// The module's sections, in the order a file lays them out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Section {
    Capability = 0,
    Extension,
    ExtInstImport,
    MemoryModel,
    EntryPoint,
    ExecutionMode,
    Debug,
    Annotation,
    Type,
    Function,
}

impl Section {
    /// One per variant, for the array's length.
    pub const COUNT: usize = 10;

    /// Which section an instruction belongs to.
    ///
    /// The mapping is fixed by the specification; anything not named here
    /// is a function-body instruction.
    pub fn of(op: Op) -> Section {
        match op {
            Op::Capability => Section::Capability,
            Op::Extension => Section::Extension,
            Op::ExtInstImport => Section::ExtInstImport,
            Op::MemoryModel => Section::MemoryModel,
            Op::EntryPoint => Section::EntryPoint,
            Op::ExecutionMode => Section::ExecutionMode,
            Op::SourceContinued
            | Op::Source
            | Op::SourceExtension
            | Op::Name
            | Op::MemberName
            | Op::String
            | Op::Line => Section::Debug,
            Op::Decorate | Op::MemberDecorate => Section::Annotation,
            Op::TypeVoid
            | Op::TypeBool
            | Op::TypeInt
            | Op::TypeFloat
            | Op::TypeVector
            | Op::TypeMatrix
            | Op::TypeImage
            | Op::TypeSampler
            | Op::TypeSampledImage
            | Op::TypeArray
            | Op::TypeRuntimeArray
            | Op::TypeStruct
            | Op::TypeOpaque
            | Op::TypePointer
            | Op::TypeFunction
            | Op::ConstantTrue
            | Op::ConstantFalse
            | Op::Constant
            | Op::ConstantComposite
            | Op::ConstantSampler
            | Op::ConstantNull
            | Op::SpecConstantTrue
            | Op::SpecConstantFalse
            | Op::SpecConstant
            | Op::SpecConstantComposite
            | Op::Undef
            | Op::Variable => Section::Type,
            _ => Section::Function,
        }
    }
}

impl Module {
    /// An empty module: no header yet, no ids.
    pub fn new() -> Self {
        Module {
            sections: Default::default(),
            bound: 1,
        }
    }

    /// A fresh id. Ids are never reused.
    pub fn id(&mut self) -> Id {
        let id = Id(self.bound);
        self.bound += 1;
        id
    }

    /// The largest id handed out so far (zero when none has been).
    pub fn bound(&self) -> u32 {
        self.bound
    }

    /// Appends `op` with an optional result type and result id and its
    /// operand words.
    pub fn op(&mut self, op: Op, result_type: Option<Id>, result: Option<Id>, operands: &[u32]) {
        let mut words = 1;
        words += usize::from(result_type.is_some());
        words += usize::from(result.is_some());
        words += operands.len();
        assert!(words < 0x1_0000, "an instruction's word count is 16 bits");
        let section = &mut self.sections[Section::of(op) as usize];
        section.push((words as u32) << 16 | op as u32);
        if let Some(t) = result_type {
            section.push(t.0);
        }
        if let Some(r) = result {
            section.push(r.0);
        }
        section.extend_from_slice(operands);
    }

    /// Appends `op` for a result of `ty`, id `result`.
    pub fn op_result(&mut self, op: Op, ty: Id, result: Id, operands: &[u32]) {
        self.op(op, Some(ty), Some(result), operands);
    }

    /// Appends `op` into the function body whatever its usual section is.
    ///
    /// `OpVariable` is the instruction this exists for: at module scope it
    /// belongs with the types, and inside a function it belongs in the body,
    /// and a function-scope one written to the type section is a module
    /// Vulkan (and `spirv-val`) rejects.
    pub fn op_in_function(
        &mut self,
        op: Op,
        result_type: Option<Id>,
        result: Option<Id>,
        operands: &[u32],
    ) {
        let mut words = 1;
        words += usize::from(result_type.is_some());
        words += usize::from(result.is_some());
        words += operands.len();
        assert!(words < 0x1_0000, "an instruction's word count is 16 bits");
        let section = &mut self.sections[Section::Function as usize];
        section.push((words as u32) << 16 | op as u32);
        if let Some(t) = result_type {
            section.push(t.0);
        }
        if let Some(r) = result {
            section.push(r.0);
        }
        section.extend_from_slice(operands);
    }

    /// Appends `op` whose only operand is `id` (branch targets, merges).
    pub fn op_id(&mut self, op: Op, id: Id) {
        self.op(op, None, None, &[id.0]);
    }

    /// Appends an instruction carrying a literal string as its last
    /// operand: `OpName`, `OpMemberName`, `OpEntryPoint`.
    pub fn op_string(&mut self, op: Op, prefix: &[u32], s: &str) {
        let mut operands = prefix.to_vec();
        operands.extend(literal_string(s));
        self.op(op, None, None, &operands);
    }

    /// The instruction words of one section (no header).
    pub fn section(&self, section: Section) -> &[u32] {
        &self.sections[section as usize]
    }

    /// The instruction words written so far, in the file's order.
    pub fn words(&self) -> Vec<u32> {
        let mut out = Vec::new();
        for section in self.sections.iter() {
            out.extend_from_slice(section);
        }
        out
    }

    /// The module as a file: the header, then each section in the order the
    /// specification fixes.
    pub fn finish(self) -> Vec<u8> {
        let total: usize = self.sections.iter().map(|s| s.len()).sum();
        let mut all = Vec::with_capacity(total + 5);
        all.push(MAGIC);
        all.push(VERSION);
        all.push(0); // generator: none, deliberately
        all.push(self.bound.max(1));
        all.push(0); // schema
        for section in self.sections.iter() {
            all.extend_from_slice(section);
        }
        all.iter().flat_map(|w| w.to_le_bytes()).collect()
    }
}

/// A NUL-terminated literal string as SPIR-V words, zero-padded.
pub fn literal_string(s: &str) -> Vec<u32> {
    let bytes = s.as_bytes();
    let words = bytes.len() / 4 + 1;
    let mut out = vec![0u32; words];
    for (i, &b) in bytes.iter().enumerate() {
        out[i / 4] |= u32::from(b) << (8 * (i % 4));
    }
    out
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::ops::Capability;
    use crate::parse::read_string;

    #[test]
    fn the_header_is_magic_version_generator_bound_schema() {
        let out = Module::new().finish();
        let words: Vec<u32> = out
            .chunks_exact(4)
            .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect();
        assert_eq!(words, [MAGIC, VERSION, 0, 1, 0]);
        assert_eq!(out.len(), 20);
    }

    #[test]
    fn an_instructions_word_count_and_opcode_are_packed() {
        let mut m = Module::new();
        m.op(Op::Capability, None, None, &[Capability::Shader as u32]);
        assert_eq!(m.words(), [2 << 16 | 17, 1]);
        // With a type and result: two more words.
        let mut m = Module::new();
        let (ty, result) = (m.id(), m.id());
        m.op(Op::Load, Some(ty), Some(result), &[7]);
        assert_eq!(m.words(), [4 << 16 | 61, ty.0, result.0, 7]);
    }

    #[test]
    fn ids_increase_and_the_bound_follows() {
        let mut m = Module::new();
        let a = m.id();
        let b = m.id();
        assert_eq!((a, b), (Id(1), Id(2)));
        assert_eq!(m.bound(), 3);
    }

    #[test]
    fn literal_strings_pad_and_read_back() {
        assert_eq!(literal_string("ab"), [u32::from_le_bytes([b'a', b'b', 0, 0])]);
        let words = literal_string("main");
        assert_eq!(words.len(), 2);
        assert_eq!(read_string(&words).as_deref(), Some("main"));
        // Four characters need a fifth byte for the terminator.
        assert_eq!(literal_string("abcd").len(), 2);
    }
}
