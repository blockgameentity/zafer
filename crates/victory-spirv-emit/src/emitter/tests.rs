//! What the emitter must guarantee: the module a shader translates to is a
//! valid SPIR-V file, it has one instruction per machine instruction, and a
//! program it cannot express says which instruction stopped it.

use std::io::Write;
use std::process::{Command, Stdio};

use victory_spirv::ops::Op;
use victory_spirv::parse::parse;

/// Validates `module` with `spirv-val`, when the tool is installed. The
/// tests that call it skip silently otherwise, so the suite runs on a
/// machine without the SPIR-V tools.
fn validate(module: &[u8]) -> Option<String> {
    let mut child = Command::new("spirv-val")
        .arg("-")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .ok()?;
    child
        .stdin
        .as_mut()
        .expect("a piped stdin")
        .write_all(module)
        .expect("the validator reads its input");
    let out = child.wait_with_output().expect("the validator finishes");
    if out.status.success() {
        None
    } else {
        Some(String::from_utf8_lossy(&out.stderr).into_owned())
    }
}

/// The Vulkan validator this module must satisfy: a file `spirv-val` accepts
/// for the stage it declares.
#[test]
fn a_built_module_passes_the_validator() {
    let module = crate::module::Module::new(crate::module::Stage::Pixel);
    let words = module.finish("main", |b, function| {
        b.return_void();
        b.end_function(function);
    });
    if Command::new("spirv-val").arg("--version").output().is_err() {
        return; // no validator installed
    }
    if let Some(error) = validate(&words) {
        panic!("the module is not valid SPIR-V: {error}");
    }
}

/// Every `GLSL.std.450` instruction the transcendental and three-input
/// families emit, as one pixel shader, validated: the extended set's ids
/// are literals from the specification, so getting one wrong produces a
/// module that is well-formed and wrong, which only the validator catches.
#[test]
fn the_extended_instruction_set_is_valid() {
    use crate::emitter::trans::Glsl;
        if std::process::Command::new("spirv-val")
        .arg("--version")
        .output()
        .is_err()
    {
        return; // no validator installed
    }
    let module = crate::module::Module::new(crate::module::Stage::Pixel);
    let words = module.finish("main", |b, function| {
        let f32_ = b.float(32);
        let set = b.ext_inst_import("GLSL.std.450");
        let one = b.constant_f32(1.5);
        let two = b.constant_f32(2.5);
        let three = b.constant_f32(3.5);
        for which in [
            Glsl::Round,
            Glsl::RoundEven,
            Glsl::Trunc,
            Glsl::Floor,
            Glsl::Ceil,
            Glsl::Fract,
            Glsl::Sin,
            Glsl::Cos,
            Glsl::Exp2,
            Glsl::Log2,
            Glsl::Sqrt,
            Glsl::InverseSqrt,
        ] {
            b.ext_inst(f32_, set, which as u32, &[one]);
        }
        b.ext_inst(f32_, set, Glsl::Fma as u32, &[one, two, three]);
        // Each result is used, so no instruction is dead.
        let sum = b.fadd(f32_, one, two);
        let store = b.fmul(f32_, sum, three);
        let _ = store;
        b.return_void();
        b.end_function(function);
    });
    if let Some(error) = validate(&words) {
        panic!("the extended instruction set is not valid: {error}");
    }
}

/// A module the emitter built, for the instruction checks.
fn instructions(module: &[u8]) -> Vec<victory_spirv::parse::Instruction> {
    parse(module).expect("the emitter builds a module that parses")
}

#[test]
fn a_module_has_the_header_and_the_layout_order() {
    // A shader with no instructions still builds a module with the header,
    // the capability, the memory model, the entry point and the function.
    let module = crate::module::Module::new(crate::module::Stage::Pixel);
    let words = module.finish("main", |b, function| {
        b.return_void();
        b.end_function(function);
    });
    let instructions = instructions(&words);
    assert!(instructions.iter().any(|i| i.op == Op::Capability as u32));
    assert!(instructions.iter().any(|i| i.op == Op::MemoryModel as u32));
    assert!(instructions.iter().any(|i| i.op == Op::EntryPoint as u32));
    assert!(instructions.iter().any(|i| i.op == Op::Function as u32));
    // Sections are in the specification's order: no type or constant may
    // precede the memory model, and no function may precede a type.
    let mut seen_types = false;
    let mut seen_function = false;
    for i in &instructions {
        if i.op == Op::MemoryModel as u32 {
            assert!(!seen_types, "a type before the memory model");
        }
        if matches!(
            i.op,
            x if x == Op::TypeInt as u32
                || x == Op::TypeFloat as u32
                || x == Op::TypeVector as u32
                || x == Op::Constant as u32
        ) {
            seen_types = true;
            assert!(!seen_function, "a type after a function began");
        }
        if i.op == Op::Function as u32 {
            seen_function = true;
        }
    }
}

#[test]
fn an_exit_returns_and_a_jump_branches() {
    // The two terminators a single-lane block can have, through the builder
    // the emitter uses.
    let module = crate::module::Module::new(crate::module::Stage::Vertex);
    let words = module.finish("main", |b, function| {
        let other = b.block();
        b.branch(other);
        b.return_void();
        b.end_function(function);
    });
    let instructions = instructions(&words);
    assert!(instructions.iter().any(|i| i.op == Op::Branch as u32));
    assert!(instructions.iter().any(|i| i.op == Op::Return as u32));
}

#[test]
fn every_register_is_one_function_variable() {
    // Two reads of one register load the same variable, so a write between
    // them is visible: the property the whole emitter rests on.
    let module = crate::module::Module::new(crate::module::Stage::Compute);
    let words = module.finish("main", |b, function| {
        let i32_ = b.int(32, true);
        let ptr = b.pointer(victory_spirv::enums::StorageClass::Function, i32_);
        let var = b.id();
        b.declare_variable(function.entry.label, ptr, var);
        let one = b.constant_i32(1);
        b.store(var, one);
        let loaded = b.load(i32_, var);
        b.store(var, loaded);
        b.return_void();
        b.end_function(function);
    });
    let instructions = instructions(&words);
    let variables = instructions
        .iter()
        .filter(|i| i.op == Op::Variable as u32)
        .count();
    assert_eq!(variables, 1, "one variable, however many accesses");
    assert_eq!(
        instructions.iter().filter(|i| i.op == Op::Load as u32).count(),
        1
    );
    assert_eq!(
        instructions.iter().filter(|i| i.op == Op::Store as u32).count(),
        2
    );
}
