//! `HostOps::get_with_phase`/`set_with_phase` report whether a lookup reached
//! the host as the receiver itself or through the receiver's prototype chain.

use std::cell::RefCell;
use std::rc::Rc;

use crux::error::JsError;
use crux::host::{HostOps, LookupPhase};
use crux::object::JsObject;
use crux::property::PropertyKey;
use crux::string::JsString;
use crux::value::Value;

#[derive(Debug, Default)]
struct PhaseRecorder {
    phases: RefCell<Vec<LookupPhase>>,
}

impl HostOps for PhaseRecorder {
    fn get_with_phase(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
        _receiver: &Value,
        phase: LookupPhase,
    ) -> Option<Result<Value, JsError>> {
        self.phases.borrow_mut().push(phase);
        Some(Ok(Value::Number(1.0)))
    }

    fn set_with_phase(
        &self,
        _object: &JsObject,
        _key: &PropertyKey,
        _value: &Value,
        _receiver: &Value,
        _throw: bool,
        phase: LookupPhase,
    ) -> Option<Result<bool, JsError>> {
        self.phases.borrow_mut().push(phase);
        Some(Ok(true))
    }
}

#[test]
fn a_host_read_and_write_report_their_lookup_phase() {
    let recorder = Rc::new(PhaseRecorder::default());
    let host = JsObject::host_object_create(recorder.clone(), None);
    let receiver = JsObject::ordinary_object_create(Some(host));
    let key = JsString::from_utf8("x");

    assert_eq!(host.get(&key).unwrap(), Value::Number(1.0));
    assert_eq!(receiver.get(&key).unwrap(), Value::Number(1.0));
    receiver.set(&key, Value::Number(2.0), true).unwrap();
    host.set(&key, Value::Number(3.0), true).unwrap();

    assert_eq!(
        *recorder.phases.borrow(),
        vec![
            LookupPhase::OwnProperty,
            LookupPhase::PrototypeChain,
            LookupPhase::PrototypeChain,
            LookupPhase::OwnProperty,
        ]
    );
}
