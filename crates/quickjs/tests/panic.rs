//! Panic containment at the Rust/C boundary (`ffi::guard` re-exported as
//! `quickjs::guard`). A panic in a Rust host callback unwinds to the guard,
//! which poisons the thread's isolate; later guarded calls reject.

use quickjs::Runtime;

#[test]
fn host_callback_panic_is_contained_and_poisons() {
    quickjs::unpoison();
    let mut runtime = Runtime::new();
    let context = runtime.create_context().unwrap();
    runtime
        .add_function(&context, "boom", |_args| {
            panic!("host callback panic");
        })
        .unwrap();

    // The panic must not unwind past the guard; the guard answers its default.
    let outcome: i32 = quickjs::guard(|| {
        let _ = context.eval("boom()");
        0
    });
    assert_eq!(outcome, 0);
    assert!(quickjs::is_poisoned());

    // A later guarded call short-circuits without running its body.
    let mut ran = false;
    let _: u8 = quickjs::guard(|| {
        ran = true;
        7
    });
    assert!(!ran);

    // An explicit reset restores the calls.
    quickjs::unpoison();
    assert!(!quickjs::is_poisoned());
}
