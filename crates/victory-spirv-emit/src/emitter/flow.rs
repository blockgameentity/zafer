//! Control flow: blocks, branches and the end of the program.
//!
//! The IR's blocks are already one lane's view of the program: a branch
//! carries the per-lane condition of that edge. A SPIR-V block is entered by
//! every lane that took the edge, so the condition becomes the operand of an
//! `OpBranchConditional`, and no lane mask is ever materialised.
//!
//! A block whose guard is a lane condition (the console's per-lane early
//! exits) needs the guard applied inside it, not on the edges into it: the
//! IR gives each instruction its guard, and [`super::vector`] tests them.

use victory_shader::ir::Exit;
use victory_shader::structure::LaneExit;
use victory_spirv::module::Id;
use victory_spirv::builder::Builder;

use super::Emitter;
use crate::Error;

impl<'a> Emitter<'a> {
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

    /// The exit of block `i`, as a SPIR-V terminator.
    pub(crate) fn exit(&mut self, b: &mut Builder, i: usize) -> Result<(), Error> {
        let exit = match self.st.exits[i] {
            LaneExit::Jump(target) => Exit::Jump(target),
            LaneExit::Branch {
                when,
                taken,
                not_taken,
            } => {
                let condition = self.branch_condition(b, i, when)?;
                let taken = self.labels[taken];
                let not_taken = self.labels[not_taken];
                let taken = b.at(taken);
                let not_taken = b.at(not_taken);
                b.branch_conditional(condition, taken, not_taken);
                return Ok(());
            }
            LaneExit::End => Exit::End,
            LaneExit::Indirect => Exit::Indirect,
        };
        match exit {
            Exit::Jump(target) => {
                let target = self.labels[target];
                let block = b.at(target);
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
}
