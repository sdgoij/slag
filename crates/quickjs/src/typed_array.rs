//! The `TypedArray`/`ArrayBuffer`/`DataView` facades (rquickjs-shaped).
//!
//! ArrayBuffers own their bytes through the engine's `byteblock` storage; a
//! typed array is an `IntegerIndexed` view over an ArrayBuffer. Construction
//! goes through the engine's own buffer/view constructors, so range and
//! alignment checks match what `new` in a script would enforce.

pub use crux::typed_array::ElementType;

use crux::error::{ErrorKind, JsError};
use crux::object::ObjectKind;
use crux::typed_array::SharedBuffer;
use runtime::api::{self, Local};
use runtime::builtins::array_buffer::array_buffer_from_block;
use runtime::builtins::typed_array::typed_array_buffer_path;

use crate::context::Context;
use crate::exception::Error;
use crate::value::Value;

/// A JavaScript ArrayBuffer (rquickjs's `ArrayBuffer`).
#[derive(Debug, Clone, Copy)]
pub struct ArrayBuffer(Local);

impl ArrayBuffer {
    /// Create a zero-filled ArrayBuffer of `len` bytes.
    pub fn new(context: &Context, len: usize) -> Result<Self, Error> {
        let shared = SharedBuffer::new(len);
        context
            .api()
            .with_agent(|agent| array_buffer_from_block(agent, shared, len))
            .map(Local::from_value)
            .map(Self)
            .map_err(Error::from)
    }

    /// Create an ArrayBuffer holding a copy of `bytes`.
    pub fn new_copy(context: &Context, bytes: &[u8]) -> Result<Self, Error> {
        let shared = SharedBuffer::new(bytes.len());
        shared
            .write(0, bytes)
            .map_err(|_| JsError::new(ErrorKind::RangeError, "buffer too small".into()))?;
        context
            .api()
            .with_agent(|agent| array_buffer_from_block(agent, shared, bytes.len()))
            .map(Local::from_value)
            .map(Self)
            .map_err(Error::from)
    }

    /// The buffer's byte length, or 0 if detached.
    pub fn len(&self, context: &Context) -> usize {
        let Some(id) = self.object_id() else {
            return 0;
        };
        context.api().with_agent(|agent| {
            agent.buffer_state(id).map_or(
                0,
                |state| {
                    if state.detached { 0 } else { state.byte_length }
                },
            )
        })
    }

    pub fn is_empty(&self, context: &Context) -> bool {
        self.len(context) == 0
    }

    /// A copy of the buffer's bytes.
    pub fn to_vec(&self, context: &Context) -> Option<Vec<u8>> {
        let id = self.object_id()?;
        context.api().with_agent(|agent| {
            let state = agent.buffer_state(id)?;
            if state.detached {
                return None;
            }
            state.shared.read(0, state.byte_length).ok()
        })
    }

    /// Overwrite the buffer's bytes; fails when `bytes` is longer than the
    /// buffer.
    pub fn copy_from_slice(&self, context: &Context, bytes: &[u8]) -> Result<(), Error> {
        let Some(id) = self.object_id() else {
            return Err(not_an_array_buffer().into());
        };
        context
            .api()
            .with_agent(|agent| {
                let state = agent.buffer_state(id).ok_or_else(not_an_array_buffer)?;
                if state.detached {
                    return Err(not_an_array_buffer());
                }
                state
                    .shared
                    .write(0, bytes)
                    .map_err(|_| JsError::new(ErrorKind::RangeError, "buffer too small".into()))
            })
            .map_err(Error::from)
    }

    /// The value this buffer is.
    pub fn as_value(&self) -> Value {
        Value::from_local(self.0)
    }

    fn object_id(&self) -> Option<u64> {
        self.0.value().as_object().map(|object| object.id())
    }
}

/// A JavaScript DataView (rquickjs's `DataView`).
#[derive(Debug, Clone, Copy)]
pub struct DataView(Local);

impl DataView {
    /// A new view over `buffer` (the `new DataView(buffer, byteOffset,
    /// byteLength)` constructor).
    pub fn new(
        context: &Context,
        buffer: &ArrayBuffer,
        byte_offset: usize,
        byte_length: usize,
    ) -> Result<Self, Error> {
        api::DataView::new(&context.api(), &buffer.0, byte_offset, byte_length)
            .map(Self)
            .map_err(Error::from)
    }

    /// The value this view is.
    pub fn as_value(&self) -> Value {
        Value::from_local(self.0)
    }
}

/// A JavaScript typed array (rquickjs's `TypedArray<T>`), generic over the
/// element type.
#[derive(Debug, Clone, Copy)]
pub struct TypedArray(Local);

impl TypedArray {
    /// A typed array of `length` zero-filled elements.
    pub fn new(context: &Context, element_type: ElementType, length: usize) -> Result<Self, Error> {
        let buffer = ArrayBuffer::new(context, length * element_type.size())?;
        Self::new_buffer(context, element_type, &buffer, 0, length)
    }

    /// A typed array over a region of `buffer`.
    pub fn new_buffer(
        context: &Context,
        element_type: ElementType,
        buffer: &ArrayBuffer,
        byte_offset: usize,
        length: usize,
    ) -> Result<Self, Error> {
        let args = [
            crux::value::Value::Number(byte_offset as f64),
            crux::value::Value::Number(length as f64),
        ];
        context
            .api()
            .with_agent(|agent| {
                let name = format!("%{}Array.prototype%", element_type.name());
                let proto = agent
                    .current_realm()?
                    .intrinsics
                    .get(&name)
                    .and_then(|value| value.as_object())
                    .ok_or_else(|| {
                        JsError::new(ErrorKind::TypeError, format!("{name} is not defined"))
                    })?;
                typed_array_buffer_path(agent, proto, element_type, buffer.0.value(), &args)
            })
            .map(Local::from_value)
            .map(Self)
            .map_err(Error::from)
    }

    /// The view's element type.
    pub fn element_type(&self) -> Option<ElementType> {
        self.slots().map(|(element_type, _, _, _)| element_type)
    }

    /// The view's byte length.
    pub fn byte_length(&self) -> usize {
        self.slots().map_or(0, |(_, _, byte_length, _)| byte_length)
    }

    /// The number of elements in the view.
    pub fn length(&self) -> usize {
        self.slots().map_or(0, |(element_type, _, byte_length, _)| {
            byte_length / element_type.size()
        })
    }

    /// A copy of the view's bytes.
    pub fn to_vec(&self, context: &Context) -> Option<Vec<u8>> {
        let (_, byte_offset, byte_length, buffer_id) = self.slots()?;
        context.api().with_agent(|agent| {
            let state = agent.buffer_state(buffer_id)?;
            if state.detached {
                return None;
            }
            state.shared.read(byte_offset, byte_length).ok()
        })
    }

    /// The value this view is.
    pub fn as_value(&self) -> Value {
        Value::from_local(self.0)
    }

    fn slots(&self) -> Option<(ElementType, usize, usize, u64)> {
        let object = self.0.value().as_object()?;
        let ObjectKind::IntegerIndexed(slots) = &object.kind else {
            return None;
        };
        let buffer_id = slots.buffer_object.as_object()?.id();
        Some((
            slots.element_type,
            slots.byte_offset,
            slots.byte_length,
            buffer_id,
        ))
    }
}

fn not_an_array_buffer() -> JsError {
    JsError::new(ErrorKind::TypeError, "value is not an ArrayBuffer".into())
}
