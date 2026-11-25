//! Temporarily: does a two-block module validate?

use victory_spirv::builder::Builder;
use victory_spirv::enums::StorageClass;
use victory_spirv::module::Id;

#[test]
fn two_blocks_validate() {
    let module = crate::module::Module::new(crate::module::Stage::Compute);
    let words = module.finish("main", |b: &mut Builder, function| {
        let i32_ = b.int(32, true);
        let ptr = b.pointer(StorageClass::Function, i32_);
        let var = b.id();
        b.declare_variable(function.entry.label, ptr, var);
        let one = b.constant_i32(1);
        b.store(var, one);
        // Reserve both blocks' ids, then write labels and bodies interleaved.
        let a = b.reserve_label();
        let c = b.reserve_label();
        b.return_void();
        b.label(a.label);
        b.return_void();
        b.label(c.label);
        b.return_void();
        let _: Id = a.label;
        b.end_function(function);
    });
    std::fs::write("/tmp/probe.spv", &words).unwrap();
    match crate::validate::validate(&words) {
        crate::validate::Validity::Invalid(why) => panic!("spirv-val: {why}"),
        _ => {}
    }
}
