//! The per-lane state machine, for control flow SPIR-V cannot nest.
//!
//! A console shader is compiled without the constraints SPIR-V puts on
//! control flow: branches are predicated jumps, a block may jump into the
//! middle of a region, and the graph need not be reducible at all.
//! `spirv-val` rejects a module that copies such a graph with "selection
//! must be structured", "back-edges can only be formed between a block and a
//! loop header" or "block exits the loop, but not via a structured exit".
//!
//! So the whole program becomes one loop with a dispatch:
//!
//! ```text
//!   entry      variables; pc = 0; branch -> header
//!   header     OpLoopMerge after continue ; branch -> dispatch
//!   dispatch   OpSwitch pc plain { 0: b0, 1: b1, ... }  (merged into plain)
//!   continue   branch -> header          ; where a finished block jumps
//!   b0..bn     the shader's own blocks, each ending in
//!              "store pc; branch continue"
//!   plain      branch -> after
//!   after      OpReturn
//! ```
//!
//! The shader's blocks sit between the loop's head and its merge, so they are
//! reachable from the dispatch and every value they define dominates its uses
//! (a block written after the loop's `OpReturn` would be unreachable, and
//! `spirv-val` says "ID defined in block N does not dominate its use in block
//! M").
//!
//! Each lane has its own `pc`, so divergence is exactly what it is on the
//! console, and the module is structured by construction: one loop, one
//! switch, one exit. The cost is a switch per block rather than a direct
//! branch, which is why the structured shape is tried first.

use victory_spirv::builder::{Block, Builder};
use victory_spirv::module::Id;

use super::Emitter;

/// The shape of the state machine.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Machine {
    /// The lane's program counter.
    pub pc: Id,
    /// Where a block branches when it is done: back to the header.
    pub continue_at: Id,
    /// The loop's merge, written after the shader's blocks.
    pub after: Id,
    /// The switch's merge, written after the shader's blocks.
    pub plain: Id,
}

impl<'a> Emitter<'a> {
    /// The loop's head: the counter, the entry branch, the loop, and the
    /// dispatch. Everything up to the shader's own first block.
    pub(crate) fn machine_head(&mut self, b: &mut Builder) -> Machine {
        let header = b.reserve_label().label;
        let dispatch = b.reserve_label().label;
        let continue_at = b.reserve_label().label;
        let plain = b.reserve_label().label;
        let after = b.reserve_label().label;
        // The counter is a function variable like a register's, so every lane
        // has its own; it is declared with the others, at the top.
        let pc = self.new_variable(b, self.types.i32_);
        let zero = b.constant_i32(0);
        b.store(pc, zero);
        let block = b.at(header);
        b.branch(block);
        // The one loop: everything the shader does is inside it.
        b.label(header);
        let merge = b.at(after);
        let continued = b.at(continue_at);
        b.loop_merge(merge, continued, 0);
        let block = b.at(dispatch);
        b.branch(block);
        // The dispatch: one case per block of the program. The switch's
        // merge is a block of its own (a block may be one header's merge),
        // which branches to the loop's merge.
        b.label(dispatch);
        let counter = b.load(self.types.i32_, pc);
        let plain_block = b.at(plain);
        b.selection_merge(plain_block, 0);
        let labels: Vec<Id> = self.labels.clone();
        let targets: Vec<(u32, Block)> = labels
            .iter()
            .enumerate()
            .map(|(n, label)| (n as u32, b.at(*label)))
            .collect();
        b.switch(counter, plain_block, &targets);
        // The continue block: the loop's one back edge.
        b.label(continue_at);
        let block = b.at(header);
        b.branch(block);
        Machine {
            pc,
            continue_at,
            after,
            plain,
        }
    }

    /// The loop's tail, after the shader's blocks: the switch's merge, the
    /// loop's merge, and the return.
    pub(crate) fn machine_tail(&mut self, b: &mut Builder, machine: Machine) {
        let _ = self;
        b.label(machine.plain);
        let block = b.at(machine.after);
        b.branch(block);
        b.label(machine.after);
        b.return_void();
    }
}
