//! The console's shader IR to SPIR-V.
//!
//! A console shader is translated in stages (`victory_shader`): GFX10 code
//! to a one-to-one SSA [`Program`](victory_shader::ir::Program), that to a
//! per-lane [`Structured`](victory_shader::structure::Structured), its
//! resources, and its interface. This crate is the last stage: every
//! instruction of the program becomes SPIR-V through a typed
//! [`Builder`](victory_spirv::builder::Builder), so Vulkan takes the module
//! directly.
//!
//! It replaces the DXIL emitter instruction for instruction. Where that one
//! bitcasts every register to `i32` and calls `dx.op` intrinsics, this one
//! keeps `f32` registers as floats and names the arithmetic instruction
//! Vulkan has for each operation.
//!
//! # Layout
//!
//! [`module`] builds the header, the interface and the entry point;
//! [`emitter`] owns the per-instruction translation; [`driver`] runs the
//! three translation stages and hands back a finished module.

pub mod driver;
pub mod emitter;
pub mod interface;
pub mod module;
pub mod report;
pub mod validate;
pub mod symbol;

pub use driver::{Translation, translate};
pub use module::Module;
pub use report::Report;

use victory_shader::ShaderError;

/// What the emitter could not express.
///
/// A translated shader either produces a module or says which instruction
/// and why not: a silent gap would be a shader that renders wrongly.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Error {
    /// The program uses something SPIR-V has no instruction for (yet).
    #[error("GFX10 code at {offset:#x}: {what} has no SPIR-V form")]
    Unsupported { offset: usize, what: &'static str },
    /// The module was emitted but is not valid SPIR-V (`spirv-val`'s
    /// finding). A module Vulkan refuses is not a translation.
    #[error("spirv-val: {0}")]
    Invalid(String),
    /// The translation stages before this one failed.
    #[error(transparent)]
    Shader(#[from] ShaderError),
}
