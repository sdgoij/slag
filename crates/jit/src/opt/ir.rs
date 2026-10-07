//! The SSA control-flow IR of the optimizing front end.
//!
//! The IR is deliberately Cranelift-free: the lowering lives in
//! `crate::opt_lower`, and the lift out of the interpreter's `Step` stream
//! builds on top of this. See `.notes/optimizing-tier-impl.md` §2–§4.

/// A value: a block parameter or an instruction result.
pub type ValueId = u32;

/// A basic block.
pub type BlockId = u32;

/// The coarse type lattice.
///
/// A handful of cases captures most of what the passes need; every engine that
/// tried engine-global type inference deleted it, so this stays small.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum Type {
    /// No value (an effect-only instruction) or an unreachable point.
    Never,
    Undefined,
    Null,
    Bool,
    /// A known int32 (the result of `| 0`, a bit op, or `>>> 0`).
    Int,
    /// A JS number that may be a heap number.
    Number,
    String,
    Object,
    /// The dense-array hole sentinel.
    Hole,
    Unknown,
}

impl Type {
    /// The least upper bound of two types, used to type a block parameter
    /// whose incoming values disagree.
    #[must_use]
    pub fn join(self, other: Type) -> Type {
        if self == other {
            return self;
        }
        match (self, other) {
            (Type::Never, t) | (t, Type::Never) => t,
            (Type::Int, Type::Number) | (Type::Number, Type::Int) => Type::Number,
            _ => Type::Unknown,
        }
    }

    /// Whether the type is a number (either the int or the heap-number case).
    #[must_use]
    pub fn is_numeric(self) -> bool {
        matches!(self, Type::Int | Type::Number)
    }
}

/// A coarse heap region. The passes only need to know whether an instruction
/// observes a region and whether a later instruction may have changed it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Heap {
    /// Frame slots and the inline frame.
    Slots,
    /// Array and typed-array elements.
    Elements,
    /// Global bindings.
    Globals,
    /// Everything else, and the whole world for a call.
    World,
}

impl Heap {
    const fn bit(self) -> u8 {
        match self {
            Heap::Slots => 1,
            Heap::Elements => 2,
            Heap::Globals => 4,
            Heap::World => 8,
        }
    }
}

/// What an instruction reads and writes, as a bit set over [`Heap`].
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Effects {
    reads: u8,
    writes: u8,
}

impl Effects {
    /// No reads, no writes.
    #[must_use]
    pub const fn pure() -> Self {
        Effects {
            reads: 0,
            writes: 0,
        }
    }

    /// Reads `heap`, writes nothing.
    #[must_use]
    pub const fn read(heap: Heap) -> Self {
        Effects {
            reads: heap.bit(),
            writes: 0,
        }
    }

    /// Writes `heap`, reads nothing.
    #[must_use]
    pub const fn write(heap: Heap) -> Self {
        Effects {
            reads: 0,
            writes: heap.bit(),
        }
    }

    /// The effects of anything that may run arbitrary code: reads and writes
    /// the whole world.
    #[must_use]
    pub const fn call() -> Self {
        Effects {
            reads: 0b1111,
            writes: 0b1111,
        }
    }

    /// The union of two effect sets.
    #[must_use]
    pub const fn union(self, other: Effects) -> Effects {
        Effects {
            reads: self.reads | other.reads,
            writes: self.writes | other.writes,
        }
    }

    /// Whether the instruction reads anything.
    #[must_use]
    pub const fn reads_any(self) -> bool {
        self.reads != 0
    }

    /// Whether the instruction writes anything.
    #[must_use]
    pub const fn writes_any(self) -> bool {
        self.writes != 0
    }

    /// Whether the instruction reads or writes anything.
    #[must_use]
    pub const fn is_pure(self) -> bool {
        self.reads == 0 && self.writes == 0
    }

    /// Whether the instruction may observe `heap`.
    #[must_use]
    pub const fn may_read(self, heap: Heap) -> bool {
        self.reads & heap.bit() != 0
    }

    /// Whether the instruction may modify `heap`.
    #[must_use]
    pub const fn may_write(self, heap: Heap) -> bool {
        self.writes & heap.bit() != 0
    }

