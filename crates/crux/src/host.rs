//! Host-defined exotic objects (the `ObjectKind::Host` variant): internal
//! methods dispatch to a [`HostOps`] implementation with ordinary fallback.
//!
//! This is the seam the C drop-in surfaces use for host objects: the JSC
//! `JSClassRef` callbacks (crates/jsc) and V8 handler objects (crates/v8)
//! implement `HostOps`, and every method defaults to `None` — not
//! intercepted, so the ordinary internal method runs (the model of e.g.
//! JavaScriptCore's `JSClassRef` callbacks, where an absent callback
//! behaves ordinarily).

use crate::error::JsError;
use crate::heap::{GcAny, Trace};
use crate::object::{JsObject, Property};
use crate::property::{PropertyDescriptor, PropertyKey};
use crate::value::Value;

/// The host-defined behaviour of an `ObjectKind::Host` object. Each method
/// returns `Option<Result<..>>`: `None` falls back to the ordinary internal
/// method, `Some(Err(..))` surfaces an error, `Some(Ok(..))` overrides.
pub trait HostOps: std::fmt::Debug {
    /// [[GetOwnProperty]].
    fn get_own_property(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
    ) -> Option<Result<Property, JsError>> {
        None
    }

    /// [[DefineOwnProperty]].
    fn define_property(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
        _desc: &PropertyDescriptor,
    ) -> Option<Result<bool, JsError>> {
        None
    }

    /// [[HasProperty]] (the prototype-walking form, not [[HasOwnProperty]]).
    fn has_property(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
    ) -> Option<Result<bool, JsError>> {
        None
    }

    /// [[Get]] (P, Receiver).
    fn get(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
        _receiver: &Value,
    ) -> Option<Result<Value, JsError>> {
        None
    }

    /// [[Set]] (P, V, Receiver, Throw).
    ///
    /// `throw` is the operation's `Throw` argument (spec 10.1.9.3 step 3), the
    /// one V8's `PropertyCallbackArguments::should_throw_on_error` reports: a
    /// host that answers `false` here lets the engine turn the failed set into a
    /// TypeError exactly as an ordinary set would.
    fn set(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
        _value: &Value,
        _receiver: &Value,
        _throw: bool,
    ) -> Option<Result<bool, JsError>> {
        None
    }

    /// [[Delete]].
    fn delete(&self, _object: &JsObject, _key: &PropertyKey) -> Option<Result<bool, JsError>> {
        None
    }

    /// [[OwnPropertyKeys]].
    fn own_property_keys(&self, _object: &JsObject) -> Option<Result<Vec<PropertyKey>, JsError>> {
        None
    }

    /// [[Call]]: a host object that reports [`is_callable`](Self::is_callable)
    /// must implement this.
    fn call(
        &self,
        _object: &JsObject,
        _this: &Value,
        _args: &[Value],
    ) -> Option<Result<Value, JsError>> {
        None
    }

    /// [[Construct]]: a host object that reports
    /// [`is_constructible`](Self::is_constructible) must implement this.
    fn construct(
        &self,
        _object: &JsObject,
        _args: &[Value],
        _new_target: &Value,
    ) -> Option<Result<Value, JsError>> {
        None
    }

    /// Whether `typeof` reports the object as `"function"` (and `call` is
    /// implemented).
    fn is_callable(&self) -> bool {
        false
    }

    /// Whether the object is a constructor (and `construct` is implemented).
    fn is_constructible(&self) -> bool {
        false
    }

