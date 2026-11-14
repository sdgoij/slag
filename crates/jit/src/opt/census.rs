//! The materialization census: the B0 probe of
//! `.notes/register-tier-plan.md` (its §1.3 holds the measurements).
//!
//! A read-only count of where a body's values leave the register
//! representations — `Type::Int` (an i32 register) and `Type::Float` (a raw
//! f64, `pass::unbox`) — for a `Value` word, grouped by *why* the consumer
//! needed the word. §2 of the plan enumerates four legitimate reasons (a frame
//! access, a guard's deopt branch, an allocation, a call); every conversion at
//! any other consumer is the budget Phase B is trying to spend.
//!
//! The counts decide whether Phase B is worth its rewrite, so they are printed
//! beside the `JIT_DUMP_IR` dump. Nothing here transforms the IR, and the only
//! caller is env-gated, so a body's generated code cannot change.
//!
//! Causes are classified from the op alone, which is exact for the two
//! mechanisms that matter (an `Op::Canon` in front of a consumer, and an `Int`
//! operand of a non-int-native op) and a documented heuristic for the rest: an
//! op that calls a helper but is not a call *step* (a member read, an element
//! read) is counted as [`Cause::Call`], because the helper is the operation.
//!
//! "Per iteration" means "the cycle-resident instructions", so a body with
//! several cycles counts each of them — including cycles that are mutually
//! exclusive at run time (a hoist guard's guarded and general copies). Read the
//! number as an upper bound for such a body, and check `loops` before quoting
//! it.

use std::fmt::Write as _;

use super::ir::{BlockId, Function, Inst, Op, Term, Type, ValueId};

/// One of §2's four materialization points, or the avoidable remainder.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Cause {
    /// A frame slot access: the slot is the value's home.
    Frame,
    /// A guard's deopt branch: the resumed interpreter reads the mirrored
    /// operand stack and the frame.
    Guard,
    /// An allocation or a literal build, i.e. a GC safepoint.
    Alloc,
    /// A call, or a operation lowered through a helper (a member read, an
    /// element read), whose callee may observe the world.
    Call,
    /// A plain computation or comparison consuming a `Value` word only because
    /// it has no register-lane lowering: the conversions Phase B can remove.
    Compute,
    /// A conversion this census cannot attribute (a value with two consumers,
    /// or a conversion with none).
    Other,
}

impl Cause {
    /// Every cause, in report order.
    pub const ALL: [Cause; 6] = [
        Cause::Frame,
        Cause::Guard,
        Cause::Alloc,
        Cause::Call,
        Cause::Compute,
        Cause::Other,
    ];

    /// The report label.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Cause::Frame => "frame",
            Cause::Guard => "guard",
            Cause::Alloc => "alloc",
            Cause::Call => "call",
            Cause::Compute => "compute",
            Cause::Other => "other",
        }
    }
}

/// The conversion counts of one scope (the whole body, or its cycles).
#[derive(Clone, Copy, Debug, Default)]
pub struct Counts {
    pub frame_stores: u32,
    pub frame_loads: u32,
    /// `Int` operands materialized into a `Value` word.
    pub int_widen: u32,
    /// `Op::Canon` conversions from a raw f64 to a `Value` word.
    pub canon: u32,
    /// Every conversion above, by cause.
    pub by_cause: [u32; 6],
    /// A raw f64 consumed by an op that is neither a raw-accepting arithmetic
    /// op nor `Op::Canon`. `pass::unbox` inserts the `Canon`, so a non-zero
    /// count means the one-funnel invariant is broken somewhere.
    pub uncovered: u32,
}

impl Counts {
    /// Total level-3 → `Value` conversions.
    #[must_use]
    pub fn conversions(&self) -> u32 {
        self.int_widen + self.canon
    }

    /// The conversions no materialization point explains.
    #[must_use]
    pub fn avoidable(&self) -> u32 {
        self.by_cause[Cause::Compute as usize]
    }
}

