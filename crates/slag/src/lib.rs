//! The `slag` embedding API: the host-facing surface of the Slag engine.
//!
//! One crate to depend on — a [`Context`] per agent/realm, [`JsValue`] and
//! [`JsObject`] handles, host callbacks and hooks, and (with the `jit`
//! feature) the Cranelift JIT hook. Everything else in the workspace is
//! internal; nothing else is part of the embedding contract.
//!
//! ```
//! use slag::{Context, JsValue};
//!
//! let mut context = Context::new().unwrap();
//! let value = context.eval("1 + 2").unwrap();
//! assert_eq!(value.as_number(), Some(3.0));
//! ```

pub use crux::error::{ErrorKind, JsError};
pub use runtime::HostHooks;
pub use runtime::dump;
pub use runtime::embed::{Context, FunctionCall, HostCallbacks, HostFn, JsObject, JsValue};
pub use runtime::embed::{OutputFn, RandomFn};

/// The V8-shaped Rust surface — [`Isolate`](api::Isolate), [`Context`](api::Context),
/// [`Local`](api::Local), [`Module`](api::Module) and the rest — re-exported so a
/// host porting from the `v8` crate depends on this crate alone.
pub use runtime::api;

pub mod buffers;
pub mod objects;

/// Re-export the WebAssembly engine, so an embedder can drive a module from
/// Rust (`Store`/`Instance`/`Memory`/`Value`) instead of through the JS API.
#[cfg(feature = "wasm")]
pub use runtime::wasm;