    /// Called once, after a collection swept the object this behaviour was
    /// installed on, with that object's identity (`JsObject::id`).
    ///
    /// This is the engine's *only* call about the object's death, and it is
    /// deliberately not `Drop`: the sweep drops the object's payload while an
    /// `Rc` the host still holds stays a live Rust value whose JS wrapper is
    /// gone, and this callback is what tells the host which wrapper died. It is
    /// never called mid-collection — the sweep queues the pair and the host
    /// drains it at a safe point (`Isolate::run_finalizers`) — so an
    /// implementation may allocate and may run JS. One behaviour shared by many
    /// objects is finalized once per object, so use `object_id` to tell them
    /// apart.
    ///
    /// A host that registers a `Weak` handle (`.notes/host-object-gc.md` §4.3)
    /// gets the same news through that handle; this callback is for a host whose
    /// objects have host state to release rather than a value to watch.
    fn finalize(&self, _object_id: u64) {}
}

/// A host exotic object's engine-owned state: the host's behaviour, plus the
/// values the host retained on the object.
///
/// The behaviour is shared — one `Rc` per host *class*, which is what lets a
/// C surface answer "is this value an instance of that class" by comparing the
/// handle by address — while the edge list is per object. Both halves are
/// engine-visible, and the edge list has to be: a value a host object holds is
/// otherwise invisible to the collector, so it is swept and its arena slot is
/// handed to the next allocation, and the host's handle then silently aliases
/// whatever took the slot. A C host cannot implement a tracer, which is why the
/// list lives here rather than in `HostOps`.
#[derive(Debug)]
pub struct HostObject {
    behaviour: std::rc::Rc<dyn HostOps>,
    /// The retained values (spec: none — this is the engine's own edge list, the
    /// counterpart of a host handle table). `RefCell` because a host retains and
    /// releases from its own code while a collection may be tracing the list;
    /// `Trace for RefCell` aborts that sweep rather than panicking.
    edges: std::cell::RefCell<Vec<Value>>,
}

impl HostObject {
    pub fn new(behaviour: std::rc::Rc<dyn HostOps>) -> Self {
        Self {
            behaviour,
            edges: std::cell::RefCell::new(Vec::new()),
        }
    }

    /// The host behaviour, shared with every object of the same host class.
    pub fn behaviour(&self) -> &std::rc::Rc<dyn HostOps> {
        &self.behaviour
    }

    /// Retain one more reference to `value`.
    ///
    /// The caller is `JsObject::host_object_retain`, which also runs the write
    /// barrier: this is a store, and only the object can be remembered.
    pub(crate) fn retain(&self, value: Value) {
        self.edges.borrow_mut().push(value);
    }

    /// Drop one retention of `value`; a value that was never retained is a
    /// no-op. No barrier: removing an edge cannot create an old->young one.
    pub(crate) fn release(&self, value: Value) {
        let mut edges = self.edges.borrow_mut();
        if let Some(index) = edges.iter().position(|held| *held == value) {
            edges.remove(index);
        }
    }

    /// How many values this object retains.
    pub fn edge_count(&self) -> usize {
        self.edges.borrow().len()
    }
}

impl std::ops::Deref for HostObject {
    type Target = dyn HostOps;

    fn deref(&self) -> &Self::Target {
        &*self.behaviour
    }
}

impl Trace for HostObject {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        self.edges.trace(visit);
    }
}