/// The census of one body.
#[derive(Clone, Copy, Debug, Default)]
pub struct Census {
    pub blocks: u32,
    /// Blocks on a cycle (and therefore executed once per iteration of it).
    pub loop_blocks: u32,
    /// The number of cyclic strongly connected components.
    pub loops: u32,
    /// Counts restricted to cyclic blocks: per iteration, if the body loops.
    pub looped: Counts,
    /// Counts over every block: per body invocation.
    pub body: Counts,
}

/// Count `func`.
#[must_use]
pub fn count(func: &Function) -> Census {
    let succ = successors(func);
    let (loops, cyclic) = cyclic_blocks(&succ);
    let (uses, last_consumer) = consumers(func);
    let mut census = Census {
        blocks: func.block_count() as u32,
        loops,
        loop_blocks: cyclic.iter().filter(|c| **c).count() as u32,
        ..Census::default()
    };
    for b in 0..func.block_count() as BlockId {
        let in_cycle = cyclic[b as usize];
        for inst in &func.block(b).insts {
            tally(func, inst, &uses, &last_consumer, &mut census.body);
            if in_cycle {
                tally(func, inst, &uses, &last_consumer, &mut census.looped);
            }
        }
    }
    census
}

/// Render the census of `func` as the text printed with the `JIT_DUMP_IR`
/// dump.
#[must_use]
pub fn report(func: &Function) -> String {
    let c = count(func);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "blocks {}, loops {} ({} cycle-resident blocks)",
        c.blocks, c.loops, c.loop_blocks
    );
    if c.loops == 0 {
        let _ = writeln!(out, "per body (no cycle): {}", counts_line(&c.body));
    } else {
        let _ = writeln!(out, "per iteration: {}", counts_line(&c.looped));
        let _ = writeln!(out, "per body:      {}", counts_line(&c.body));
    }
    let _ = writeln!(
        out,
        "raw f64 without Canon: loop {}, body {}",
        c.looped.uncovered, c.body.uncovered
    );
    out
}

fn counts_line(c: &Counts) -> String {
    let mut line = format!(
        "frame_store {}, frame_load {}, canon {}, int->value {} ({} conversions)",
        c.frame_stores,
        c.frame_loads,
        c.canon,
        c.int_widen,
        c.conversions()
    );
    let causes = Cause::ALL
        .iter()
        .map(|cause| format!("{} {}", cause.label(), c.by_cause[*cause as usize]))
        .collect::<Vec<_>>()
        .join(", ");
    let _ = write!(line, "; causes: {causes}; avoidable {}", c.avoidable());
    line
}

/// Add `inst`'s traffic to `counts`.
fn tally(
    func: &Function,
    inst: &Inst,
    uses: &[u32],
    last_consumer: &[Option<Op>],
    counts: &mut Counts,
) {
    match inst.op {
        Op::FrameLoad => counts.frame_loads += 1,
        Op::FrameStore => counts.frame_stores += 1,
        Op::Canon => {
            counts.canon += 1;
            // `unbox` places the conversion directly in front of the consumer
            // that needs the word, so the consumer names the reason.
            let cause = inst
                .result
                .filter(|r| uses[*r as usize] == 1)
                .and_then(|r| last_consumer[r as usize])
                .map_or(Cause::Other, cause_of);
            counts.by_cause[cause as usize] += 1;
        }
        _ => {}
    }
    let cause = cause_of(inst.op);
    for &arg in &inst.args {
        match func.value_type(arg) {
            Type::Int if !preserves_int(inst.op) => {
                counts.int_widen += 1;
                counts.by_cause[cause as usize] += 1;
            }
            Type::Float if !accepts_raw(inst.op) && inst.op != Op::Canon => {
                counts.uncovered += 1;
            }
            _ => {}
        }
    }
}

