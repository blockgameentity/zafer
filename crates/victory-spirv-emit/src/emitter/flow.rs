//! Control flow: blocks, branches, and the end of the program.
//!
//! The IR's blocks are already one lane's view of the program: a branch
//! carries the per-lane condition of that edge. Two shapes are emitted:
//!
//! * **Structured** — when the console's CFG happens to be reducible and
//!   every construct has a legal merge ([`super::structure`]), each block is
//!   a SPIR-V block and a branch becomes `OpBranchConditional`.
//! * **Per-lane state machine** — when it is not. A shader compiled for a
//!   console can jump anywhere, and SPIR-V has no goto; the driver emits
//!   structured first and falls back to this shape when `spirv-val` rejects
//!   the module ([`super::machine`]).
//!
//! A block whose guard is a lane condition (the console's per-lane early
//! exits) has the guard applied inside it, not on the edges into it: the IR
//! gives each instruction its guard, and [`super::vector`] tests them.

use victory_shader::ir::Exit;
use victory_shader::structure::LaneExit;
use victory_spirv::builder::Builder;
use victory_spirv::module::Id;

use super::structure::Merge;
use super::Emitter;
use crate::Error;

impl<'a> Emitter<'a> {
    /// The merge instruction block `i`'s terminator needs.
    ///
    /// SPIR-V has a branch on a condition only inside a selection or a loop:
    /// a header without `OpSelectionMerge`/`OpLoopMerge` immediately before
    /// its `OpBranchConditional` is what `spirv-val` reports as "Selection
    /// must be structured". The merges come from the CFG
    /// ([`super::structure`]), and a header whose merge could not be found is
    /// a gap, which sends the program to the state machine instead.
    fn merge(&mut self, b: &mut Builder, i: usize) -> Result<(), Error> {
        // A join another header already merges at needs a block of its own
        // (a block may be one header's merge).
        let own: Option<Id> = self.trampoline_labels.get(&i).copied();
        match self.merges[i] {
            Merge::Selection(merge) => {
                let merge = own.map_or(self.labels[merge], |label| label);
                let merge = b.at(merge);
                b.selection_merge(merge, 0);
            }
            Merge::Loop { merge, continue_at } => {
                let merge = own.map_or(self.labels[merge], |label| label);
                let merge = b.at(merge);
                let continue_at = b.at(self.labels[continue_at]);
                b.loop_merge(merge, continue_at, 0);
            }
            Merge::None if self.loops.contains_key(&i) => {
                return Err(Error::Unsupported {
                    offset: self.ir.blocks[i].offset,
                    what: "a loop with no legal merge point",
                });
            }
            Merge::None if self.ir.blocks[i].exit.successors().len() > 1 => {
                return Err(Error::Unsupported {
                    offset: self.ir.blocks[i].offset,
                    what: "a selection with no legal merge point",
                });
            }
            Merge::None => {}
        }
        Ok(())
    }

    /// The per-lane condition of a branch.
    ///
    /// A lane mask is a boolean in structurisation, and so is SCC; a mask
    /// whose bit is a constant (all lanes or none) is a constant branch,
    /// which SPIR-V has no instruction for, so it becomes a comparison
    /// against `true`.
    fn branch_condition(
        &mut self,
        b: &mut Builder,
        block: usize,
        when: victory_shader::structure::Bool,
    ) -> Result<Id, Error> {
        if let Some(condition) = self.lane_bit(b, when) {
            return Ok(condition);
        }
        Err(Error::Unsupported {
            offset: self.ir.blocks[block].offset,
            what: "a branch condition with no lane value",
        })
    }

    /// The exit of block `i`: a terminator in structured shape, or a write of
    /// the next block's number and a branch back to the dispatcher.
    pub(crate) fn exit(&mut self, b: &mut Builder, i: usize) -> Result<(), Error> {
        if self.machine.is_some() {
            return self.exit_machine(b, i);
        }
        let exit = match self.st.exits[i] {
            LaneExit::Jump(target) => Exit::Jump(target),
            LaneExit::Branch {
                when,
                taken,
                not_taken,
            } => {
                let condition = self.branch_condition(b, i, when)?;
                self.merge(b, i)?;
                let taken = b.at(self.labels[taken]);
                let not_taken = b.at(self.labels[not_taken]);
                b.branch_conditional(condition, taken, not_taken);
                return Ok(());
            }
            LaneExit::End => Exit::End,
            LaneExit::Indirect => Exit::Indirect,
        };
        self.merge(b, i)?;
        match exit {
            Exit::Jump(target) => {
                let block = b.at(self.labels[target]);
                b.branch(block);
                Ok(())
            }
            Exit::Branch { .. } => unreachable!("branches are handled above"),
            // The program ends: the entry point returns. `OpReturn` inside a
            // function is legal in any block, so no join block is needed.
            Exit::End => {
                b.return_void();
                Ok(())
            }
            Exit::Indirect => Err(Error::Unsupported {
                offset: self.ir.blocks[i].offset,
                what: "an indirect branch (s_setpc_b64)",
            }),
        }
    }

    /// Block `i`'s exit in the state machine: the number of the next block
    /// goes into the lane's program counter and the lane returns to the
    /// dispatcher, which reads it.
    fn exit_machine(&mut self, b: &mut Builder, i: usize) -> Result<(), Error> {
        let Some(machine) = self.machine else {
            unreachable!("only called in the state machine");
        };
        let (pc, continue_at) = (machine.pc, machine.continue_at);
        let value = match self.st.exits[i] {
            LaneExit::Jump(target) => b.constant_i32(target as i32),
            LaneExit::Branch {
                when,
                taken,
                not_taken,
            } => {
                let condition = self.branch_condition(b, i, when)?;
                let taken = b.constant_i32(taken as i32);
                let not_taken = b.constant_i32(not_taken as i32);
                b.select(self.types.i32_, condition, taken, not_taken)
            }
            // No case matches `-1`, so the switch's default case leaves the
            // loop and the entry point returns.
            LaneExit::End => b.constant_i32(-1),
            LaneExit::Indirect => {
                return Err(Error::Unsupported {
                    offset: self.ir.blocks[i].offset,
                    what: "an indirect branch (s_setpc_b64)",
                });
            }
        };
        b.store(pc, value);
        let next = b.at(continue_at);
        b.branch(next);
        Ok(())
    }
}