/// Install the Cranelift JIT hook on `context`'s agent (feature `jit`).
///
/// The hook is what makes hot certified bodies run at machine speed; the
/// CLI and conformance sweep enable it by default.
#[cfg(feature = "jit")]
pub fn install_jit(context: &mut Context) -> Result<(), String> {
    jit::install(context.agent_mut())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eval_works_through_the_facade() {
        let mut context = Context::new().unwrap();
        let value = context.eval("1 + 2").unwrap();
        assert_eq!(value.as_number(), Some(3.0));
    }

    #[test]
    fn two_contexts_on_one_thread() {
        let mut a = Context::new().unwrap();
        let mut b = Context::new().unwrap();
        assert_eq!(a.eval("1 + 1").unwrap().as_number(), Some(2.0));
        assert_eq!(b.eval("2 + 2").unwrap().as_number(), Some(4.0));
    }

    #[test]
    fn contexts_stored_together_survive_moves() {
        // The host shape the sibling-GC bug bit: several contexts kept in one
        // container, so pushing reallocates and moves the earlier ones, and a
        // sibling collection roots the non-current ones.
        let mut contexts: Vec<Context> = Vec::new();
        for i in 0..4 {
            let mut context = Context::new().unwrap();
            context.eval(&format!("globalThis.tag = {i};")).unwrap();
            contexts.push(context);
        }
        // A collection driven by the last context must root the earlier ones.
        contexts[3].agent_mut().collect_garbage();
        for (i, context) in contexts.iter_mut().enumerate() {
            assert_eq!(
                context.eval("globalThis.tag").unwrap().as_number(),
                Some(i as f64)
            );
        }
    }

    #[cfg(feature = "jit")]
    #[test]
    fn two_jit_contexts_on_one_thread() {
        let mut a = Context::new().unwrap();
        install_jit(&mut a).unwrap();
        assert_eq!(a.eval("1 + 1").unwrap().as_number(), Some(2.0));
        let mut b = Context::new().unwrap();
        install_jit(&mut b).unwrap();
        // Compile bodies and allocate in `b`, then evaluate in `a`.
        b.eval("var acc = []; for (var i = 0; i < 20000; i++) { acc.push({ i: i }); } acc.length")
            .unwrap();
        assert_eq!(a.eval("1 + 1").unwrap().as_number(), Some(2.0));
        assert_eq!(b.eval("2 + 2").unwrap().as_number(), Some(4.0));
    }

    /// A host that installs its own global object implements [`api::HostOps`],
    /// so that trait is part of the boundary: this names it through `slag` alone
    /// — no `crux` dependency — builds a context over it, and calls the
    /// finalizer drain a host drives.
    #[test]
    fn a_host_defined_global_is_reachable_through_the_facade() {
        use crate::api::{Context, HostOps, Isolate};

        #[derive(Debug)]
        struct Global;

        impl HostOps for Global {}

        let mut isolate = Isolate::new();
        let context =
            Context::new_with_global_ops(&mut isolate, Some(std::rc::Rc::new(Global))).unwrap();
        let value = context.try_eval("1 + 1").expect("eval");
        assert_eq!(value.as_number(), Some(2.0));
        isolate.run_finalizers();
    }

    /// The V8-shaped surface, module loading included, is reachable from the
    /// embedding entrypoint alone — a host depends on `slag`, not `runtime`.
    #[test]
    fn the_v8_shaped_surface_is_reachable_through_the_facade() {
        use crate::api::{Isolate, Module, Object};
        let mut isolate = Isolate::new();
        let context = crate::api::Context::new(&mut isolate).expect("context");
        let module = Module::compile(&context, "m", "export const x = 41;").expect("compile");
        module.register(&context, "m").expect("register");
        module.instantiate(&context).expect("instantiate");
        module.evaluate(&context).expect("evaluate");
        let namespace = module.namespace(&context).expect("namespace");
        assert_eq!(
            Object::get(&context, &namespace, "x")
                .expect("get")
                .as_number(),
            Some(41.0)
        );
    }

    #[cfg(feature = "jit")]
    #[test]
    fn jit_hook_installs_and_runs_a_loop() {
        let mut context = Context::new().unwrap();
        install_jit(&mut context).unwrap();
        let value = context
            .eval("function f(n) { var s = 0; for (var i = 0; i < n; i++) { s += i; } return s; } f(1000)")
            .unwrap();
        assert_eq!(value.as_number(), Some(499500.0));
    }

    #[cfg(feature = "fs")]
    #[test]
    fn fs_globals_install_through_the_feature() {
        let mut context = Context::new().unwrap();
        context.install_fs().unwrap();
        assert!(context.eval("typeof fs").is_ok());
    }

    #[cfg(feature = "raylib")]
    #[test]
    fn raylib_namespace_installs_through_the_feature() {
        let mut context = Context::new().unwrap();
        context.install_raylib().unwrap();
        assert_eq!(
            context.eval("typeof rl").unwrap().as_string().as_deref(),
            Some("object")
        );
        assert!(context.eval("rl.drawCircle").is_ok());
    }

    #[test]
    fn rlx_demo_renders_headlessly_against_a_stub_rl() {
        // Run the actual rlx_demo.js (render + draw path) against a stub `rl`
        // backend so the demo file is exercised without a window. This keeps
        // the demo honest headlessly — a bad `rl`/`rlx` reference or a draw
        // mistake fails here instead of at the first frame on a display.
        let mut context = Context::new().unwrap();
        context.install_rlx().unwrap();
        context
            .eval(
                "if (typeof console === 'undefined') { globalThis.console = { log: function () {} }; }\n\
                 let frames = 0;\n\
                 globalThis.rl = {\n\
                     DARKGRAY: 1, RAYWHITE: 2,\n\
                     initWindow: function () {}, setTargetFPS: function () {},\n\
                     windowShouldClose: function () { frames += 1; return frames > 1; },\n\
                     beginDrawing: function () {}, endDrawing: function () {},
                     clearBackground: function () {}, drawText: function () {},
                     getFPS: function () { return 60; }, closeWindow: function () {},
                     getFrameTime: function () { return 0.016; },
                     drawRectangle: function () {}, drawCircle: function () {},
                     color: function (r, g, b, a) {
                         return ((r & 255) << 24) | ((g & 255) << 16) | ((b & 255) << 8) | (a & 255);
                     },
                     guiPanel: function () {}, guiLabel: function () {},
                     guiSlider: function () {}, guiCheckBox: function () {},
                     guiToggle: function () {}, guiTextBox: function () {},
                     guiButton: function () { return false; },
                     guiStatusBar: function () {},\n\
                 };\n",
            )
            .unwrap();
        context
            .eval_jsx(include_str!("../examples/rlx_demo.jsx"))
            .unwrap();
    }
}
