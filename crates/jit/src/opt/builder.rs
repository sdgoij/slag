//! A low-level builder for the SSA IR.
//!
//! The builder allocates values and appends instructions; it does not verify.
//! A producer (the lift, or a pass) builds a graph and then calls
//! [`verify`](super::verify::verify). Block parameters are the phis: a value
//! defined by a parameter is passed as an edge argument at each predecessor.

use super::ir::{BlockId, Effects, Function, Imm, Inst, Op, Term, Type, ValueId};

/// Appends instructions and blocks to a [`Function`].
pub struct Builder<'f> {
    func: &'f mut Function,
}

impl<'f> Builder<'f> {
    /// A builder over `func`.
    pub fn new(func: &'f mut Function) -> Self {
        Builder { func }
    }

    /// The function under construction.
    #[must_use]
    pub fn func(&self) -> &Function {
        self.func
    }

    /// The function under construction, mutably.
    pub fn func_mut(&mut self) -> &mut Function {
        self.func
    }

    /// Allocate a value that is not yet defined by a parameter or an
    /// instruction. A value that is never defined is rejected by `verify`.
    pub fn value(&mut self, ty: Type) -> ValueId {
        self.func.push_value(ty)
    }

    /// Append an empty block and return its id.
    pub fn block(&mut self) -> BlockId {
        self.func.push_block()
    }

    /// Add a parameter to `block` and return its value.
    pub fn param(&mut self, block: BlockId, ty: Type) -> ValueId {
        let v = self.func.push_value(ty);
        self.func.block_mut(block).params.push(v);
        v
    }

    /// Emit an instruction that produces a value.
    pub fn emit(
        &mut self,
        block: BlockId,
        op: Op,
        args: &[ValueId],
        ty: Type,
        effects: Effects,
        imm: Imm,
    ) -> ValueId {
        let result = self.func.push_value(ty);
        self.func.block_mut(block).insts.push(Inst {
            op,
            args: args.to_vec(),
            result: Some(result),
            ty,
            effects,
            imm,
        });
        result
    }

    /// Emit an instruction with no result (a store or a bare guard).
    pub fn emit_void(
        &mut self,
        block: BlockId,
        op: Op,
        args: &[ValueId],
        effects: Effects,
        imm: Imm,
    ) {
        self.func.block_mut(block).insts.push(Inst {
            op,
            args: args.to_vec(),
            result: None,
            ty: Type::Never,
            effects,
            imm,
        });
    }

    /// Set the terminator of `block`.
    pub fn term(&mut self, block: BlockId, term: Term) {
        self.func.block_mut(block).term = Some(term);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_a_straight_line_function() {
        let mut func = Function::new();
        let entry = func.entry();
        let mut b = Builder::new(&mut func);
        let one = b.emit(
            entry,
            Op::Const,
            &[],
            Type::Int,
            Effects::pure(),
            Imm::Int(1),
        );
        let two = b.emit(
            entry,
            Op::Const,
            &[],
            Type::Int,
            Effects::pure(),
            Imm::Int(2),
        );
        let sum = b.emit(
            entry,
            Op::Add,
            &[one, two],
            Type::Number,
            Effects::call(),
            Imm::None,
        );
        b.term(entry, Term::Return(Some(sum)));

        assert_eq!(func.block_count(), 1);
        assert_eq!(func.value_count(), 3);
        assert_eq!(func.block(entry).insts.len(), 3);
        assert_eq!(func.value_type(sum), Type::Number);
    }

    #[test]
    fn allocates_dense_value_ids() {
        let mut func = Function::new();
        let entry = func.entry();
        let mut b = Builder::new(&mut func);
        let v0 = b.value(Type::Unknown);
        let v1 = b.value(Type::Unknown);
        assert_eq!((v0, v1), (0, 1));
        let p = b.param(entry, Type::Int);
        assert_eq!(p, 2);
        assert_eq!(func.block(entry).params, vec![2]);
    }
}
