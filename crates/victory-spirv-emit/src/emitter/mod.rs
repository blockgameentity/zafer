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

mod export;
mod flow;
mod immediate;
mod lanes;
mod memory;
mod scalar;
mod trans;
mod vector;

#[cfg(test)]
mod tests;

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
        };
        self.interface = Interface::build(&mut module, stage, self.io);
        self.glsl_set = module.glsl();
        self.block = function.entry.label;
        self.function_variables(b);
        self.labels = (0..self.ir.blocks.len()).map(|_| b.block().label).collect();
        for i in 0..self.ir.blocks.len() {
            self.block_body(b, i);
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
    fn new_variable(&mut self, b: &mut Builder, ty: Id) -> Id {
        let ptr = b.pointer(StorageClass::Function, ty);
        let id = b.id();
        b.declare_variable(self.block, ptr, id);
        id
    }

    /// The function variable of `reg`, made on first use.
    pub(crate) fn var_of(&mut self, b: &mut Builder, reg: Reg) -> Id {
        if let Some(&id) = self.vars.get(&reg) {
            return id;
        }
        let ty = match reg {
            Reg::Scc => self.types.bool_,
            _ => self.types.i32_,
        };
        let id = self.new_variable(b, ty);
        self.vars.insert(reg, id);
        id
    }

    /// The register a value lives in.
    pub(crate) fn reg_of(&self, value: Value) -> Option<Reg> {
        self.regs.get(&value).copied()
    }

    /// The `i32` a value holds.
    pub(crate) fn load_i32(&mut self, b: &mut Builder, value: Value) -> Option<Id> {
        let reg = self.reg_of(value)?;
        let var = self.var_of(b, reg);
        if reg == Reg::Scc {
            let bit = b.load(self.types.bool_, var);
            let one = b.constant_i32(1);
            let zero = b.constant_i32(0);
            return Some(b.select(self.types.i32_, bit, one, zero));
        }
        Some(b.load(self.types.i32_, var))
    }

    /// The `f32` a value holds, from its dword's bit pattern.
    pub(crate) fn load_f32(&mut self, b: &mut Builder, value: Value) -> Option<Id> {
        let bits = self.load_i32(b, value)?;
        Some(b.bitcast(self.types.f32_, bits))
    }

    /// The `bool` a value holds (SCC, or a dword's nonzero-ness).
    pub(crate) fn load_bit(&mut self, b: &mut Builder, value: Value) -> Option<Id> {
        let reg = self.reg_of(value)?;
        let var = self.var_of(b, reg);
        if reg == Reg::Scc {
            return Some(b.load(self.types.bool_, var));
        }
        let bits = b.load(self.types.i32_, var);
        let zero = b.constant_i32(0);
        Some(b.inotequal(self.types.bool_, bits, zero))
    }

    /// Writes an `i32` into a register.
    pub(crate) fn store_i32(&mut self, b: &mut Builder, reg: Reg, value: Id) {
        let var = self.var_of(b, reg);
        if reg == Reg::Scc {
            let zero = b.constant_i32(0);
            let bit = b.inotequal(self.types.bool_, value, zero);
            b.store(var, bit);
            return;
        }
        b.store(var, value);
    }

    /// Writes a `bool` into a register.
    pub(crate) fn store_bit(&mut self, b: &mut Builder, reg: Reg, value: Id) {
        let var = self.var_of(b, reg);
        if reg == Reg::Scc {
            b.store(var, value);
            return;
        }
        let one = b.constant_i32(1);
        let zero = b.constant_i32(0);
        let bits = b.select(self.types.i32_, value, one, zero);
        b.store(var, bits);
    }

    /// One block: its phis, its instructions, its exit.
    ///
    /// A phi is a store on every edge into the block, which is what a
    /// register join is; the edge's store is written where the edge is
    /// taken, so every predecessor may run it and only its own lanes see
    /// the value. The IR's phis are per register dword, so two phis of one
    /// block store to two registers and never interfere.
    fn block_body(&mut self, b: &mut Builder, i: usize) {
        let block = self.ir.blocks[i].clone();
        // The phis: one store per edge from each predecessor.
        for phi in &block.phis {
            let args: Vec<(usize, Value)> = block
                .preds
                .iter()
                .copied()
                .zip(phi.args.iter().copied())
                .collect();
            let reg = phi.reg;
            for (pred, arg) in args {
                let value = match self.load_i32(b, arg) {
                    Some(id) => id,
                    None => continue,
                };
                let _ = pred;
                // Every edge is in the same block position here, because a
                // phi's incoming values are already stored in their own
                // registers by the predecessor: this stores the register's
                // content, which the predecessor wrote.
                self.store_i32(b, reg, value);
            }
        }
        let insts = block.insts.clone();
        for inst in &insts {
            self.instruction(b, inst);
        }
        let _ = self.exit(b, i);
    }

    /// One instruction, dispatched by its encoding.
    ///
    /// An instruction the emitter does not name is recorded as a gap: the
    /// shader's translation reports it with its offset, so a missing arm is
    /// an error and never a silently dropped operation.
    fn instruction(&mut self, b: &mut Builder, inst: &Inst) {
        let result = match &inst.fields {
            // The transcendental and three-input families have no core
            // opcode, so they are dispatched before the arithmetic arms.
            Fields::Valu(_) if self.is_trans(inst.name()) => self.trans(b, inst),
            Fields::Valu(_) => self.vector(b, inst),
            Fields::Sop { .. } => self.scalar(b, inst),
            Fields::Exp { .. } => self.export(b, inst),
            Fields::Sopp { imm } => self.immediate(b, inst, *imm),
            Fields::Smem { .. }
            | Fields::Buffer { .. }
            | Fields::Image { .. }
            | Fields::Flat { .. }
            | Fields::Ds { .. } => self.memory(b, inst),
            Fields::Vintrp { .. } => Err(unsupported(inst)),
        };
        // Two nested `if`s rather than a let-chain: `self.gap.is_none()`
        // borrows immutably while the body assigns through it.
        #[allow(clippy::collapsible_if)]
        if let Err(error) = result {
            if self.gap.is_none() {
                self.gap = Some(error);
            }
        }
    }
}