/// Whether the lowering keeps an `Int` operand in its i32 register.
fn preserves_int(op: Op) -> bool {
    matches!(
        op,
        Op::IntAdd
            | Op::IntSub
            | Op::IntMul
            | Op::BitAnd
            | Op::BitOr
            | Op::BitXor
            | Op::Shl
            | Op::Shr
            | Op::UShr
    )
}

/// Whether the lowering feeds a raw f64 straight into the op, mirroring
/// `pass::unbox::accepts_raw`.
fn accepts_raw(op: Op) -> bool {
    matches!(
        op,
        Op::Add | Op::Sub | Op::Mul | Op::Div | Op::Lt | Op::Le | Op::Gt | Op::Ge
    )
}

/// Which materialization point, if any, explains a conversion at `op`.
fn cause_of(op: Op) -> Cause {
    match op {
        Op::FrameLoad | Op::FrameStore | Op::TdzCheck => Cause::Frame,
        Op::Check | Op::GuardType | Op::GuardCallee | Op::MemberGuard | Op::CalleeEq => {
            Cause::Guard
        }
        Op::NewObject
        | Op::NewArray
        | Op::ArrayBegin
        | Op::ObjectBegin
        | Op::NewClosure
        | Op::StringPush
        | Op::StringSub
        | Op::StringTemplate => Cause::Alloc,
        Op::Call
        | Op::Intrinsic
        | Op::Construct
        | Op::ConstructArgs
        | Op::TaggedTemplate
        | Op::FunctionDecl
        | Op::ArgsBase
        | Op::ArgsPush
        | Op::ArgsSpread
        | Op::GlobalLoad
        | Op::GlobalStore
        | Op::IdentLoad
        | Op::ContextLoad
        | Op::ContextStore
        | Op::ContextInit
        | Op::MemberLoad
        | Op::MemberStore
        | Op::MemberCellLoad
        | Op::ElementLoad
        | Op::ElementStore
        | Op::ArrayElement
        | Op::ArrayEnd
        | Op::ObjectInitName
        | Op::ForOfBegin
        | Op::ForOfNextBindLocal
        | Op::ForOfClose
        | Op::ForInBegin
        | Op::ForInNext
        | Op::ForOfBindLocal
        | Op::CompletionReset
        | Op::CompletionStore
        | Op::UpdateValue => Cause::Call,
        Op::Add
        | Op::Sub
        | Op::Mul
        | Op::Div
        | Op::Mod
        | Op::Pow
        | Op::Eq
        | Op::StrictEq
        | Op::Lt
        | Op::Le
        | Op::Gt
        | Op::Ge
        | Op::Neg
        | Op::BitNot
        | Op::Not
        | Op::ToNumber
        | Op::ToString
        | Op::ToBoolean
        | Op::ToObject
        | Op::IntAdd
        | Op::IntSub
        | Op::IntMul
        | Op::BitAnd
        | Op::BitOr
        | Op::BitXor
        | Op::Shl
        | Op::Shr
        | Op::UShr => Cause::Compute,
        _ => Cause::Other,
    }
}

/// The successor blocks of every block, from its terminator.
fn successors(func: &Function) -> Vec<Vec<BlockId>> {
    let mut succ = vec![Vec::new(); func.block_count()];
    for b in 0..func.block_count() as BlockId {
        match &func.block(b).term {
            Some(Term::Jump { target, .. }) => succ[b as usize].push(*target),
            Some(Term::Branch {
                then_block,
                else_block,
                ..
            }) => {
                succ[b as usize].push(*then_block);
                succ[b as usize].push(*else_block);
            }
            _ => {}
        }
    }
    succ
}

