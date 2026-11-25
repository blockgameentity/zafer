//! Functions and their blocks: the control-flow skeleton of a module.

use crate::module::Id;
use crate::ops::Op;

use super::Builder;

/// One block of a function, being written.
#[derive(Clone, Copy, Debug)]
pub struct Block {
    /// The block's label id.
    pub label: Id,
    done: bool,
}

impl Block {
    /// Appends an instruction with no operands beyond `operands`.
    pub(super) fn op(
        self,
        b: &mut Builder,
        op: Op,
        ty: Option<Id>,
        result: Option<Id>,
        operands: &[u32],
    ) {
        
        b.module.op(op, ty, result, operands);
    }
}

/// A function being written.
#[derive(Clone, Copy, Debug)]
pub struct Function {
    /// The function's own id.
    pub id: Id,
    /// The entry block, where declarations that must come first go.
    pub entry: Block,
}

impl Builder {
    /// Starts a function `result` from `params` with `control` flags. The
    /// entry block is created and labelled, as the layout demands.
    pub fn function(&mut self, name: &str, result: Id, params: &[Id], control: u32) -> Function {
        let ty = self.function_type(result, params);
        let id = self.module.id();
        // `OpFunction`: result type is the function's return type, then the
        // result id, the control flags and the function type.
        self.module
            .op(Op::Function, Some(result), Some(id), &[control, ty.0]);
        let label = self.module.id();
        self.module.op(Op::Label, None, Some(label), &[]);
        self.name(id, name);
        Function {
            id,
            entry: Block { label, done: false },
        }
    }

    /// A new block, which becomes the current one.
    pub fn block(&mut self) -> Block {
        let label = self.module.id();
        self.module.op(Op::Label, None, Some(label), &[]);
        Block { label, done: false }
    }

    /// An id for a block whose `OpLabel` is written later.
    ///
    /// A function's blocks must appear in order, each label immediately
    /// followed by its own body: an emitter that needs every target's id
    /// before it writes any body (branches refer forward) reserves the ids
    /// here and writes the labels as it reaches them with [`Builder::label`].
    pub fn reserve_label(&mut self) -> Block {
        let label = self.module.id();
        Block { label, done: false }
    }

    /// Writes the `OpLabel` of the block `id`, starting its body.
    pub fn label(&mut self, id: Id) -> Block {
        self.module.op(Op::Label, None, Some(id), &[]);
        Block { label: id, done: false }
    }

    /// The block whose label is `label`, so an instruction can be written
    /// into a block by its id (the function's entry, after more blocks).
    pub fn at(&mut self, label: Id) -> Block {
        Block {
            label,
            done: false,
        }
    }

    /// Writes `OpVariable` with a callee-chosen id: a function variable is
    /// declared in the entry block but the sections reorder around it, so
    /// the block only documents where it came from.
    pub fn declare_variable(&mut self, block: Id, pointer_type: Id, id: Id) -> Id {
        // The storage class is an operand (`Function` here: a declared
        // function variable is always that), and a module without it is not
        // SPIR-V.
        let space = crate::enums::StorageClass::Function as u32;
        self.module
            .op_in_function(Op::Variable, Some(pointer_type), Some(id), &[space]);
        let _ = block;
        id
    }

    /// Emits `OpFunctionParameter` and returns its id.
    pub fn function_parameter(&mut self, ty: Id) -> Id {
        let id = self.module.id();
        self.module.op(Op::FunctionParameter, Some(ty), Some(id), &[]);
        id
    }

    /// `OpFunctionEnd`.
    pub fn end_function(&mut self, f: Function) {
        self.module.op(Op::FunctionEnd, None, None, &[]);
        let _ = f;
    }

    /// `OpBranch`.
    pub fn branch(&mut self, target: Block) {
        self.module.op(Op::Branch, None, None, &[target.label.0]);
    }

    /// `OpBranchConditional`.
    pub fn branch_conditional(&mut self, condition: Id, then: Block, otherwise: Block) {
        self.module.op(
            Op::BranchConditional,
            None,
            None,
            &[condition.0, then.label.0, otherwise.label.0],
        );
    }

    /// `OpSwitch` over `selector`, `default` and `(literal, target)` pairs.
    pub fn switch(&mut self, selector: Id, default: Block, targets: &[(u32, Block)]) {
        let mut operands = vec![selector.0, default.label.0];
        for (literal, target) in targets {
            operands.push(*literal);
            operands.push(target.label.0);
        }
        self.module.op(Op::Switch, None, None, &operands);
    }

    /// `OpLoopMerge`: `merge` is where the loop ends, `continue_target`
    /// where an iteration continues.
    pub fn loop_merge(&mut self, merge: Block, continue_target: Block, control: u32) {
        self.module.op(
            Op::LoopMerge,
            None,
            None,
            &[merge.label.0, continue_target.label.0, control],
        );
    }

    /// `OpSelectionMerge`.
    pub fn selection_merge(&mut self, merge: Block, control: u32) {
        self.module
            .op(Op::SelectionMerge, None, None, &[merge.label.0, control]);
    }

    /// `OpPhi` for `ty` from `(value, block)` pairs.
    pub fn phi(&mut self, ty: Id, incoming: &[(Id, Block)]) -> Id {
        let mut operands = Vec::with_capacity(incoming.len() * 2);
        for (value, block) in incoming {
            operands.push(value.0);
            operands.push(block.label.0);
        }
        let id = self.module.id();
        self.module.op(Op::Phi, Some(ty), Some(id), &operands);
        id
    }

    /// `OpReturn`.
    pub fn return_void(&mut self) {
        self.module.op(Op::Return, None, None, &[]);
    }

    /// `OpReturnValue`.
    pub fn return_value(&mut self, value: Id) {
        self.module.op(Op::ReturnValue, None, None, &[value.0]);
    }

    /// `OpUnreachable`.
    pub fn unreachable(&mut self) {
        self.module.op(Op::Unreachable, None, None, &[]);
    }

    /// `OpKill`, in a fragment shader.
    pub fn kill(&mut self) {
        self.module.op(Op::Kill, None, None, &[]);
    }

    /// `OpFunctionCall` of `function` with `args`.
    pub fn call(&mut self, ty: Id, function: Id, args: &[Id]) -> Id {
        let mut operands = vec![function.0];
        operands.extend(args.iter().map(|a| a.0));
        let id = self.module.id();
        self.module.op(Op::FunctionCall, Some(ty), Some(id), &operands);
        id
    }
}
