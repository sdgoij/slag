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

**S2b done — public element decorators.** `decorate_element` (method/getter/
setter) and `decorate_field` build the `{kind, name, static, private:false,
access, addInitializer}` context and call the decorators in reverse source order.
`access` is synthesized as real JS closures over the key (`obj[k]`, `k in obj`,
`obj[k] = v`) so it runs the full `[[Get]]`/`[[Set]]`/`[[HasProperty]]` protocol
— note the trap: a synthesized function must carry a *unique* span, or
`shared_function_body` (keyed on the function node's address *and* span) hands
the first-built body to all of them. Method/get/set replacement and field
value-initializers apply; instance initializers are stored on the constructor
(`EcmaFunction::decorator_initializers`, run before the fields) and static
initializers run before the static fields. Private elements and auto-accessors
stay unevaluated (S3). Fifteen unit tests in the module; main 48632/0/1 and
staging 77/0/1406 unchanged.

## S3 — private elements + auto-accessors (done)

**S3a done — private elements.** `ElementName` generalizes an element's name to
a public key or a private name + the class PrivateEnvironment; the synthesized
`access` closures index `#name` (`obj.#name`, `#name in obj`, `obj.#name = v`)
and carry that environment, so `has` is the brand check and `get`/`set` run the
private element. `context.name` is the `"#name"` description and `private` is
true. Private method/get/set/field decoration (including field value
initializers) all work. Five more unit tests (20 total); sweeps unchanged.

**S3b done — auto-accessors.** `build_class` now indexes the element list and
recognizes the parser's desugared trio (a private `%auto-accessorN%` storage
Field + a Get + a Set sharing one span) when the Get carries decorators. The
trio decorates as ONE `kind:"accessor"` element: the context value is
`{ get, set }`, `access` is `{ has, get, set }`, and a returned object may
replace `get`/`set` and supply an `init` value initializer for the backing
storage. Undecorated auto-accessors still take the ordinary Field/Get/Set arms
(a computed key still evaluates twice, unchanged). A private auto-accessor
(`accessor #x`) desugars to a private getter/setter pair over its storage, so
it decorates as an accessor too. Three more tests (23 total).

## S4 — hand-written tests + notes

test262 cannot gate S2/S3, so tests are written by hand (the
`crates/runtime/src/**` `mod tests` `run(...)` style). S2a/S2b cover reverse-order
calls, the context shape per kind, the class context with no
`access`/`static`/`private`, method replacement, field value-initializers,
`access.get/has/set` through the property protocol, and the initializer phase
ordering, plus private elements and auto-accessors (25 tests in `decorators.rs`).
The evaluation arc is done, both deviations closed; `.notes/conformance.md`
records it.

## Traps

- A green test262 sweep proves nothing here; do not claim decorators work on it.
- `context.metadata` is TypeScript-only — do not add it.
- `@dec static {}` is a SyntaxError (the proposal gives ClassStaticBlock no
  DecoratorList); enforced in `parse_class_element`.
- Composed `@d [k](){}`: the decorator expression evaluates before the computed
  key (reading order), matching S1's capture.