    /// Whether a write in `self` could change anything `earlier` reads.
    ///
    /// A world write clobbers every read; otherwise the write and the read
    /// must share a region. This is the query a load-elimination or LICM pass
    /// asks before moving or folding a load.
    #[must_use]
    pub const fn may_clobber_reads_of(self, earlier: Effects) -> bool {
        self.writes & (earlier.reads | Heap::World.bit()) != 0
    }
}

/// A constant or slot payload carried by an instruction.
#[derive(Clone, PartialEq, Debug, Default)]
pub enum Imm {
    #[default]
    None,
    Int(i32),
    Float(f64),
    Bool(bool),
    Str(Box<str>),
    /// A frame slot index.
    Slot(u32),
    /// An argument index.
    Arg(u32),
    /// An interned property/binding name atom.
    Atom(u32),
}

/// An instruction opcode.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    /// A constant; the payload is the [`Imm`].
    Const,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Pow,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    UShr,
    Neg,
    BitNot,
    Not,
    Eq,
    StrictEq,
    Lt,
    Le,
    Gt,
    Ge,
    ToNumber,
    ToString,
    ToBoolean,
    ToObject,
    FrameLoad,
    FrameStore,
    GlobalLoad,
    GlobalStore,
    MemberLoad,
    MemberStore,
    ElementLoad,
    ElementStore,
    NewObject,
    NewArray,
    NewClosure,
    Call,
    Construct,
    /// A speculation guard. When it fails the body retires and the
    /// interpreter resumes at the current step.
    Check,
    /// Write the statement-completion register to `undefined` and mark it
    /// empty (spec 6.2.2.3).
    CompletionReset,
    /// Write the statement-completion register to the operand, marked
    /// non-empty (spec 6.2.2.4). The register lives on the `Vm`, not in a
    /// [`Heap`] region, so both ops are modelled as `World` writes.
    CompletionStore,
}

impl Op {
    /// The default effect set for the op.
    ///
    /// This is the *sound* default: an arithmetic or comparison op may invoke
    /// `valueOf`/`toString`, so it reads and writes the world until the typer
    /// or the lift proves its operands are primitive. A pass must narrow this
    /// with what it knows, never assume it away.
    #[must_use]
    pub fn default_effects(self) -> Effects {
        use Effects as E;
        match self {
            Op::Const | Op::StrictEq | Op::ToBoolean | Op::Not | Op::Check => E::pure(),
            Op::FrameLoad => E::read(Heap::Slots),
            Op::FrameStore => E::write(Heap::Slots),
            Op::GlobalLoad => E::read(Heap::Globals),
            Op::GlobalStore => E::write(Heap::Globals),
            Op::MemberLoad => E::read(Heap::Slots).union(E::read(Heap::Elements)),
            Op::MemberStore => E::write(Heap::Slots).union(E::write(Heap::Elements)),
            Op::ElementLoad => E::read(Heap::Elements),
            Op::ElementStore => E::write(Heap::Elements),
            Op::CompletionReset | Op::CompletionStore => E::write(Heap::World),
            Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Div
            | Op::Mod
            | Op::Pow
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Neg
            | Op::BitNot
            | Op::Eq
            | Op::Lt
            | Op::Le
            | Op::Gt
            | Op::Ge
            | Op::ToNumber
            | Op::ToString
            | Op::ToObject
            | Op::NewObject
            | Op::NewArray
            | Op::NewClosure
            | Op::Call
            | Op::Construct => E::call(),
        }
    }
}

/// A single SSA instruction.
#[derive(Clone, Debug)]
pub struct Inst {
    pub op: Op,
    pub args: Vec<ValueId>,
    /// The produced value, or `None` for an effect-only instruction.
    pub result: Option<ValueId>,
    /// The result type (`Type::Never` when there is no result).
    pub ty: Type,
    pub effects: Effects,
    pub imm: Imm,
}

/// A block terminator.
#[derive(Clone, Debug)]
pub enum Term {
    Jump {
        target: BlockId,
        args: Vec<ValueId>,
    },
    Branch {
        cond: ValueId,
        then_block: BlockId,
        then_args: Vec<ValueId>,
        else_block: BlockId,
        else_args: Vec<ValueId>,
    },
    Return(Option<ValueId>),
    Throw(ValueId),
    Unreachable,
}