/// The number of cyclic SCCs and a per-block "is on a cycle" flag.
fn cyclic_blocks(succ: &[Vec<BlockId>]) -> (u32, Vec<bool>) {
    struct Tarjan<'a> {
        succ: &'a [Vec<BlockId>],
        index: Vec<u32>,
        low: Vec<u32>,
        on_stack: Vec<bool>,
        stack: Vec<BlockId>,
        next: u32,
        cyclic: Vec<bool>,
        loops: u32,
    }

    impl Tarjan<'_> {
        fn visit(&mut self, v: BlockId) {
            let vi = v as usize;
            self.index[vi] = self.next;
            self.low[vi] = self.next;
            self.next += 1;
            self.stack.push(v);
            self.on_stack[vi] = true;
            // Clone: `visit` needs `&mut self`.
            let edges = self.succ[vi].clone();
            for w in edges {
                let wi = w as usize;
                if self.index[wi] == u32::MAX {
                    self.visit(w);
                    self.low[vi] = self.low[vi].min(self.low[wi]);
                } else if self.on_stack[wi] {
                    self.low[vi] = self.low[vi].min(self.index[wi]);
                }
            }
            if self.low[vi] != self.index[vi] {
                return;
            }
            let mut members = Vec::new();
            while let Some(w) = self.stack.pop() {
                self.on_stack[w as usize] = false;
                members.push(w);
                if w == v {
                    break;
                }
            }
            let self_loop = self.succ[vi].contains(&v);
            if members.len() > 1 || self_loop {
                self.loops += 1;
                for m in members {
                    self.cyclic[m as usize] = true;
                }
            }
        }
    }

    let n = succ.len();
    let mut tarjan = Tarjan {
        succ,
        index: vec![u32::MAX; n],
        low: vec![0; n],
        on_stack: vec![false; n],
        stack: Vec::new(),
        next: 0,
        cyclic: vec![false; n],
        loops: 0,
    };
    for b in 0..n as BlockId {
        if tarjan.index[b as usize] == u32::MAX {
            tarjan.visit(b);
        }
    }
    (tarjan.loops, tarjan.cyclic)
}

/// The use count of every value, and the op of its last consumer.
fn consumers(func: &Function) -> (Vec<u32>, Vec<Option<Op>>) {
    let n = func.value_count() as usize;
    let mut uses = vec![0u32; n];
    let mut last = vec![None; n];
    for b in 0..func.block_count() as BlockId {
        for inst in &func.block(b).insts {
            for &arg in &inst.args {
                uses[arg as usize] += 1;
                last[arg as usize] = Some(inst.op);
            }
        }
        // An edge argument is consumed by the target block's parameter, not by
        // an op, so it has no cause to charge a conversion to.
        let term_args: Vec<ValueId> = match &func.block(b).term {
            Some(Term::Jump { args, .. }) => args.clone(),
            Some(Term::Branch {
                then_args,
                else_args,
                ..
            }) => then_args.iter().chain(else_args).copied().collect(),
            _ => Vec::new(),
        };
        for arg in term_args {
            uses[arg as usize] += 1;
            last[arg as usize] = None;
        }
    }
    (uses, last)
}

#[cfg(test)]
mod tests {
    use super::super::builder::Builder;
    use super::super::ir::{Effects, Heap, Imm};
    use super::*;

    /// `entry -> header(p) -> { body -> header, exit }`, the shape a lifted
    /// counting loop lowers to.
    fn counting_loop() -> Function {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let header = b.block();
            let body = b.block();
            let exit = b.block();
            let one = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Int,
                Effects::pure(),
                Imm::Int(1),
            );
            b.term(
                entry,
                Term::Jump {
                    target: header,
                    args: vec![one],
                },
            );

            let i = b.param(header, Type::Int);
            let n = b.emit(
                header,
                Op::Const,
                &[],
                Type::Int,
                Effects::pure(),
                Imm::Int(10),
            );
            let test = b.emit(
                header,
                Op::Lt,
                &[i, n],
                Type::Bool,
                Effects::call(),
                Imm::None,
            );
            b.term(
                header,
                Term::Branch {
                    cond: test,
                    then_block: body,
                    then_args: vec![i],
                    else_block: exit,
                    else_args: vec![i],
                },
            );

