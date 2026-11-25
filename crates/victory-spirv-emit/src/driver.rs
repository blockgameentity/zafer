//! Running the three translation stages and emitting one module.
//!
//! `victory_shader` already turns a container into a bare `Program`, a
//! `Structured` form of it, its resources and its interface. This module
//! runs those stages in the order the emitter needs (structurisation first,
//! because tracing per-lane descriptors reads it) and hands the four to the
//! emitter.

use victory_shader::native::{HwStage, NativeProgram, StageIo};
use victory_shader::{io, ir, native, resource, structure};

use crate::Error;
use crate::module::{Module, Stage};

/// A translated shader: the module plus what a pipeline must bind for it.
#[derive(Debug, Clone)]
pub struct Translation {
    /// The SPIR-V module, ready for `vkCreateShaderModule`.
    pub spirv: Vec<u8>,
    /// Which stage this is.
    pub stage: HwStage,
    /// The user-data dwords the shader reads, with the root parameter that
    /// fills each (the push-constant block's layout).
    pub user_data: Vec<victory_shader::emit::UserData>,
    /// The immediate samplers, `s<n>` at position `n`.
    pub samplers: Vec<[u32; 4]>,
    /// Root CBVs the shader reads, by root parameter index.
    pub root_cbvs: Vec<u8>,
    /// Root SRVs (raw buffers) the shader reads.
    pub root_srvs: Vec<u8>,
}

impl Translation {
    /// The module's stage as the emitter's own.
    pub fn stage_model(&self) -> Stage {
        match self.stage {
            HwStage::Ps => Stage::Pixel,
            HwStage::Cs => Stage::Compute,
            _ => Stage::Vertex,
        }
    }
}

/// Translates the first vertex, pixel, compute or mesh program of
/// `container` to SPIR-V.
pub fn translate(container: &[u8]) -> Result<Translation, Error> {
    let c = victory_shader::Container::parse(container)?;
    let dxil = c.part(victory_shader::FourCC::DXIL).ok_or(
        victory_shader::ShaderError::Malformed("no DXIL part"),
    )?;
    let program: NativeProgram = native::native_programs(dxil.data)?
        .into_iter()
        .find(|p| matches!(p.stage, HwStage::Vs | HwStage::Ps | HwStage::Cs | HwStage::Ms))
        .ok_or(victory_shader::ShaderError::Unsupported {
            offset: 0,
            what: "a container without a vertex, pixel, compute or mesh program",
        })?;
    translate_program(&program)
}

/// Translates one native program.
pub fn translate_program(program: &NativeProgram) -> Result<Translation, Error> {
    let p = ir::build(&program.patched_code(), program.wave64, program.vgprs)?;
    // Structurisation first: tracing per-lane descriptors needs it.
    let st = structure::structure(&p);
    let res = resource::track(&p, &program.layout, program.stage, st.as_ref().ok())?;
    let io = io::raise(&p, program)?;
    let st = st?;
    let out = emit(program, &p, &st, &io, &res);
    // One line per program: a title that shows no image is usually a shader
    // that did not translate, and the *reason* is what a reader needs.
    match &out {
        Ok(t) => tracing::debug!(
            stage = ?program.stage,
            blocks = p.blocks.len(),
            words = t.spirv.len() / 4,
            "translated to SPIR-V"
        ),
        Err(e) => tracing::warn!(
            stage = ?program.stage,
            code = program.code.len(),
            "not translated: {e}"
        ),
    }
    out
}

/// Emits a program whose earlier stages already ran.
pub fn emit(
    program: &NativeProgram,
    p: &ir::Program,
    st: &structure::Structured,
    io: &io::Io,
    res: &resource::Resources,
) -> Result<Translation, Error> {
    let stage = match program.stage {
        HwStage::Ps => Stage::Pixel,
        HwStage::Cs => Stage::Compute,
        _ => Stage::Vertex,
    };
    let mut module = Module::new(stage);
    // A compute program's workgroup is the console's thread group: the
    // thread ids, the lane masks and the shared memory are all laid out in
    // a group of that shape, so Vulkan gets that shape and not a guess.
    if let StageIo::Cs(cs) = &program.io {
        module.set_local_size(cs.threads[0], cs.threads[1], cs.threads[2]);
    }
    let mut emitter = crate::emitter::Emitter::new(program, p, st, io, res);
    let spirv = module.finish("main", |b, function| {
        emitter.run(b, function);
    });
    // A module nothing has checked is only known to be a module; the
    // validator is what makes "emitted" mean "Vulkan will take it".
    if let Some(why) = crate::validate::validate(&spirv).reason() {
        return Err(Error::Invalid(why.to_owned()));
    }
    Ok(Translation {
        spirv,
        stage: program.stage,
        user_data: emitter.user_data.clone(),
        samplers: emitter.samplers.clone(),
        root_cbvs: emitter.root_cbvs.clone(),
        root_srvs: emitter.root_srvs.clone(),
    })
}
