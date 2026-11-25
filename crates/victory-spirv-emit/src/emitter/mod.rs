//! IR to SPIR-V, one machine instruction at a time.
//!
//! A register dword is a SPIR-V function variable of type `i32`, and SCC one
//! of type `bool`. A read is `OpLoad` and a write `OpStore`. That model is
//! exactly the IR's: a register is a place whose content changes, so the
//! emitter never has to reproduce the IR's SSA form (phis, block order,
//! dominance) in SPIR-V's, which is where a translator of this kind goes
//! wrong. A load after a store sees the stored value because the function
//! variable is one location; a phi becomes a store on each incoming edge.
//!
//! Float values exist only between two instructions: an `f32` register holds
//! a bit pattern, so an `f32` operation bitcasts its operands in and its
//! result out. That keeps one type for every dword, as the hardware has.
//!
//! Lane masks stay booleans: a block's guard is a bool tested before a
//! vector instruction's effects, and a branch on a lane mask is a branch on
//! that bool. EXEC is never read as data.
//!
//! An instruction the emitter does not name is [`Error::Unsupported`]: a
//! shader either translates or says which instruction stopped it, so a gap
//! can never render wrongly in silence.

mod body;
mod export;
mod flow;
mod immediate;
mod interp;
mod lanes;
mod machine;
mod memory;
mod scalar;
mod registers;
pub(crate) mod structure;
mod trans;
mod vector;

pub(crate) use body::{has_effect, operands, static_name, unsupported};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod blocks_probe;

use std::collections::HashMap;

use victory_shader::emit::UserData;
use victory_shader::gfx10::{Fields, Operand};
use victory_shader::io::Io;
use victory_shader::ir::{Inst, Program, Reg, Value};
use victory_shader::native::NativeProgram;
use victory_shader::resource::Resources;
use victory_shader::structure::Structured;
use victory_spirv::builder::{Builder, Function};
use victory_spirv::enums::StorageClass;
use victory_spirv::module::Id;

use crate::Error;
use crate::interface::Interface;
use crate::module::{Module, Stage};

/// The types every register and instruction uses.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Types {
    pub i32_: Id,
    pub u32_: Id,
    pub f32_: Id,
    pub bool_: Id,
    /// `struct { u32, u32 }`, the result of the extended multiplies.
    pub u32_pair: Id,
    /// `vecN<f32>`, for an output built from several channels.
    pub f32_2: Id,
    pub f32_3: Id,
    pub f32_4: Id,
}

/// A shader's translation in progress.
pub(crate) struct Emitter<'a> {
    pub(crate) program: &'a NativeProgram,
    pub(crate) ir: &'a Program,
    pub(crate) st: &'a Structured,
    pub(crate) io: &'a Io,
    pub(crate) res: &'a Resources,
    pub(crate) types: Types,
    /// The function variable of each register dword and of SCC.
    pub(crate) vars: HashMap<Reg, Id>,
    /// The register each value was written to, so a read finds its variable.
    pub(crate) regs: HashMap<Value, Reg>,
    /// The label of each block.
    pub(crate) labels: Vec<Id>,
    /// Where the shader's instructions go.
    pub(crate) block: Id,
    pub(crate) interface: Interface,
    pub(crate) user_data: Vec<UserData>,
    pub(crate) samplers: Vec<[u32; 4]>,
    pub(crate) root_cbvs: Vec<u8>,
    pub(crate) root_srvs: Vec<u8>,
    /// The SPIR-V boolean each of structurisation's diagrams denotes.
    pub(crate) diagrams: lanes::Diagrams,
    /// The `GLSL.std.450` set, for the instructions core SPIR-V lacks.
    pub(crate) glsl_set: Id,
    /// The first instruction the emitter could not translate.
    pub gap: Option<Error>,
    /// Whether the instruction just written terminated the function
    /// (`s_endpgm`): the block's own exit must not be written then.
    returned: bool,
    /// Where in the function body a variable made after the first block
    /// belongs: right after the ones the entry block declared.
    decls_at: Option<usize>,
    /// The merges every terminator needs, and the order to write blocks in.
    merges: Vec<structure::Merge>,
    loops: HashMap<usize, usize>,
    order: Vec<usize>,
    /// Headers whose merge is a block of their own, by header.
    trampolines: HashMap<usize, usize>,
    /// The label of each such block, by header.
    trampoline_labels: HashMap<usize, Id>,
    /// The per-lane state machine, when the program's CFG is not something
    /// SPIR-V can nest.
    machine: Option<machine::Machine>,
    /// Whether to emit the state machine rather than nested control flow.
    pub(crate) use_machine: bool,
    /// The instruction being emitted, as the tracer named it: a memory
    /// access's address is looked up by this.
    pub(crate) site: victory_shader::resource::Site,
    /// The block holding the root parameters' addresses.
    pub(crate) parameters: Option<Id>,
    /// The workgroup's shared memory, made when an LDS access needs it.
    pub(crate) shared: Option<Id>,
    /// The lane's index within its wave, loaded once.
    pub(crate) subgroup_lane: Option<Id>,
}

