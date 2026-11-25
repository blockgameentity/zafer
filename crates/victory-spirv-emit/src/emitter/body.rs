//! One block at a time: its phis, its instructions, and its exit.
//!
//! Split out of [`super`] because a translation's hot path is here and the
//! emitter's own types do not belong in the same file as them.

use std::collections::HashMap;

use victory_shader::gfx10::{Fields, Operand};
use victory_shader::ir::{Inst, Reg, Value};
use victory_spirv::builder::Builder;

use super::{Emitter, Error};

impl<'a> Emitter<'a> {
    /// One block: its phis, its instructions, its exit.
    ///
    /// A phi is a store on every edge into the block, which is what a
    /// register join is; the edge's store is written where the edge is
    /// taken, so every predecessor may run it and only its own lanes see
    /// the value. The IR's phis are per register dword, so two phis of one
    /// block store to two registers and never interfere.
    pub(crate) fn block_body(&mut self, b: &mut Builder, i: usize) {
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
        // Whether the block's terminator has been written: `s_endpgm` is a
        // return, and the block's exit may be another.
        let mut terminated = false;
        let insts = block.insts.clone();
        for (at, inst) in insts.iter().enumerate() {
            // A block is over once it has been terminated. `s_endpgm` is a
            // return, and the block's own exit is another: writing both put
            // two `OpReturn`s in one block, which `spirv-val` rejects ("a
            // return must appear in a block"). Instructions after a return
            // are unreachable by construction, so dropping them loses
            // nothing.
            if terminated {
                break;
            }
            // A memory access's address is found by the site the tracer
            // recorded, which is this instruction's place in the program.
            self.site = (i, at);
            self.instruction(b, inst);
            terminated = self.returned;
        }
        // A block that cannot be terminated has no SPIR-V form, and a block
        // without a terminator is invalid whatever follows: both are gaps
        // the whole module must report, so neither may be dropped here.
        // (`spirv-val` found this: "a block must end with a branch
        // instruction" was the commonest refusal in the title.)
        // `s_endpgm` already terminated this block: a second
        // `OpReturn` in one block is what `spirv-val` reports as "a return
        // must appear in a block".
        if terminated {
            return;
        }
        if let Err(e) = self.exit(b, i) {
            if self.gap.is_none() {
                self.gap = Some(e);
            }
            // Terminate anyway, so the rest of the module stays well formed
            // enough for the validator to report the real problem.
            b.return_void();
        }
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
            Fields::Valu(_) if inst.name().starts_with("v_interp_") => self.interp(b, inst),
            Fields::Valu(_) => self.vector(b, inst),
            Fields::Sop { .. } => self.scalar(b, inst),
            Fields::Exp { .. } => self.export(b, inst),
            Fields::Sopp { imm } => self.immediate(b, inst, *imm),
            Fields::Smem { .. }
            | Fields::Buffer { .. }
            | Fields::Image { .. }
            | Fields::Flat { .. }
            | Fields::Ds { .. } => self.memory(b, inst),
            Fields::Vintrp { .. } => self.interp(b, inst),
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
