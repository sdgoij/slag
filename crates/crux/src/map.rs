//! Map (hidden class / shape descriptor).
//!
//! Part B: map-based object model. A Map describes the shape of an object —
//! its property descriptors (name, field offset, attributes), prototype, and
//! the transition tree to child maps created by adding new properties.
//!
//! Two objects with the same shape share one Map. Maps are immutable shapes;
//! mutations fork to a child map via the transition tree.
//!
//! B5.1 status: parallel shape, no storage change. `JsObject.map` holds a
//! handle; reads/writes stay through `SmallProps`.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::handle::Handle;
use crate::heap::{GcAny, Trace};
use crate::object::JsObject;
use crate::property::PropertyKey;

/// A map descriptor: the name, field offset, and property attributes.
///
/// Attributes are stored as a bitmask: bit 0 = writable, bit 1 = enumerable,
/// bit 2 = configurable (matching PropertyDescriptor semantics).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MapAttrs(u8);

impl MapAttrs {
    pub const fn new(writable: bool, enumerable: bool, configurable: bool) -> Self {
        Self((writable as u8) | ((enumerable as u8) << 1) | ((configurable as u8) << 2))
    }

    pub fn writable(self) -> bool {
        self.0 & 1 != 0
    }

    pub fn enumerable(self) -> bool {
        self.0 & 2 != 0
    }

    pub fn configurable(self) -> bool {
        self.0 & 4 != 0
    }
}

/// A hidden class descriptor: (property key, field offset, attributes).
pub type MapEntry = (PropertyKey, usize, MapAttrs);

/// A map (V8 hidden class): the shape of objects that share this map.
///
/// Maps are heap-allocated and identity-compared. Two objects with the same
/// shape share one Map.
pub struct Map {
    /// Unique identity for this map.
    id: u64,
    /// Ordered property descriptors. Field offsets are indices into the
    /// in-object `in_fields` region (not yet allocated — stored as the
    /// descriptor offset for now).
    descriptors: Vec<MapEntry>,
    /// Prototype of objects described by this map.
    prototype: Option<Handle<JsObject>>,
    /// Transition tree: property name → child map created when that property
    /// is added.
    transitions: std::collections::HashMap<PropertyKey, Handle<Map>>,
    /// Back-pointer to the parent map in the transition tree.
    back_pointer: Option<Handle<Map>>,
    /// Generation counter: bumped when this map's shape changes.
    generation: Cell<u32>,
}

/// Global monotonic counter for map identities.
static MAP_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

impl PartialEq for Map {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}
impl Eq for Map {}

impl Trace for Map {
    fn trace(&self, visit: &mut dyn FnMut(GcAny)) {
        if let Some(proto) = self.prototype {
            proto.trace(visit);
        }
        for child in self.transitions.values() {
            child.trace(visit);
        }
        if let Some(bp) = self.back_pointer {
            bp.trace(visit);
        }
    }
}

impl Map {
    /// Create a new map describing a prototype-less object.
    pub fn new_empty(prototype: Option<Handle<JsObject>>) -> Handle<Self> {
        Self::new(prototype, None)
    }

    /// Create a new map with a back-pointer to its parent.
    pub fn new(
        prototype: Option<Handle<JsObject>>,
        back_pointer: Option<Handle<Map>>,
    ) -> Handle<Self> {
        Handle::new(Self {
            id: MAP_ID_COUNTER.fetch_add(1, Ordering::Relaxed),
            descriptors: Vec::new(),
            prototype,
            transitions: std::collections::HashMap::new(),
            back_pointer,
            generation: Cell::new(0),
        })
    }

    /// Return the map's unique identity.
    pub fn id(&self) -> u64 {
        self.id
    }

    /// The number of properties described by this map.
    pub fn descriptor_count(&self) -> usize {
        self.descriptors.len()
    }

    /// Whether this map describes a plain, empty object (no properties).
    pub fn is_empty(&self) -> bool {
        self.descriptors.is_empty()
    }

    /// Look up a property key in this map's descriptors.
    pub fn find(&self, key: &PropertyKey) -> Option<usize> {
        self.descriptors.iter().position(|(k, _, _)| k == key)
    }

