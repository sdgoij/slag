//! Handles: [`Local`], [`MaybeLocal`], [`Global`], and the payload they carry.

use std::fmt;
use std::marker::PhantomData;
use std::rc::Rc;

use runtime::api;

use crate::scope::PinScope;

/// What a handle points at.
///
/// The `T` in `Local<'s, T>` is a zero-sized tag whose only job is to select
/// methods and impls, so the payload cannot live in the tag — it lives here,
/// and `T` decides how it is read. There are two variants because the engine
/// has two kinds of thing a handle can name: a language value, and the realm a
/// `Local<Context>` names.
///
/// This is public only because [`Handle`] is; nothing outside the crate should
/// name it.
#[doc(hidden)]
#[derive(Clone)]
pub enum Payload {
    Value(api::Local),
    Context(Rc<api::Context>),
    /// A compiled script, held as its source. The engine parses at compile time
    /// and evaluates at run time, so there is no compiled-script object to
    /// point at.
    Script(Rc<str>),
}

impl Payload {
    pub(crate) fn as_value(&self) -> &api::Local {
        match self {
            Self::Value(value) => value,
            _ => panic!("bridge bug: a non-value handle read as a value"),
        }
    }

    /// The engine value, when this handle names one.
    pub(crate) fn as_value_opt(&self) -> Option<&api::Local> {
        match self {
            Self::Value(value) => Some(value),
            Self::Context(_) | Self::Script(_) => None,
        }
    }

    pub(crate) fn as_context(&self) -> &Rc<api::Context> {
        match self {
            Self::Context(context) => context,
            _ => panic!("bridge bug: a non-Context handle read as a Context"),
        }
    }

    pub(crate) fn as_script(&self) -> &Rc<str> {
        match self {
            Self::Script(source) => source,
            _ => panic!("bridge bug: a non-Script handle read as a Script"),
        }
    }
}

impl fmt::Debug for Payload {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Value(value) => write!(f, "Payload::Value({value:?})"),
            Self::Context(_) => f.write_str("Payload::Context(..)"),
            Self::Script(source) => write!(f, "Payload::Script({source:?})"),
        }
    }
}

/// The tag and the handle's lifetime, as one zero-sized marker.
///
/// `fn() -> T` rather than `&'s T` so that the marker is covariant in `T` and
/// does not require `T: 's` — the tag is only ever a type-level selector.
type Tag<'s, T> = PhantomData<(&'s (), fn() -> T)>;

/// A scoped handle over a language value (`v8::Local`).
///
/// The tag `T` decides which methods are in scope: method availability comes
/// from the `Deref` chain in [`data`](crate::data), which stands in for the C++
/// inheritance the crate we stand in for relies on.
pub struct Local<'s, T> {
    payload: Payload,
    marker: Tag<'s, T>,
}

impl<'s, T> Local<'s, T> {
    pub(crate) fn from_payload(payload: Payload) -> Self {
        Self {
            payload,
            marker: PhantomData,
        }
    }

    /// Wrap an engine value under an unchecked tag. The caller knows what it
    /// put in, which is the same trust the crate we stand in for places in its
    /// own casts.
    pub(crate) fn from_engine(value: api::Local) -> Self {
        Self::from_payload(Payload::Value(value))
    }

    /// Construct a handle from an existing persistent handle (`v8::Local::New`).
    pub fn new<'i, H: Handle<Data = T>>(_scope: &PinScope<'s, 'i, ()>, handle: H) -> Local<'s, T> {
        Self::from_payload(handle.into_payload())
    }

    /// The engine value behind the handle.
    pub(crate) fn engine(&self) -> &api::Local {
        self.payload.as_value()
    }

    /// The payload, for the tag predicates in [`data`](crate::data).
    pub(crate) fn payload(&self) -> &Payload {
        &self.payload
    }

    /// The realm behind a `Context` handle.
    pub(crate) fn context(&self) -> &Rc<api::Context> {
        self.payload.as_context()
    }

    /// The source behind a `Script` handle.
    pub(crate) fn script_source(&self) -> &Rc<str> {
        self.payload.as_script()
    }

    /// Retag the handle.
    ///
    /// Safe, unlike the crate we stand in for: the payload is untouched and the
    /// tag is phantom, so this is a rebuild rather than a reinterpretation.
    pub(crate) fn cast<U>(self) -> Local<'s, U> {
        let Self { payload, .. } = self;
        Local::from_payload(payload)
    }

    /// The same handle under another tag, by reference.
    ///
    /// # Safety
    ///
    /// Sound because `T` appears only in `PhantomData`, so every
    /// `Local<'s, T>` has the same layout regardless of `T`. It is unsafe only
    /// because it produces a shared reference to the reinterpreted value; the
    /// caller must not use it as a tag whose payload variant differs.
    pub(crate) fn cast_ref<U>(&self) -> &Local<'s, U> {
        // SAFETY: as documented above — layout does not depend on the tag.
        unsafe { &*(self as *const Self).cast::<Local<'s, U>>() }
    }

    /// The engine value behind the handle, as a `Value`.
    pub(crate) fn into_engine(self) -> api::Local {
        match self.payload {
            Payload::Value(value) => value,
            _ => panic!("bridge bug: a non-value handle read as a value"),
        }
    }
}