/// A basic block: parameters, instructions, and a terminator.
#[derive(Clone, Debug, Default)]
pub struct Block {
    /// The block parameters, which are the SSA phis: their values are defined
    /// by the incoming edge arguments.
    pub params: Vec<ValueId>,
    pub insts: Vec<Inst>,
    /// `None` only while a block is under construction; `verify` rejects a
    /// block with no terminator.
    pub term: Option<Term>,
}

/// A control-flow graph. Blocks are addressed by index; the entry is block 0.
#[derive(Clone, Debug)]
pub struct Function {
    blocks: Vec<Block>,
    value_types: Vec<Type>,
    entry: BlockId,
}

impl Function {
    /// A new function with a single empty entry block.
    #[must_use]
    pub fn new() -> Self {
        Function {
            blocks: vec![Block::default()],
            value_types: Vec::new(),
            entry: 0,
        }
    }

    /// The entry block.
    #[must_use]
    pub fn entry(&self) -> BlockId {
        self.entry
    }

    /// The number of blocks.
    #[must_use]
    pub fn block_count(&self) -> usize {
        self.blocks.len()
    }

    /// The number of values allocated.
    #[must_use]
    pub fn value_count(&self) -> u32 {
        self.value_types.len() as u32
    }

    /// The block with id `id`.
    ///
    /// # Panics
    /// Panics if `id` is not a valid block.
    #[must_use]
    pub fn block(&self, id: BlockId) -> &Block {
        &self.blocks[id as usize]
    }

    /// The block with id `id`, mutably.
    ///
    /// # Panics
    /// Panics if `id` is not a valid block.
    pub fn block_mut(&mut self, id: BlockId) -> &mut Block {
        &mut self.blocks[id as usize]
    }

    /// The type of a value.
    ///
    /// # Panics
    /// Panics if `v` is not a valid value.
    #[must_use]
    pub fn value_type(&self, v: ValueId) -> Type {
        self.value_types[v as usize]
    }

    /// Every value's type, indexed by [`ValueId`].
    #[must_use]
    pub fn value_types(&self) -> &[Type] {
        &self.value_types
    }

    pub(crate) fn push_value(&mut self, ty: Type) -> ValueId {
        let id = self.value_types.len() as ValueId;
        self.value_types.push(ty);
        id
    }

    pub(crate) fn push_block(&mut self) -> BlockId {
        let id = self.blocks.len() as BlockId;
        self.blocks.push(Block::default());
        id
    }
}

impl Default for Function {
    fn default() -> Self {
        Function::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lattice_joins_by_case() {
        assert_eq!(Type::Never.join(Type::Int), Type::Int);
        assert_eq!(Type::Int.join(Type::Number), Type::Number);
        assert_eq!(Type::Number.join(Type::Int), Type::Number);
        assert_eq!(Type::Int.join(Type::Int), Type::Int);
        assert_eq!(Type::String.join(Type::Object), Type::Unknown);
        assert_eq!(Type::Unknown.join(Type::Int), Type::Unknown);
    }

    #[test]
    fn effects_classify_and_clobber() {
        let load = Effects::read(Heap::Elements);
        let store = Effects::write(Heap::Elements);
        let call = Effects::call();
        assert!(!load.is_pure());
        assert!(Effects::pure().is_pure());
        assert!(call.may_read(Heap::Globals) && call.may_write(Heap::Globals));
        // A world write clobbers an element read even though the buckets differ.
        assert!(call.may_clobber_reads_of(load));
        // A slots write does not clobber an element read.
        assert!(!Effects::write(Heap::Slots).may_clobber_reads_of(load));
        assert!(store.may_clobber_reads_of(load));
        // A read never clobbers.
        assert!(!load.may_clobber_reads_of(load));
    }

    #[test]
    fn default_effects_are_sound() {
        assert!(Op::StrictEq.default_effects().is_pure());
        // Loose equality can coerce, so it is a world effect.
        assert!(Op::Eq.default_effects().may_read(Heap::World));
        assert!(Op::Add.default_effects().may_read(Heap::World));
        assert!(Op::FrameLoad.default_effects().may_read(Heap::Slots));
        assert!(Op::FrameStore.default_effects().may_write(Heap::Slots));
    }
}