    /// Return the field offset for a property key in this map's descriptors,
    /// or `None` if the key is not described by this map.
    pub fn field_offset(&self, key: &PropertyKey) -> Option<usize> {
        self.descriptors
            .iter()
            .find_map(|(k, offset, _)| if k == key { Some(*offset) } else { None })
    }

    /// Add a property descriptor, returning the assigned field offset.
    pub fn add_descriptor(&mut self, key: PropertyKey, _offset: usize, attrs: MapAttrs) -> usize {
        let field = self.descriptors.len();
        self.descriptors.push((key, field, attrs));
        self.generation.set(self.generation.get().wrapping_add(1));
        field
    }

    /// Fork this map with a new property: create a child map with the
    /// descriptor added, and cache it in the transition tree.
    pub fn add_transition(
        &mut self,
        key: PropertyKey,
        _offset: usize,
        attrs: MapAttrs,
    ) -> Handle<Map> {
        if let Some(child) = self.transitions.get(&key) {
            return *child;
        }
        // Create a back-pointer handle to *this* allocation (not a new box).
        // GcBox header is mark(1) + padding(3) + size(4) = 8 bytes before data.
        let header_offset: usize = 8;
        let back = unsafe {
            Handle::<Map>::from_box_ptr((&*self as *const Map as usize).wrapping_sub(header_offset))
        };
        let mut child = Map::new(self.prototype, Some(back));
        // The child describes the parent's whole shape plus the new key
        // (`add_descriptor` assigns the next offset, so the parent's
        // descriptor count is the child's field offset for the new key).
        child.descriptors = self.descriptors.clone();
        child.add_descriptor(key.clone(), 0, attrs);
        self.transitions.insert(key, child);
        child
    }

    /// Get or create a child map for a new property key. Returns `None`
    /// if the map already has `INLINE_FIELDS` descriptors (in-field capacity
    /// exhausted — the object will drop to dictionary mode).
    pub fn get_or_create_child(
        &mut self,
        key: PropertyKey,
        attrs: MapAttrs,
    ) -> Option<Handle<Map>> {
        if let Some(child) = self.transitions.get(&key) {
            return Some(*child);
        }
        if self.descriptors.len() >= crate::object::INLINE_FIELDS {
            return None;
        }
        // GcBox header is mark(1) + padding(3) + size(4) = 8 bytes before data.
        let header_offset: usize = 8;
        let back = unsafe {
            Handle::<Map>::from_box_ptr((&*self as *const Map as usize).wrapping_sub(header_offset))
        };
        let mut child = Map::new(self.prototype, Some(back));
        // The child describes the parent's whole shape plus the new key
        // (`add_descriptor` assigns the next offset, so the parent's
        // descriptor count is the child's field offset for the new key).
        child.descriptors = self.descriptors.clone();
        child.add_descriptor(key.clone(), 0, attrs);
        self.transitions.insert(key, child);
        Some(child)
    }

    /// Get the generation counter (for IC invalidation).
    pub fn generation(&self) -> u32 {
        self.generation.get()
    }
}

thread_local! {
    static EMPTY_MAP_CACHE: std::cell::RefCell<
        Vec<(Option<u64>, Handle<Map>)>,
    > = const { std::cell::RefCell::new(Vec::new()) };
}