            let i_in = b.param(body, Type::Int);
            let next = b.emit(
                body,
                Op::IntAdd,
                &[i_in, one],
                Type::Int,
                Effects::pure(),
                Imm::None,
            );
            b.emit_void(
                body,
                Op::FrameStore,
                &[next],
                Effects::write(Heap::Slots),
                Imm::Slot(3),
            );
            b.term(
                body,
                Term::Jump {
                    target: header,
                    args: vec![next],
                },
            );

            let _ = b.param(exit, Type::Int);
            b.term(exit, Term::Return(None));
        }
        func
    }

    #[test]
    fn counts_a_loop_wide_and_per_iteration() {
        let census = count(&counting_loop());
        assert_eq!(census.loops, 1);
        // header and body are the cycle; entry and exit are not.
        assert_eq!(census.blocks, 4);
        assert_eq!(census.loop_blocks, 2);

        // Per iteration: the header's `Lt` has two `Int` operands (a compute
        // conversion each, because comparisons have no int lane), and the
        // body's frame store widens its `Int` value. `IntAdd` keeps both
        // operands in i32, so it contributes nothing.
        assert_eq!(census.looped.int_widen, 3);
        assert_eq!(census.looped.frame_stores, 1);
        assert_eq!(census.looped.frame_loads, 0);
        assert_eq!(census.looped.by_cause[Cause::Frame as usize], 1);
        assert_eq!(census.looped.by_cause[Cause::Compute as usize], 2);
        assert_eq!(census.looped.conversions(), 3);
        assert_eq!(census.looped.avoidable(), 2);

        // The entry and exit blocks add no conversion of their own.
        assert_eq!(census.body.conversions(), 3);
        assert_eq!(census.body.int_widen, census.looped.int_widen);
    }

    #[test]
    fn a_loopless_body_reports_no_cycle() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let v = b.emit(
                entry,
                Op::Const,
                &[],
                Type::Int,
                Effects::pure(),
                Imm::Int(1),
            );
            b.emit_void(
                entry,
                Op::FrameStore,
                &[v],
                Effects::write(Heap::Slots),
                Imm::Slot(0),
            );
            b.term(entry, Term::Return(None));
        }
        let census = count(&func);
        assert_eq!(census.loops, 0);
        assert_eq!(census.loop_blocks, 0);
        assert_eq!(census.body.frame_stores, 1);
        assert_eq!(census.body.int_widen, 1);
        assert!(report(&func).contains("no cycle"));
    }

    #[test]
    fn a_canon_is_charged_to_its_consumer() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let raw = b.emit(entry, Op::Add, &[], Type::Float, Effects::pure(), Imm::None);
            let canon = b.emit(
                entry,
                Op::Canon,
                &[raw],
                Type::Number,
                Effects::pure(),
                Imm::None,
            );
            // The consumer is a helper-shaped read (a member load), so the
            // conversion is charged to the call class rather than to compute.
            b.emit(
                entry,
                Op::MemberLoad,
                &[canon],
                Type::Unknown,
                Effects::read(Heap::Elements),
                Imm::Atom(0),
            );
            b.term(entry, Term::Return(None));
        }
        let census = count(&func);
        assert_eq!(census.body.canon, 1);
        assert_eq!(census.body.by_cause[Cause::Call as usize], 1);
        assert_eq!(census.body.uncovered, 0);
    }

    #[test]
    fn an_uncanonicalized_raw_float_is_flagged() {
        let mut func = Function::new();
        let entry = func.entry();
        {
            let mut b = Builder::new(&mut func);
            let raw = b.emit(entry, Op::Add, &[], Type::Float, Effects::pure(), Imm::None);
            b.emit(
                entry,
                Op::ToString,
                &[raw],
                Type::String,
                Effects::call(),
                Imm::None,
            );
            b.term(entry, Term::Return(None));
        }
        assert_eq!(count(&func).body.uncovered, 1);
    }
}
