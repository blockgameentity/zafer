//! A hand-written SPIR-V module writer: the second backend's encoder.
//!
//! Shaders are translated from the console's GFX10 to an IR
//! (`victory_shader::ir`) and then encoded for a host API. The DXIL backend
//! writes LLVM bitcode (`victory_shader::emit::bitcode`); this one writes
//! SPIR-V, which Vulkan takes directly. Both encoders are plain word buffers
//! with an id allocator and typed helpers, so the emitter above them reads
//! the same either way.
//!
//! Only the instructions a shader needs are named here. Opcodes are the
//! specification's numbers; `tests` checks a subset of them against
//! `spirv-as`/`spirv-dis` when the tools are installed, so a typo cannot
//! drift silently.
//!
//! # Layout
//!
//! A module is a five-word header (`MAGIC`, `VERSION`, generator, the id
//! bound, the schema) and then instructions as
//! `word_count << 16 | opcode`. The bound is fixed when [`Module::finish`]
//! runs, so ids may be allocated in any order and forward references (phi
//! operands) work as in the DXIL writer.
#![allow(dead_code)]

pub mod enums;
pub mod module;
pub mod builder;
pub mod ops;
pub mod parse;
pub mod types;

pub use enums::{
    BuiltIn, Decoration, Dim, StorageClass, function_control, image_operands, loop_control,
    memory_access, selection_control,
};
pub use module::{Id, Module};
pub use ops::{
    AddressingModel, Capability, ExecutionMode, ExecutionModel, MemoryModel, Op,
};
pub use parse::{Instruction, parse, read_string};
pub use types::Types;

/// The SPIR-V magic number.
pub const MAGIC: u32 = 0x0723_0203;
/// The version this writer emits: SPIR-V 1.3 (1.3.0), what Vulkan 1.1
/// accepts.
pub const VERSION: u32 = 0x0001_0300;
