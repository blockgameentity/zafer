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
    // Structured first: it is what Vulkan likes best and what most programs
    // take. A module nothing has checked is only known to be a module; the
    // validator is what makes "emitted" mean "Vulkan will take it".
    let (spirv, emitter) = assemble(program, p, st, io, res, false);
    // An instruction the emitter could not express makes the module wrong,
    // not merely incomplete: SPIR-V has no "skip this", so a dropped
    // instruction is a shader that renders wrongly with no error anywhere.
    // The gap is reported before the module is even validated.
    if let Some(gap) = &emitter.gap {
        return Err(match gap {
            Error::Unsupported { offset, what } => Error::Unsupported {
                offset: *offset,
                what,
            },
            other => Error::Invalid(other.to_string()),
        });
    }
    match crate::validate::validate(&spirv).reason() {
        None => Ok(Translation {
            spirv,
            stage: program.stage,
            user_data: emitter.user_data,
            samplers: emitter.samplers,
            root_cbvs: emitter.root_cbvs,
            root_srvs: emitter.root_srvs,
        }),
        // The console's CFG is often not something SPIR-V can nest, and no
        // amount of merge analysis makes an irreducible graph legal. The
        // retry emits the same program as a per-lane state machine, which is
        // structured by construction; only a program that fails both is a
        // gap.
        Some(why) => {
            let (spirv, emitter) = assemble(program, p, st, io, res, true);
            match crate::validate::validate(&spirv).reason() {
                None => Ok(Translation {
                    spirv,
                    stage: program.stage,
                    user_data: emitter.user_data,
                    samplers: emitter.samplers,
                    root_cbvs: emitter.root_cbvs,
                    root_srvs: emitter.root_srvs,
                }),
                // The reason reported is the state machine's: it is the
                // shape that decided, so what it cannot express is what
                // there is left to fix. (The structured attempt's message,
                // "selection must be structured", says nothing a reader can
                // act on.)
                Some(second) => {
                    let _ = why;
                    Err(Error::Invalid(second.to_owned()))
                }
            }
        }
    }
}

/// Builds the module of `program`, either nested or as a state machine, and
/// hands back its bytes with the emitter that made it (which knows what the
/// pipeline must bind).
fn assemble<'a>(
    program: &'a NativeProgram,
    p: &'a ir::Program,
    st: &'a structure::Structured,
    io: &'a io::Io,
    res: &'a resource::Resources,
    use_machine: bool,
) -> (Vec<u8>, crate::emitter::Emitter<'a>) {
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
    emitter.use_machine = use_machine;
    let spirv = module.finish("main", |b, function| {
        emitter.run(b, function);
    });
    (spirv, emitter)
}
