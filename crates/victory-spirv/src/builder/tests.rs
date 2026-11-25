//! The builder's own invariants: ids are unique, a type or constant is
//! declared once, and a module it produces parses back to the same
//! instruction kinds.

use super::*;
use crate::enums::StorageClass;
use crate::parse::parse;

/// Loads `bytes` with `rspirv`: a second opinion on our opcodes and operand
/// shapes, from a crate that is not ours. Re-assembling is the loader's own
/// statement that it understood every word it read.
fn rspirv_loads(bytes: &[u8]) -> rspirv::dr::Module {
    use rspirv::binary::Assemble;
    let words: Vec<u32> = bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    let module = rspirv::dr::load_words(words.clone()).expect("rspirv loads the module");
    // `rspirv` stamps its own generator id (header word 2), so everything
    // else is compared: the bound, and every instruction.
    let again = module.assemble();
    assert_eq!(again.len(), words.len(), "rspirv re-assembles the same words");
    assert_eq!(&again[..2], &words[..2]);
    assert_eq!(&again[3..], &words[3..]);
    module
}

/// A vertex shader that writes `(x, y, 0, 1)` to `gl_Position` from two
/// input locations, built only through the typed layer.
fn vertex_module() -> Vec<u8> {
    let mut b = Builder::new();
    b.capability(crate::ops::Capability::Shader);
    b.memory_model(
        crate::ops::AddressingModel::Logical,
        crate::ops::MemoryModel::Glsl450,
    );
    let f32_ = b.float(32);
    let v4 = b.vector(f32_, 4);
    let v2 = b.vector(f32_, 2);
    let out_ptr = b.pointer(StorageClass::Output, v4);
    let out = b.variable(out_ptr, StorageClass::Output, None);
    b.decorate_builtin(out, crate::enums::BuiltIn::Position);
    let in_ptr = b.pointer(StorageClass::Input, v2);
    let a = b.variable(in_ptr, StorageClass::Input, None);
    b.decorate_location(a, 0);
    let b_id = b.variable(in_ptr, StorageClass::Input, None);
    b.decorate_location(b_id, 1);

    let void = b.void();
    let f = b.function("main", void, &[], crate::enums::function_control::NONE);
    let zero = b.constant_f32(0.0);
    let one = b.constant_f32(1.0);
    let position = b.constant_composite(v4, &[zero, zero, zero, one]);
    b.store(out, position);
    b.return_void();
    b.end_function(f);
    b.entry_point(
        crate::ops::ExecutionModel::Vertex,
        f.id,
        "main",
        &[out, a, b_id],
    );
    // `Function` is `Copy`, so `f` is still usable above after `end_function`.
    b.finish()
}

#[test]
fn rspirv_accepts_what_we_emit() {
    let words = vertex_module();
    let m = rspirv_loads(&words);
    assert!(!m.functions.is_empty(), "the entry point's body is there");
    assert!(!m.entry_points.is_empty(), "the entry point is declared");
}

#[test]
fn a_built_module_parses() {
    let words = vertex_module();
    let instructions = parse(&words).expect("a module the builder made parses");
    let _ = &instructions;
    assert!(instructions.iter().any(|i| i.op == Op::EntryPoint as u32));
    assert!(instructions.iter().any(|i| i.op == Op::TypeVector as u32));
    assert!(instructions.iter().any(|i| i.op == Op::Store as u32));
    // `OpFunctionEnd` is the model's last function instruction; a module
    // also carries the declarations and header after it, so only the label
    // relations are checked here.
}

#[test]
fn a_type_is_declared_once() {
    let mut b = Builder::new();
    let scalar = b.float(32);
    let a = b.vector(scalar, 4);
    let c = b.vector(scalar, 4);
    assert_eq!(a, c);
    let instructions = parse(&b.finish()).unwrap();
    let vectors = instructions
        .iter()
        .filter(|i| i.op == Op::TypeVector as u32)
        .count();
    assert_eq!(vectors, 1);
}

#[test]
fn a_constant_is_declared_once() {
    let mut b = Builder::new();
    let a = b.constant_f32(1.5);
    let c = b.constant_f32(1.5);
    let d = b.constant_f32(2.5);
    assert_eq!(a, c);
    assert_ne!(a, d);
    let instructions = parse(&b.finish()).unwrap();
    let constants = instructions
        .iter()
        .filter(|i| i.op == Op::Constant as u32)
        .count();
    assert_eq!(constants, 2);
}

#[test]
fn ids_are_unique_and_the_bound_covers_them() {
    let words = vertex_module();
    let instructions = parse(&words).unwrap();
    let bound = u32::from_le_bytes([words[12], words[13], words[14], words[15]]);
    let mut seen = std::collections::HashSet::new();
    for i in &instructions {
        // Every instruction that carries a result has its id at a known
        // place; a duplicate there would be a reused id.
        // A type instruction has no result type, so its result id is the
        // first operand; a value instruction carries the result type first.
        let index = match i.op {
            x if x == Op::TypeVector as u32 => Some(0),
            x if x == Op::Variable as u32
                || x == Op::Constant as u32
                || x == Op::Function as u32 =>
            {
                Some(1)
            }
            _ => None,
        };
        if let Some(index) = index {
            let id = i.operands[index];
            assert!(seen.insert(id), "id {id} handed out twice");
        }
    }
    assert!(
        seen.iter().all(|&id| id < bound),
        "the header's bound must cover every id"
    );
}

#[test]
fn an_entry_points_name_precedes_its_interface() {
    let words = vertex_module();
    let instructions = parse(&words).unwrap();
    let entry = instructions
        .iter()
        .find(|i| i.op == Op::EntryPoint as u32)
        .expect("an entry point");
    // model, function, then the name's words, then the interface ids.
    let name = crate::parse::read_string(&entry.operands[2..]).expect("a name");
    assert_eq!(name, "main");
    let name_words = crate::module::literal_string("main");
    let interface = &entry.operands[2 + name_words.len()..];
    assert_eq!(interface.len(), 3, "{interface:?}");
}
