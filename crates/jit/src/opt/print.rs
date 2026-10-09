//! A textual dump of the IR, for debugging and for lift/lower round-trip
//! checks.

use std::fmt::Write as _;

use super::ir::{Effects, Function, Heap, Imm, Inst, Term, Type};

/// Render `func` as one line per instruction, grouped by block.
#[must_use]
pub fn dump(func: &Function) -> String {
    let mut out = String::new();
    for b in 0..func.block_count() as u32 {
        let block = func.block(b);
        let params = block
            .params
            .iter()
            .map(|p| format!("v{p}:{}", ty(func.value_type(*p))))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(out, "b{b}({params}):");
        for inst in &block.insts {
            let _ = writeln!(out, "  {}", inst_line(func, inst));
        }
        let _ = writeln!(out, "  {}", term_line(block.term.as_ref()));
    }
    out
}

fn inst_line(func: &Function, inst: &Inst) -> String {
    let args = inst
        .args
        .iter()
        .map(|a| format!("v{a}"))
        .collect::<Vec<_>>()
        .join(", ");
    let body = format!("{:?}({args}){}", inst.op, imm_suffix(&inst.imm));
    match inst.result {
        Some(r) => format!(
            "v{r}:{} = {body} {}",
            ty(func.value_type(r)),
            eff(inst.effects)
        ),
        None => format!("{body} {}", eff(inst.effects)),
    }
}

fn term_line(term: Option<&Term>) -> String {
    match term {
        Some(Term::Jump { target, args }) => format!("jump b{target}({})", vs(args)),
        Some(Term::Branch {
            cond,
            then_block,
            then_args,
            else_block,
            else_args,
        }) => format!(
            "branch v{cond}, b{then_block}({}), b{else_block}({})",
            vs(then_args),
            vs(else_args)
        ),
        Some(Term::Return(Some(v))) => format!("return v{v}"),
        Some(Term::Return(None)) => "return".to_string(),
        Some(Term::Throw(v)) => format!("throw v{v}"),
        Some(Term::Unreachable) => "unreachable".to_string(),
        None => "<no terminator>".to_string(),
    }
}

fn vs(args: &[u32]) -> String {
    args.iter()
        .map(|a| format!("v{a}"))
        .collect::<Vec<_>>()
        .join(", ")
}

fn imm_suffix(imm: &Imm) -> String {
    match imm {
        Imm::None => String::new(),
        Imm::Int(i) => format!(" {i}"),
        Imm::Float(f) => format!(" {f}"),
        Imm::Bool(b) => format!(" {b}"),
        Imm::Str(s) => format!(" {s:?}"),
        Imm::Slot(i) => format!(" slot{i}"),
        Imm::Arg(i) => format!(" arg{i}"),
        Imm::Atom(i) => format!(" atom{i}"),
        Imm::Context { depth, index } => format!(" ctx{depth}:{index}"),
        Imm::ForOfBind { slot, cursor } => {
            format!(" slot{slot} cursor({},{})", cursor.0, cursor.1)
        }
        Imm::U64(v) => format!(" {v:#x}"),
    }
}

fn eff(e: Effects) -> String {
    let reads = heaps(|h| e.may_read(h));
    let writes = heaps(|h| e.may_write(h));
    format!("[{reads}/{writes}]")
}

fn heaps(has: impl Fn(Heap) -> bool) -> String {
    let mut s = String::new();
    if has(Heap::Slots) {
        s.push('s');
    }
    if has(Heap::Elements) {
        s.push('e');
    }
    if has(Heap::Globals) {
        s.push('g');
    }
    if has(Heap::World) {
        s.push('w');
    }
    if s.is_empty() {
        s.push('-');
    }
    s
}

fn ty(t: Type) -> &'static str {
    match t {
        Type::Never => "never",
        Type::Undefined => "undef",
        Type::Null => "null",
        Type::Bool => "bool",
        Type::Int => "int",
        Type::Number => "num",
        Type::String => "str",
        Type::Object => "obj",
        Type::Hole => "hole",
        Type::Unknown => "?",
    }
}

#[cfg(test)]
mod tests {
    use super::super::builder::Builder;
    use super::*;
    use crate::opt::ir::Op;

    #[test]
    fn dumps_blocks_instructions_and_terminators() {
        let mut func = Function::new();
        let entry = func.entry();
        {
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
        }
        let text = dump(&func);
        assert!(text.contains("b0():"));
        assert!(text.contains("v0:int = Const() 1 [-/-]"));
        assert!(text.contains("v2:num = Add(v0, v1) [segw/segw]"));
        assert!(text.contains("return v2"));
    }
}
