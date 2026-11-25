//! Reading a finished module back instruction by instruction.

use crate::{MAGIC, VERSION};

/// Reads a literal string back out of instruction words (tests and
/// diagnostics only).
pub fn read_string(words: &[u32]) -> Option<String> {
    let mut bytes = Vec::new();
    for w in words {
        for i in 0..4 {
            let b = (w >> (8 * i)) as u8;
            if b == 0 {
                return Some(String::from_utf8_lossy(&bytes).into_owned());
            }
            bytes.push(b);
        }
    }
    None
}

/// An instruction as [`Module::parse`] finds it.
#[derive(Clone, Debug)]
pub struct Instruction {
    pub op: u32,
    /// Operand words, without the opcode's word.
    pub operands: Vec<u32>,
}

/// Every instruction of a finished module, so tests can check a build
/// without an external tool. `None` if the buffer is not a module.
pub fn parse(module: &[u8]) -> Option<Vec<Instruction>> {
    let words: Vec<u32> = module
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    if words.len() < 5 || words[0] != MAGIC || words[1] != VERSION {
        return None;
    }
    let mut out = Vec::new();
    let mut i = 5;
    while i < words.len() {
        let count = (words[i] >> 16) as usize;
        if count == 0 || i + count > words.len() {
            return None;
        }
        out.push(Instruction {
            op: words[i] & 0xFFFF,
            operands: words[i + 1..i + count].to_vec(),
        });
        i += count;
    }
    Some(out)
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::module::Module;
    use crate::enums::function_control;
    use crate::ops::{
        AddressingModel, Capability, ExecutionMode, ExecutionModel, MemoryModel, Op,
    };
    use crate::types::Types;

    fn build_trivial_compute() -> Vec<u8> {
        let mut m = Module::new();
        let mut t = Types::default();
        m.op(Op::Capability, None, None, &[Capability::Shader as u32]);
        m.op(
            Op::MemoryModel,
            None,
            None,
            &[AddressingModel::Logical as u32, MemoryModel::Glsl450 as u32],
        );
        // The layout sections are ordered: capabilities, extensions, the
        // memory model, entry points, execution modes, debug names,
        // annotations, types and constants, then functions.
        let main = m.id();
        m.op_string(Op::EntryPoint, &[ExecutionModel::GlCompute as u32, main.0], "main");
        m.op(Op::ExecutionMode, None, None, &[main.0, ExecutionMode::LocalSize as u32, 1, 1, 1]);
        let void = t.void(&mut m);
        let fn_ty = t.function(&mut m, void, &[]);
        m.op(Op::Function, Some(void), Some(main), &[function_control::NONE, fn_ty.0]);
        let entry = m.id();
        m.op(Op::Label, None, Some(entry), &[]);
        m.op(Op::Return, None, None, &[]);
        m.op(Op::FunctionEnd, None, None, &[]);
        m.finish()
    }
    #[test]
    fn a_module_parses_back_instruction_for_instruction() {
        let out = build_trivial_compute();
        let ops = parse(&out).expect("our own module parses");
        assert_eq!(ops.len(), 10, "{ops:#?}");
        assert_eq!(ops[0].op, Op::Capability as u32);
        assert_eq!(ops[1].op, Op::MemoryModel as u32);
        assert_eq!(ops[2].op, Op::EntryPoint as u32);
        // OpEntryPoint: execution model, entry id, then the name words.
        assert_eq!(ops[2].operands[0], ExecutionModel::GlCompute as u32);
        assert_eq!(read_string(&ops[2].operands[2..]).as_deref(), Some("main"));
        assert_eq!(ops[3].operands[0], ops[2].operands[1]);
        assert_eq!(ops[3].operands[1], ExecutionMode::LocalSize as u32);
        assert_eq!(ops[9].op, Op::FunctionEnd as u32);
        assert!(parse(&[0u8; 8]).is_none(), "not a module");
    }
    /// The point of the writer: a module a Vulkan driver would accept.
    #[test]
    fn a_trivial_compute_module_is_valid_spir_v() {
        let module = build_trivial_compute();
        let dir = std::env::temp_dir().join(format!("victory-spirv-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("trivial.spv");
        std::fs::write(&path, &module).unwrap();
        let val = std::process::Command::new("spirv-val")
            .arg(&path)
            .output();
        match val {
            Err(_) => {
                // No validator here: the structural invariants are all we
                // can check, and `a_module_parses_back_instruction_for_instruction`
                // already covers them.
                let ops = parse(&module).unwrap();
                assert_eq!(ops.len(), 10);
            }
            Ok(o) => assert!(
                o.status.success(),
                "spirv-val refused our module: {}",
                String::from_utf8_lossy(&o.stderr)
            ),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
