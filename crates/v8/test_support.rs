//! Scaffolding for this crate's own tests: a scope with a context entered, and
//! the ways a test gets a value out of a script.

use crate::data::{Number, Object, String, Value};
use crate::handle::Local;
use crate::scope::PinScope;

/// Run `body` with a handle scope over a fresh isolate and a context entered,
/// binding the scope to the given name.
macro_rules! in_context {
    ($scope:ident, $body:block) => {
        let isolate = &mut crate::Isolate::new(crate::CreateParams::default());
        crate::scope!(let handle_scope, isolate);
        let context = crate::Context::new(handle_scope, Default::default());
        let $scope = &mut crate::ContextScope::new(handle_scope, context);
        $body
    };
}
pub(crate) use in_context;

/// The value a script evaluates to.
pub(crate) fn eval<'s>(scope: &PinScope<'s, '_>, source: &str) -> Local<'s, Value> {
    let code = String::new(scope, source).expect("string");
    let script = crate::Script::compile(scope, code, None).expect("compile");
    script.run(scope).expect("run")
}

/// The number a script evaluates to.
pub(crate) fn eval_number(scope: &PinScope<'_, '_>, source: &str) -> f64 {
    Local::<Number>::try_from(eval(scope, source))
        .expect("number")
        .value()
}

/// Bind `value` on the realm's global object, which is how a host hands a value
/// to a script.
pub(crate) fn bind(scope: &PinScope<'_, '_>, name: &str, value: Local<'_, Value>) {
    let global = Local::<Object>::from_engine(crate::realm_of(scope).global());
    let key = String::new(scope, name).expect("string");
    global.set(scope, key.into(), value).expect("set");
}