thread_local! {
    /// Host objects the arena sweep dropped since the runtime last drained the
    /// queue. Each entry is the behaviour to call and the identity to call it
    /// with, both captured *before* the payload was dropped (they live in it).
    /// Ids are process-global and never reused, so a stale entry names a dead
    /// object rather than a live one, and an agent that never owned it has
    /// nothing to release. Draining happens outside a collection, because a
    /// finalizer may allocate or run JS.
    static PENDING_FINALIZERS: std::cell::RefCell<Vec<(std::rc::Rc<dyn HostOps>, u64)>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Queue `object`'s finalizer, from the sweep that is about to drop its payload.
///
/// Pushing a pair is safe mid-collection: it neither allocates in the arena nor
/// touches the heap.
pub(crate) fn request_finalize(object: &HostObject, object_id: u64) {
    PENDING_FINALIZERS.with(|queue| {
        queue
            .borrow_mut()
            .push((std::rc::Rc::clone(object.behaviour()), object_id));
    });
}

/// Take the finalizers the collections since the last drain queued, in the order
/// the boxes were swept. Called outside a collection.
pub fn take_pending_finalizers() -> Vec<(std::rc::Rc<dyn HostOps>, u64)> {
    PENDING_FINALIZERS.with(|queue| std::mem::take(&mut *queue.borrow_mut()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::JsObject;
    use crate::string::JsString;

    /// A host object that intercepts reads of `magic` and behaves
    /// ordinarily otherwise.
    #[derive(Debug)]
    struct InterceptGet;

    impl HostOps for InterceptGet {
        fn get(
            &self,
            _object: &JsObject,
            key: &PropertyKey,
            _receiver: &Value,
        ) -> Option<Result<Value, JsError>> {
            if key == &PropertyKey::from_utf8("magic") {
                Some(Ok(Value::Number(42.0)))
            } else {
                None
            }
        }
    }

    #[test]
    fn host_get_intercepts_magic_and_falls_back_otherwise() {
        let object = JsObject::host_object_create(std::rc::Rc::new(InterceptGet), None);
        assert_eq!(
            object.get(&JsString::from_utf8("magic")).unwrap(),
            Value::Number(42.0)
        );
        // Unintercepted keys fall through to ordinary (absent -> undefined).
        assert_eq!(
            object.get(&JsString::from_utf8("other")).unwrap(),
            Value::Undefined
        );
    }

    /// A callable host object.
    #[derive(Debug)]
    struct Callable;

    impl HostOps for Callable {
        fn is_callable(&self) -> bool {
            true
        }

        fn call(
            &self,
            _object: &JsObject,
            _this: &Value,
            args: &[Value],
        ) -> Option<Result<Value, JsError>> {
            Some(Ok(args.first().cloned().unwrap_or(Value::Undefined)))
        }
    }

    #[test]
    fn callable_host_objects_report_function_and_call() {
        let object = JsObject::host_object_create(std::rc::Rc::new(Callable), None);
        let value = Value::Object(object);
        assert!(crate::value::is_callable(&value));
        assert_eq!(crate::value::type_of(&value), "function");
        let result =
            crate::function::call(&value, Value::Undefined, &[Value::Number(7.0)]).unwrap();
        assert_eq!(result, Value::Number(7.0));
    }

    /// The box address a string value points at.
    fn string_box_addr(value: Value) -> usize {
        match value.kind() {
            crate::value::ValueKind::String(handle) => handle.as_any().addr(),
            other => panic!("expected a string value, got {other:?}"),
        }
    }

    /// The values a host object retains — asserted directly where a test is
    /// about the list rather than about the collection's verdict.
    fn host_edge_count(object: &crate::handle::Handle<JsObject>) -> usize {
        match &object.kind {
            crate::object::ObjectKind::Host(host) => host.edge_count(),
            other => panic!("expected a host object, got {}", other.name()),
        }
    }

    /// A host object's retained value is a real GC edge: the collector marks it
    /// *through* the host object. `Heap::collect` does not scan the stack, so the
    /// object's own handle is the only root and the peer has no route to the
    /// mark at all — which is precisely the defect the edge list closes (a value
    /// a host object holds used to be swept, and the host's handle then aliased
    /// whatever reused the box).
    #[test]
    fn a_host_objects_retained_edge_roots_its_value() {
        use crate::handle::Handle;

        let retained = Value::String(Handle::new(JsString::from_utf8("retained")));
        let peer = Value::String(Handle::new(JsString::from_utf8("peer")));
        let object = JsObject::host_object_create(std::rc::Rc::new(InterceptGet), None);
        object.host_object_retain(retained);
        assert_eq!(host_edge_count(&object), 1);
        let roots = [object.as_any()];

        let swept = crate::heap::with_heap_mut(|heap| heap.collect(&roots));
        assert!(
            !swept.contains(&string_box_addr(retained)),
            "a value the host retained was swept: {swept:?}"
        );
        assert!(
            swept.contains(&string_box_addr(peer)),
            "an unretained peer survived: {swept:?}"
        );

        // Releasing the edge puts the value back where a collection with no
        // source reaches it: what the collector marks is the list's *current*
        // contents, not the fact that it was retained once.
        object.host_object_release(retained);
        assert_eq!(host_edge_count(&object), 0);
        let swept = crate::heap::with_heap_mut(|heap| heap.collect(&roots));
        assert!(
            swept.contains(&string_box_addr(retained)),
            "a released edge still rooted its value: {swept:?}"
        );
    }

    /// `host_object_retain` is a store into a possibly-*old* object, so it runs
    /// the write barrier: without it a minor collection has no reason to look at
    /// the host object, and the young value it just retained is swept. The
    /// remembered set is the direct evidence — that is the barrier's whole
    /// output — and the collection below is the same statement made by the
    /// collector.
    #[test]
    fn a_retained_edge_on_an_old_host_object_is_remembered() {
        use crate::handle::Handle;
        use crate::heap::{remembered_count, with_heap_mut};

        let object = JsObject::host_object_create(std::rc::Rc::new(InterceptGet), None);
        // Promote the host object to old, with itself as the only root.
        with_heap_mut(|heap| heap.collect(&[object.as_any()]));
        assert_eq!(remembered_count(), 0, "the collection drains the set");

        let child = Value::String(Handle::new(JsString::from_utf8("young child")));
        object.host_object_retain(child);
        assert_eq!(
            remembered_count(),
            1,
            "an old container that gained a young edge must be remembered"
        );

        // The object is deliberately not a root: the remembered set is the only
        // route to the edge, so a barrier that did not record it would lose the
        // child here.
        let swept = with_heap_mut(|heap| heap.collect_minor(&[]));
        assert!(
            !swept.contains(&string_box_addr(child)),
            "a retained edge's young value was swept: {swept:?}"
        );
    }

    /// The same edge with a collection after *every* allocation — the shape
    /// `--gc-stress` gives the engine, made explicit here so it needs no agent.
    /// The host object is promoted first, so every round is an old box gaining a
    /// young edge, the barrier's store, and a minor collection that has to reach
    /// the value through it.
    #[test]
    fn a_retained_edge_survives_a_collection_per_allocation() {
        use crate::handle::Handle;
        use crate::heap::{pin, remembered_count, with_heap_mut};

        let object = JsObject::host_object_create(std::rc::Rc::new(InterceptGet), None);
        // The real case is a host object JS can reach (a realm's global, a
        // property of another object); a pin is the flat equivalent, and it is
        // what leaves the *edge* as the only route to the values below.
        let _root = pin(Value::Object(object));
        with_heap_mut(|heap| {
            heap.collect(&[]);
        });
        assert_eq!(remembered_count(), 0, "the collection drains the set");

        let mut retained: Vec<Value> = Vec::new();
        for round in 0..32 {
            let value = Value::String(Handle::new(JsString::from_utf8("retained")));
            object.host_object_retain(value);
            retained.push(value);
            assert_eq!(
                remembered_count(),
                1,
                "round {round}: the store into the old host object is remembered"
            );
            // Garbage with the same lifetime as the value: only the edge
            // distinguishes the two in this round's collection.
            let garbage = Value::String(Handle::new(JsString::from_utf8("garbage")));
            let garbage_box = string_box_addr(garbage);

            let swept = with_heap_mut(|heap| heap.collect_minor(&[]));
            assert!(
                swept.contains(&garbage_box),
                "round {round}: unrooted garbage survived: {swept:?}"
            );
            for held in &retained {
                assert!(
                    !swept.contains(&string_box_addr(*held)),
                    "round {round}: a retained value was swept: {swept:?}"
                );
            }
        }
        assert_eq!(host_edge_count(&object), 32);
    }

    /// The sweep captures a host object's finalizer *before* dropping the
    /// payload — the behaviour handle and the identity both live in it — and the
    /// queued behaviour outlives its box, which is why the callback is not
    /// `Drop`. One swept object queues exactly one finalizer, once.
    #[test]
    fn a_swept_host_object_captures_its_finalizer() {
        use std::rc::Rc;

        // Other tests may share this thread's heap, so the queue can hold
        // entries from boxes a previous collection took: assert about *this*
        // object's entry rather than the queue's length.
        let _ = take_pending_finalizers();

        let behaviour: Rc<dyn HostOps> = Rc::new(InterceptGet);
        let (id, addr) = {
            let object = JsObject::host_object_create(Rc::clone(&behaviour), None);
            (object.id(), object.as_any().addr())
        };

        // Nothing roots it, and `Heap::collect` does not scan the stack, so the
        // sweep takes it.
        let swept = crate::heap::with_heap_mut(|heap| heap.collect(&[]));
        assert!(
            swept.contains(&addr),
            "the unrooted host object survived: {swept:?}"
        );

        let mine: Vec<(Rc<dyn HostOps>, u64)> = take_pending_finalizers()
            .into_iter()
            .filter(|(_, queued)| *queued == id)
            .collect();
        assert_eq!(mine.len(), 1, "one swept object queues one finalizer");
        assert!(
            Rc::ptr_eq(&mine[0].0, &behaviour),
            "the queued behaviour is the object's own"
        );

        // A second collection cannot queue it again: the box is gone, and both
        // sweeps clear the live bit before the payload is dropped.
        crate::heap::with_heap_mut(|heap| {
            heap.collect(&[]);
        });
        let again: Vec<u64> = take_pending_finalizers()
            .into_iter()
            .map(|(_, queued)| queued)
            .collect();
        assert!(
            !again.contains(&id),
            "a swept host object was finalized twice: {again:?}"
        );
    }

    /// A host object ↔ JS-value cycle is collected — the case a finalizer driven
    /// by reference counts can never reach (`.notes/host-object-gc.md` §1(b):
    /// `jsc`'s release-driven `finalize` never fires on one). The host object
    /// retains an object that holds the host object back, so the cycle is
    /// unreachable, and one precise collection takes both boxes and queues one
    /// finalizer.
    #[test]
    fn a_host_object_value_cycle_collects_and_finalizes() {
        use std::rc::Rc;

        let _ = take_pending_finalizers();

        let behaviour: Rc<dyn HostOps> = Rc::new(InterceptGet);
        let object = JsObject::host_object_create(Rc::clone(&behaviour), None);
        let id = object.id();
        let object_addr = object.as_any().addr();
        // The other half of the cycle: an ordinary object holding the host
        // object, which the host object retains.
        let holder = JsObject::ordinary_object_create(None);
        holder
            .define_property_or_throw(
                &JsString::from_utf8("host"),
                &crate::property::PropertyDescriptor {
                    value: Some(Value::Object(object)),
                    writable: Some(true),
                    get: None,
                    set: None,
                    enumerable: Some(true),
                    configurable: Some(true),
                },
            )
            .expect("defined");
        let holder_addr = holder.as_any().addr();
        object.host_object_retain(Value::Object(holder));

        // No roots, and `Heap::collect` does not scan the stack, so nothing names
        // either half: the two boxes are garbage together or not at all.
        let swept = crate::heap::with_heap_mut(|heap| heap.collect(&[]));
        assert!(
            swept.contains(&object_addr),
            "the host object survived its own cycle: {swept:?}"
        );
        assert!(
            swept.contains(&holder_addr),
            "the holder survived the cycle: {swept:?}"
        );

        let mine: Vec<(Rc<dyn HostOps>, u64)> = take_pending_finalizers()
            .into_iter()
            .filter(|(_, queued)| *queued == id)
            .collect();
        assert_eq!(
            mine.len(),
            1,
            "a collected cycle finalizes its host object exactly once"
        );
        assert!(Rc::ptr_eq(&mine[0].0, &behaviour));
    }
}