/// Get or create a canonical empty map for the given prototype.
///
/// Uses prototype identity (via `id()`) as the cache key. Objects with the
/// same prototype share one empty map; objects with different prototypes
/// (including null vs. null) get separate maps.
pub fn canonical_empty_map(prototype: Option<Handle<JsObject>>) -> Handle<Map> {
    let key = prototype.map(|p| p.id());
    EMPTY_MAP_CACHE.with(|cache| {
        let mut map_cache = cache.borrow_mut();
        if let Some((_, handle)) = map_cache.iter().find(|(k, _)| *k == key) {
            return *handle;
        }
        let handle = Map::new_empty(prototype);
        map_cache.push((key, handle));
        handle
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_attrs_bits() {
        let a = MapAttrs::new(true, false, false);
        assert!(a.writable());
        assert!(!a.enumerable());
        assert!(!a.configurable());

        let b = MapAttrs::new(false, true, false);
        assert!(!b.writable());
        assert!(b.enumerable());
        assert!(!b.configurable());
    }

    #[test]
    fn empty_map_is_empty() {
        let map = Map::new_empty(None);
        assert!(map.is_empty());
        assert_eq!(map.descriptor_count(), 0);
        assert_eq!(map.find(&PropertyKey::from_utf8("x")), None);
    }

    #[test]
    fn add_descriptor() {
        let mut map = Map::new_empty(None);
        let key = PropertyKey::from_utf8("x");
        let offset = map.add_descriptor(key.clone(), 0, MapAttrs::new(true, true, true));
        assert_eq!(offset, 0);
        assert_eq!(map.descriptor_count(), 1);
        assert_eq!(map.find(&key), Some(0));
    }

    #[test]
    fn add_transition_caches() {
        let mut map = Map::new_empty(None);
        let child1 = map.add_transition(
            PropertyKey::from_utf8("a"),
            0,
            MapAttrs::new(true, true, true),
        );
        let child2 = map.add_transition(
            PropertyKey::from_utf8("a"),
            0,
            MapAttrs::new(true, true, true),
        );
        assert_eq!(child1.id(), child2.id());
        assert_eq!(map.transitions.len(), 1);
    }

    #[test]
    fn child_inherits_parent_descriptors() {
        let mut map = Map::new_empty(None);
        let x = PropertyKey::from_utf8("x");
        let mut child1 = map
            .get_or_create_child(x.clone(), MapAttrs::new(true, true, true))
            .unwrap();
        // Child 1 describes x at offset 0.
        assert_eq!(child1.field_offset(&x), Some(0));
        let y = PropertyKey::from_utf8("y");
        let child2 = child1
            .get_or_create_child(y.clone(), MapAttrs::new(true, true, true))
            .unwrap();
        // Child 2 describes the parent's whole shape plus y — the child
        // inherits the descriptors, so the new key gets the next offset.
        assert_eq!(child2.field_offset(&x), Some(0));
        assert_eq!(child2.field_offset(&y), Some(1));
        assert_eq!(child2.descriptor_count(), 2);
        // The transition is cached: a second fork returns the same child.
        let again = child1
            .get_or_create_child(y.clone(), MapAttrs::new(true, true, true))
            .unwrap();
        assert_eq!(child2.id(), again.id());
    }

    #[test]
    fn inline_field_capacity_limits_transitions() {
        let mut current = Map::new_empty(None);
        for i in 0..crate::object::INLINE_FIELDS {
            let key = PropertyKey::from_utf8(&format!("k{i}"));
            let child = current
                .get_or_create_child(key, MapAttrs::new(true, true, true))
                .unwrap();
            assert_eq!(child.descriptor_count(), i + 1);
            current = child;
        }
        // One more than the inline field capacity: no transition — the
        // object drops to dictionary mode.
        let overflow = PropertyKey::from_utf8(&format!("k{}", crate::object::INLINE_FIELDS));
        assert!(
            current
                .get_or_create_child(overflow, MapAttrs::new(true, true, true))
                .is_none()
        );
    }

    #[test]
    fn canonical_empty_map_caches() {
        let proto = JsObject::ordinary_object_create(None);
        let m1 = canonical_empty_map(Some(proto));
        let m2 = canonical_empty_map(Some(proto));
        assert_eq!(m1.id(), m2.id());
    }

    #[test]
    fn canonical_empty_map_null_proto() {
        let m1 = canonical_empty_map(None);
        let m2 = canonical_empty_map(None);
        assert_eq!(m1.id(), m2.id());
    }

    #[test]
    fn canonical_empty_map_different_protos() {
        let proto1 = JsObject::ordinary_object_create(None);
        let proto2 = JsObject::ordinary_object_create(None);
        let m1 = canonical_empty_map(Some(proto1));
        let m2 = canonical_empty_map(Some(proto2));
        // Different prototypes → different maps
        assert_ne!(m1.id(), m2.id());
    }

    #[test]
    fn generation_bumps() {
        let mut map = Map::new_empty(None);
        let gen0 = map.generation();
        map.add_descriptor(
            PropertyKey::from_utf8("x"),
            0,
            MapAttrs::new(true, true, true),
        );
        assert!(map.generation() > gen0);
    }
}
