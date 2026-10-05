# Decorators: the stage-3 plan

**Status:** S1 landed (AST + parser capture, behaviour unchanged). S2–S4 pending.

## Where the spec lives (and why test262 cannot gate this)

Decorators are **not** in ecma262 main: the live `ClassElement` grammar has no
`DecoratorList`, and `ClassDefinitionEvaluation` has no decorator steps.
test262 ships **no decorator-evaluation fixtures** — only the 24 syntax fixtures
under `language/{expressions,statements}/class/decorator/syntax/`. So a green
sweep proves nothing about S2. The authoritative prose is the
`tc39/proposal-decorators` README (stage 2.7) plus **PR tc39/ecma262#2417** for
the final text.

Native V8 is **not** an available oracle: Node 24 rejects `@` even with
`--js-decorators`, `--harmony-decorators` is not a flag, and Deno 2.6 (V8
14.5.201.2-rusty) rejects it too.

**Working oracle:** TypeScript 5.9 stage-3 emit. With `experimentalDecorators`
OFF, `npx -y -p typescript tsc --target es2022 --module nodenext` lowers
decorators to `__esDecorate`/`__runInitializers`, which encode the stage-3
protocol; the emitted JS runs under plain node. TypeScript adds a **non-spec
`metadata` key** to the context — ignore it.

## Oracle results (public elements, TS 5.9)

| kind      | value passed | `access` keys   | `name`            | `static`/`private` |
|-----------|--------------|-----------------|-------------------|--------------------|
| method    | function     | `has, get`      | key (string/sym)  | present            |
| getter    | function     | `has, get`      | key               | present            |
| setter    | function     | `has, set`      | key               | present            |
| field     | `undefined`  | `has, get, set` | key               | present            |
| accessor  | `{get,set}`  | `has, get, set` | key               | present            |
| class     | the class    | **absent**      | class name / `undefined` | **absent**  |

Semantics confirmed by running emitted code:

- **Call order within one element is reverse source order** (`@m1 @m2 m(){}` →
  `m2` called first, then `m1`).
- **Class decorators** are called after all element decorators, in reverse
  source order.
- **Return handling:** method/get/set return a replacement function or
  `undefined`; a field decorator returns `(init) => newInit` or `undefined`; a
  class decorator returns a replacement constructor or `undefined`.
- **`access`:** `get(obj)` = `Get(obj, key)`, `has(obj)` = `HasProperty`,
  `set(obj, v)` = `Set`. For a field it reflects the decorated (post-initializer)
  value: `@field f = 41` with `(init)=>init+1` reads back 42.
- **Initializers (`addInitializer(fn)`, called with the class/instance as
  `this`):** non-static method/get/set run *before* any instance field; field
  and accessor initializers run immediately after that element initializes, in
  source order; static element initializers run during class definition; class
  initializers run after the class is fully defined, including static fields.

**Resolved — cross-element order is source order.** The README's TS comparison
states it directly: "the order of evaluation in this proposal is based on the
ordering in the program, regardless of whether it is static or instance."
TypeScript deviates (it batches instance before static), which is exactly what
that comparison calls out. Within one element, calls run in reverse source order.

**Access is arg-based, not `this`-based.** The README's `.call(instance, value)`
example is stage-2.7 prose; both shipping implementations pass the receiver as
the first argument — TypeScript emits `{ has: obj => "m" in obj, get: obj => obj.m,
set: (obj, v) => { obj.m = v; } }`, and Babel's `applyDecs2305` builds the same
shape (`get = target => target[name]`, `set = (target, v) => { target[name] = v }`,
`has = target => name in target`). Key presence follows the README per kind:
method/getter `{has, get}`, setter `{has, set}`, field/accessor `{has, get, set}`.

## S2 — public method/get/set/field + class evaluation

**S2a done — class decorators.** `crates/runtime/src/decorators.rs` evaluates a
class's decorator list, builds the `{kind, name, addInitializer}` context (no
`access`/`static`/`private`), calls the decorators in reverse source order,
applies a returned callable as the replacement, and runs the initializers after
the static fields, throwing on a late `addInitializer` or a non-callable result.
`build_class` wires it in before the static-elements loop. Seven unit tests in
the module; main sweep unchanged (48632/0/1).

**S2b — element decorators (next).** In `crates/runtime/src/class.rs::build_class`,
for each element with a non-empty `decorators` list:

1. Build the `context` object: `kind`, `name` (key or `undefined`), `static`,
   `private`, `access` (per the table), and `addInitializer`.
2. Evaluate/applying order: call decorators reverse-source-order, threading the
   value through; record `undefined` returns as "unchanged".
3. Apply the result: replace the method/accessor function, or attach a field
   initializer that wraps the existing initializer.
4. Collect initializers and run them at the correct phase (see above).
5. Class decorators: after the class value exists, call `(ctor, context)` with
   `kind:"class"`, apply a returned replacement, then run class initializers.

Needs a `context` helper in `crates/runtime/src/context.rs` and a
`decorate_class_element(...)` in `class.rs` invoked at the top of the element
loop. The compiled path (`ir.rs::compile_class`) only precomputes computed keys
and delegates to `build_class`, so S2 is interpreter-side.

## S3 — auto-accessor + private

`accessor` desugars in the parser to a private-storage field + public get/set
sharing a span; the decorator list currently rides the synthesized **getter**
(the first publicly visible element). S3 must reconstruct the trio as one
`kind:"accessor"` decoration (`value = {get, set}`, `access = {has,get,set}`)
and implement private elements (`name` is the `"#name"` string, `private:true`).

## S4 — hand-written tests + notes

test262 cannot gate S2/S3, so tests must be written by hand (the
`crates/runtime/src/**` `mod tests` `run(...)` style). Cover, at minimum:
reverse-order calls, context shape per kind, class context with no
`access`/`static`/`private`, field-initializer return, `access.get/has/set`,
and initializer phase ordering. Update `.notes/conformance.md`.

## Traps

- A green test262 sweep proves nothing here; do not claim decorators work on it.
- `context.metadata` is TypeScript-only — do not add it.
- `@dec static {}` is currently parsed and dropped (S1 preserves the old
  behaviour); the final proposal makes decorators on static blocks a
  SyntaxError. Verify against PR 2417 and, if so, add the early error.
- Composed `@d [k](){}`: the decorator expression evaluates before the computed
  key (reading order), matching S1's capture.