impl<T> Clone for Local<'_, T> {
    fn clone(&self) -> Self {
        Self::from_payload(self.payload.clone())
    }
}

impl<T> fmt::Debug for Local<'_, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.payload, f)
    }
}

impl<T> PartialEq for Local<'_, T> {
    fn eq(&self, other: &Self) -> bool {
        match (&self.payload, &other.payload) {
            (Payload::Value(a), Payload::Value(b)) => a == b,
            // One context per isolate: equal payloads are equal realms.
            (Payload::Context(a), Payload::Context(b)) => Rc::ptr_eq(a, b),
            (Payload::Script(a), Payload::Script(b)) => a == b,
            _ => false,
        }
    }
}

/// A handle that may be absent (`v8::MaybeLocal`).
///
/// Absence means the operation failed and a pending exception was set, which is
/// the same information the crate we stand in for carries in an empty handle.
#[derive(Debug)]
pub struct MaybeLocal<'s, T>(Option<Local<'s, T>>);

impl<'s, T> MaybeLocal<'s, T> {
    pub fn is_empty(&self) -> bool {
        self.0.is_none()
    }

    pub fn to_local(&self) -> Option<Local<'s, T>> {
        self.0.clone()
    }

    /// The handle, panicking when the operation failed — the crate we stand in
    /// for's convention: the caller asserts the operation cannot fail.
    pub fn to_local_checked(self) -> Local<'s, T> {
        self.0
            .expect("MaybeLocal::to_local_checked on an empty handle")
    }

    pub fn from_maybe(local: Local<'s, T>) -> Self {
        Self(Some(local))
    }
}

impl<'s, T> Clone for MaybeLocal<'s, T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl<'s, T> From<Local<'s, T>> for MaybeLocal<'s, T> {
    fn from(local: Local<'s, T>) -> Self {
        Self(Some(local))
    }
}

impl<'s, T> From<Option<Local<'s, T>>> for MaybeLocal<'s, T> {
    fn from(local: Option<Local<'s, T>>) -> Self {
        Self(local)
    }
}

impl<'s, T> Default for MaybeLocal<'s, T> {
    fn default() -> Self {
        Self(None)
    }
}

/// A persistent handle: keeps a value alive for as long as the handle exists
/// (`v8::Global`).
///
/// "Persistent" has to mean it. A `Global` lives in host memory the
/// conservative stack scan cannot see, so it holds a pin: the value is a root
/// of every collection until the handle is dropped. Without one, a collection
/// could free the box and the handle would silently alias whatever reused the
/// address.
pub struct Global<T> {
    payload: Payload,
    /// Held only for its `Drop`; releasing the pin is the whole of its API.
    #[allow(dead_code)]
    pin: Option<crux::heap::Pin>,
    marker: PhantomData<fn() -> T>,
}

impl<T> Global<T> {
    /// A persistent handle over a scoped one.
    pub fn new<H: Handle<Data = T>>(_isolate: &crate::Isolate, handle: H) -> Self {
        Self::from_payload(handle.into_payload())
    }

    fn from_payload(payload: Payload) -> Self {
        let pin = payload
            .as_value_opt()
            .map(|value| crux::heap::pin(*value.value()));
        Self {
            payload,
            pin,
            marker: PhantomData,
        }
    }

    /// An empty handle (`v8::Global::Empty`).
    pub fn empty() -> Self {
        Self::from_payload(Payload::Value(api::Local::undefined()))
    }

    pub fn is_empty(&self) -> bool {
        match &self.payload {
            Payload::Value(value) => value.is_undefined(),
            Payload::Context(_) | Payload::Script(_) => false,
        }
    }

    /// The scoped handle for this persistent one.
    pub fn get<'s>(&self, _scope: &PinScope<'s, '_, ()>) -> Local<'s, T> {
        Local::from_payload(self.payload.clone())
    }

    pub fn reset(&mut self) {
        *self = Self::from_payload(Payload::Value(api::Local::undefined()));
    }
}

impl<T> Clone for Global<T> {
    fn clone(&self) -> Self {
        Self::from_payload(self.payload.clone())
    }
}

impl<T> fmt::Debug for Global<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&self.payload, f)
    }
}

impl<T> Default for Global<T> {
    fn default() -> Self {
        Self::empty()
    }
}

/// Anything a [`Local`] can be built from.
///
/// The crate we stand in for exposes the same trait so that its own `Local`
/// constructor can accept both scoped and persistent handles.
pub trait Handle: Sized {
    type Data;

    #[doc(hidden)]
    fn into_payload(self) -> Payload;
}

impl<T> Handle for Local<'_, T> {
    type Data = T;

    fn into_payload(self) -> Payload {
        self.payload
    }
}

impl<T> Handle for &Local<'_, T> {
    type Data = T;

    fn into_payload(self) -> Payload {
        self.payload.clone()
    }
}

impl<T> Handle for Global<T> {
    type Data = T;

    fn into_payload(self) -> Payload {
        self.payload
    }
}

impl<T> Handle for &Global<T> {
    type Data = T;

    fn into_payload(self) -> Payload {
        self.payload.clone()
    }
}