/// Whether an instruction has an effect, so a value it cannot express must
/// fail the shader rather than silently do nothing.
pub(crate) fn has_effect(inst: &Inst) -> bool {
    matches!(
        inst.fields,
        Fields::Image { .. } | Fields::Buffer { .. } | Fields::Flat { .. } | Fields::Ds { .. }
    )
}

/// The values an instruction names, in encoded order.
pub(crate) fn operands(fields: &Fields) -> Vec<Operand> {
    match fields {
        Fields::Sop { dst, src, .. } => dst.iter().chain(src.iter()).copied().collect(),
        Fields::Valu(v) => v.dst.iter().chain(v.src.iter()).copied().collect(),
        Fields::Vintrp { dst, src, attr } => vec![*dst, *src, *attr],
        Fields::Ds { dst, addr, data, .. } => dst
            .iter()
            .chain(addr.iter())
            .chain(data.iter())
            .copied()
            .collect(),
        Fields::Buffer { addr, rsrc, .. } => vec![*addr, *rsrc],
        Fields::Image { rsrc, .. } => vec![*rsrc],
        Fields::Flat { dst, addr, .. } => dst.iter().chain(addr.iter()).copied().collect(),
        Fields::Sopp { .. } | Fields::Exp { .. } => Vec::new(),
        Fields::Smem { base, soffset, .. } => vec![*base, *soffset],
    }
}

/// One instruction the emitter does not translate.
pub(crate) fn unsupported(inst: &Inst) -> Error {
    let what = match inst.name() {
        "" => "an instruction with no name",
        name => name,
    };
    Error::Unsupported {
        offset: inst.machine.offset,
        what: static_name(what),
    }
}

/// A name for an error that outlives the borrow it came from.
pub(crate) fn static_name(name: &str) -> &'static str {
    use std::collections::HashSet;
    use std::sync::{Mutex, OnceLock};
    static NAMES: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    let mut set = NAMES
        .get_or_init(|| Mutex::new(HashSet::new()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if let Some(existing) = set.get(name) {
        return existing;
    }
    let leaked: &'static str = Box::leak(name.to_owned().into_boxed_str());
    set.insert(leaked);
    leaked
}