impl<'a> Emitter<'a> {
    /// A translation of `program`, whose earlier stages already ran.
    pub(crate) fn new(
        program: &'a NativeProgram,
        ir: &'a Program,
        st: &'a Structured,
        io: &'a Io,
        res: &'a Resources,
    ) -> Emitter<'a> {
        let mut regs = HashMap::new();
        for (reg, value) in &ir.inputs {
            regs.insert(*value, *reg);
        }
        for block in &ir.blocks {
            for phi in &block.phis {
                regs.insert(phi.value, phi.reg);
                for arg in &phi.args {
                    regs.entry(*arg).or_insert(phi.reg);
                }
            }
            for inst in &block.insts {
                for (reg, value) in &inst.writes {
                    regs.insert(*value, *reg);
                }
                // A register a block *reads* needs its variable too: every
                // `OpVariable` in a function must be among its first
                // instructions, so none may be created lazily mid-block
                // (`spirv-val`: "all OpVariable instructions in a function
                // must be the first instructions in the first block").
                for (reg, value) in &inst.reads {
                    regs.entry(*value).or_insert(*reg);
                }
            }
        }
        Emitter {
            program,
            ir,
            st,
            io,
            res,
            types: Types {
                i32_: Id(0),
                u32_: Id(0),
                f32_: Id(0),
                bool_: Id(0),
                u32_pair: Id(0),
                f32_2: Id(0),
                f32_3: Id(0),
                f32_4: Id(0),
            },
            vars: HashMap::new(),
            regs,
            labels: Vec::new(),
            block: Id(0),
            interface: Interface::default(),
            user_data: Vec::new(),
            samplers: Vec::new(),
            root_cbvs: Vec::new(),
            root_srvs: Vec::new(),
            diagrams: lanes::Diagrams::new(),
            glsl_set: Id(0),
            gap: None,
            returned: false,
            decls_at: None,
            merges: Vec::new(),
            loops: HashMap::new(),
            order: Vec::new(),
            trampolines: HashMap::new(),
            trampoline_labels: HashMap::new(),
            machine: None,
            use_machine: false,
            site: (0, 0),
            parameters: None,
            shared: None,
            subgroup_lane: None,
        }
    }

    /// The stage's module model.
    pub(crate) fn stage(&self) -> Stage {
        match self.program.stage {
            victory_shader::native::HwStage::Ps => Stage::Pixel,
            victory_shader::native::HwStage::Cs => Stage::Compute,
            _ => Stage::Vertex,
        }
    }

    /// Writes the whole shader into `function`.
    pub(crate) fn run(&mut self, b: &mut Builder, function: Function) {
        let stage = self.stage();
        let mut module = Module::new(stage);
        self.types = Types {
            i32_: module.i32_ty,
            u32_: module.u32_ty,
            f32_: module.f32_ty,
            bool_: module.bool_ty,
            // `OpUMulExtended`'s result is a struct of the two halves.
            u32_pair: b.struct_of(&[module.u32_ty, module.u32_ty]),
            f32_2: module.vec(module.f32_ty, 2),
            f32_3: module.vec(module.f32_ty, 3),
            f32_4: module.vec(module.f32_ty, 4),
        };
        self.interface = Interface::build(&mut module, stage, self.io);
        // One block dword per root parameter: the value *is* the address of
        // what the parameter describes, which is what makes a console
        // shader's root signature one host descriptor.
        let slots = self.program.layout.len();
        if let Ok(slots) = u32::try_from(slots) {
            if slots > 0 {
                self.parameters = Some(module.parameters(slots).id);
            }
        }
        self.glsl_set = module.glsl();
        self.block = function.entry.label;
        self.function_variables(b);
        // Every block's id is needed before any body is written (a branch
        // refers forward), but a function's blocks must appear in order with
        // each label immediately followed by its own body. So the ids are
        // reserved here and the labels written as each body starts; writing
        // all the labels first (what this used to do) put every label after
        // the preceding body, which `spirv-val` reports as a block that does
        // not end with a branch.
        //
        // The first block is the entry block `Builder::function` already
        // labelled, so it reuses that label: reserving one for it too left
        // an empty, unterminated block right after `OpFunction`.
        // A function's first block may not be a branch target ("first block
        // of function is targeted by block <n>"), and the console's entry
        // block is one whenever the program loops back to its start. So the
        // first block only branches to block 0's own label, which every
        // edge can then target.
        self.labels = (0..self.ir.blocks.len())
            .map(|_| b.reserve_label().label)
            .collect();
        // Later `OpVariable`s go here: after the ones just declared, before
        // the branch out of the entry block.
        self.decls_at = Some(b.function_len());
        if self.use_machine {
            // The machine's head: the entry block branches into the loop and
            // the shader's blocks are dispatched from it.
            let machine = self.machine_head(b);
            self.machine = Some(machine);
        } else {
            let first = b.at(self.labels[0]);
            b.branch(first);
        }
        // The merges and the order the blocks are written in: SPIR-V needs a
        // dominance-respecting order and a merge instruction per conditional
        // branch and back edge, neither of which the console's CFG states.
        let cfg = structure::Structured::of(&self.ir.blocks);
        self.merges = cfg.merges;
        self.loops = cfg.loops;
        self.order = cfg.order;
        // Each header that needs a merge block of its own gets one, reserved
        // here so a branch anywhere can name it.
        for (header, join) in &cfg.trampolines {
            self.trampolines.insert(*header, *join);
            let label = b.reserve_label().label;
            self.trampoline_labels.insert(*header, label);
        }
        // The entry block always comes first (it is the label `OpFunction`
        // wrote), so the order starts there and every other block follows
        // in reverse postorder.
        let order = self.order.clone();
        for i in order {
            let label = self.labels[i];
            b.label(label);
            self.returned = false;
            // A condition's SPIR-V value belongs to the block that computed
            // it: reusing the memoised id in another block is a use its
            // definition does not dominate ("ID defined in block N does not
            // dominate its use in block M"). The atoms themselves are loads
            // of registers, which every block can do for itself.
            self.diagrams.clear();
            self.block_body(b, i);
        }
        // A merge block of its own: empty but for a branch to the join, so
        // the join is one header's merge only.
        let mut pairs: Vec<(usize, usize)> = self.trampolines.iter().map(|(h, j)| (*h, *j)).collect();
        pairs.sort_unstable();
        for (header, join) in pairs {
            let label = self.trampoline_labels[&header];
            b.label(label);
            let target = b.at(self.labels[join]);
            b.branch(target);
        }
        // The strided state machine's tail closes the loop and returns.
        if let Some(machine) = self.machine {
            self.machine_tail(b, machine);
        }
        // The last block's terminator is already written; a return after it
        // is unreachable but keeps a module valid when a block's exit was a
        // branch into a label that no block owns.
        b.end_function(function);
    }

    /// A function variable per register dword, made once.
    fn function_variables(&mut self, b: &mut Builder) {
        let mut regs: Vec<Reg> = self.regs.values().copied().collect();
        regs.sort_unstable();
        regs.dedup();
        for reg in regs {
            let ty = match reg {
                Reg::Scc => self.types.bool_,
                _ => self.types.i32_,
            };
            let var = self.new_variable(b, ty);
            self.vars.insert(reg, var);
        }
    }

    /// A function-scope variable, declared in the entry block.
    ///
    /// SPIR-V requires every `OpVariable` of a function to be among the
    /// first instructions of its first block. The registers known when the
    /// function starts are declared there as the body begins; a register
    /// first named by `var_of` later (an operand the IR did not list among
    /// the block's reads) is inserted at the same point rather than
    /// appended where it was asked for, which is what `spirv-val` reports
    /// as "all OpVariable instructions in a function must be the first
    /// instructions in the first block".
    fn new_variable(&mut self, b: &mut Builder, ty: Id) -> Id {
        let ptr = b.pointer(StorageClass::Function, ty);
        let id = b.id();
        match self.decls_at {
            Some(at) => {
                let words = b.variable_words(ptr, id, StorageClass::Function);
                b.insert_function_words(at, &words);
            }
            None => {
                b.declare_variable(self.block, ptr, id);
            }
        }
        id
    }
}
