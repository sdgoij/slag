//! Map (hidden class / shape descriptor).
//!
//! Part B: map-based object model. A Map describes the shape of an object —
//! its property descriptors (name, field offset, attributes), prototype, and
//! the transition tree to child maps created by adding new properties.
//!
//! Two objects with the same shape share one Map. Maps are immutable shapes;
//! mutations fork to a child map via the transition tree.
//!
//! A descriptor's field offset addresses the object's per-map storage:
//! offsets below `INLINE_FIELDS` index the in-object `in_fields` mirror;
//! offsets at or above it index the object's property vector at the same
//! position (a map describes every default-attributes key, so for any object
//! carrying the map those keys occupy the vector prefix in descriptor order —
//! see `JsObject`'s transition discipline).

use std::cell::Cell;
use std::collections::HashMap;
use std::hash::BuildHasherDefault;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::handle::Handle;
use crate::heap::{FxHasher, GcAny, Trace};
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
    /// Ordered property descriptors. Field offsets below `INLINE_FIELDS`
    /// index the in-object `in_fields` mirror; offsets at or above it index
    /// the object's property vector (the descriptor ordinal is the vector
    /// slot for every map-carrying object).
    descriptors: Vec<MapEntry>,
    /// Prototype of objects described by this map.
    prototype: Option<Handle<JsObject>>,
    /// Transition tree: property name → child map created when that property
    /// is added. Fx-hashed (Cut 66): the closure-creation path forks the
    /// function/prototype shapes per property append, and the keys are atom
    /// ids / symbol pointers — not attacker-controlled.
    transitions: HashMap<PropertyKey, Handle<Map>, BuildHasherDefault<FxHasher>>,
    /// Back-pointer to the parent map in the transition tree.
    back_pointer: Option<Handle<Map>>,
    /// Generation counter: bumped when this map's shape changes.
    generation: Cell<u32>,
}

/// Global monotonic counter for map identities.
static MAP_ID_COUNTER: AtomicU64 = AtomicU64::new(1);

/// The byte offset of `Map::id` inside the map box's data region. The JIT's
/// inline shape read loads a `Handle<Map>` from the object's `map` cell,
/// adds `GCBOX_DATA_OFFSET` to reach the map data, and reads the id at this
/// offset — the machine shape-compare pins the descriptor layout on the id.
/// `pub` because the `id` field itself is private.
pub const MAP_ID_OFFSET: usize = std::mem::offset_of!(Map, id);

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
        // Descriptor and transition keys are `PropertyKey`s; the Symbol
        // variant holds a GC handle, so the keys are heap edges too.
        for entry in &self.descriptors {
            entry.0.trace(visit);
        }
        for (key, child) in &self.transitions {
            key.trace(visit);
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
            transitions: HashMap::default(),
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

    /// Get or create a child map for a new property key. A map describes
    /// every key its objects append (there is no descriptor cap): the new
    /// key's offset is the parent's descriptor count, and the caller is
    /// responsible for appending the key at that vector position (the
    /// L1c transition discipline on `JsObject`). Returns `None` only when
    /// the caller passes a `&mut` to a map that cannot fork.
    pub fn get_or_create_child(
        &mut self,
        key: PropertyKey,
        attrs: MapAttrs,
    ) -> Option<Handle<Map>> {
        if let Some(child) = self.transitions.get(&key) {
            return Some(*child);
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
    // The cache lookup must not hold its borrow across `Map::new_empty`: a
    // `--gc-stress` collection fires inside the allocation, and the sweep's
    // cache prune (`drop_unmarked_empty_maps`) needs the borrow.
    let cached = EMPTY_MAP_CACHE.with(|cache| {
        cache
            .borrow()
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, handle)| *handle)
    });
    if let Some(handle) = cached {
        return handle;
    }
    let handle = Map::new_empty(prototype);
    EMPTY_MAP_CACHE.with(|cache| cache.borrow_mut().push((key, handle)));
    handle
}

/// Drop the cached canonical empty maps whose boxes the just-finished mark
/// will sweep. The cache is the only owner of an empty map once the last
/// live object using it dies, and the collector does not trace the cache, so
/// an unpruned entry would dangle into every later object creation with that
/// prototype (`Heap::collect_from_work` calls this with the final mark bits
/// still set, right before the sweep frees the unmarked boxes). An entry
/// whose box survived the mark stays valid: boxes are only freed inside a
/// collection, so a surviving entry cannot go stale before the next sweep.
pub(crate) fn drop_unmarked_empty_maps() {
    EMPTY_MAP_CACHE.with(|cache| {
        cache
            .borrow_mut()
            .retain(|(_, map)| map.as_any().is_marked());
    });
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
    fn transitions_continue_past_the_inline_field_capacity() {
        // Maps describe every default key an object appends — there is no
        // 4-descriptor cap. Offsets stay ordinal-consistent (the descriptor
        // count at fork time) so the map pins each key's vector slot.
        let mut current = Map::new_empty(None);
        let limit = crate::object::INLINE_FIELDS + 3;
        for i in 0..limit {
            let key = PropertyKey::from_utf8(&format!("k{i}"));
            let child = current
                .get_or_create_child(key.clone(), MapAttrs::new(true, true, true))
                .unwrap();
            assert_eq!(child.descriptor_count(), i + 1);
            // The new key's offset is its descriptor ordinal; earlier keys
            // keep theirs (children inherit the parent's descriptors).
            assert_eq!(child.field_offset(&key), Some(i));
            current = child;
        }
        assert_eq!(current.descriptor_count(), limit);
        assert_eq!(current.field_offset(&PropertyKey::from_utf8("k0")), Some(0));
        assert_eq!(
            current.field_offset(&PropertyKey::from_utf8(&format!("k{}", limit - 1))),
            Some(limit - 1)
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
