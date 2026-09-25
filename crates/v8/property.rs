//! Property and key-collection types (`v8::PropertyFilter`,
//! `GetPropertyNamesArgsBuilder`, and the enums they carry).
//!
//! These are pure vocabulary: the crate we stand in for defines them as
//! argument and filter structs passed to `Object::GetOwnPropertyNames`, and
//! they are transcribed here so a host's call sites type-check. The operation
//! they describe is not implemented yet — see [`Object`](crate::Object)'s
//! property-name methods.

use std::ops::{BitOr, BitOrAssign};

/// The attributes a property is defined with (v8::PropertyAttribute).
#[repr(C)]
#[derive(Debug, Eq, PartialEq, Clone, Copy, Default)]
pub struct PropertyAttribute(u32);

impl PropertyAttribute {
    /// No attribute: writable, enumerable and configurable.
    pub const NONE: Self = Self(0);
    pub const READ_ONLY: Self = Self(1 << 0);
    pub const DONT_ENUM: Self = Self(1 << 1);
    pub const DONT_DELETE: Self = Self(1 << 2);

    /// Whether any attribute in `that` is set here, which is the crate we stand
    /// in for's rule (`0 != lhs & rhs`). A single-bit mask makes "any" and
    /// "every" the same question; they part on a compound one, where a host
    /// asking whether one of `READ_ONLY | DONT_ENUM` is set means exactly that.
    pub fn has(&self, that: Self) -> bool {
        let Self(lhs) = self;
        let Self(rhs) = that;
        lhs & rhs != 0
    }

    /// Whether no attribute is set.
    pub fn is_none(&self) -> bool {
        *self == Self::NONE
    }

    /// Whether the property is read-only.
    pub fn is_read_only(&self) -> bool {
        self.has(Self::READ_ONLY)
    }

    /// Whether the property is non-enumerable.
    pub fn is_dont_enum(&self) -> bool {
        self.has(Self::DONT_ENUM)
    }

    /// Whether the property is non-configurable.
    pub fn is_dont_delete(&self) -> bool {
        self.has(Self::DONT_DELETE)
    }

    /// The raw bits, which is what a host writes into a callback's return slot
    /// (`v8::PropertyAttribute` is `int` there).
    pub fn as_u32(&self) -> u32 {
        let Self(bits) = self;
        *bits
    }
}

impl BitOr for PropertyAttribute {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        let Self(rhs) = rhs;
        let Self(lhs) = self;
        Self(lhs | rhs)
    }
}

impl BitOrAssign for PropertyAttribute {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}

/// A mask selecting which properties an enumeration returns
/// (`v8::PropertyFilter`).
#[repr(C)]
#[derive(Debug, Eq, PartialEq, Clone, Copy)]
pub struct PropertyFilter(u32);

impl PropertyFilter {
    pub const ALL_PROPERTIES: Self = Self(0);
    pub const ONLY_WRITABLE: Self = Self(1 << 0);
    pub const ONLY_ENUMERABLE: Self = Self(1 << 1);
    pub const ONLY_CONFIGURABLE: Self = Self(1 << 2);
    pub const SKIP_STRINGS: Self = Self(1 << 3);
    pub const SKIP_SYMBOLS: Self = Self(1 << 4);

    pub fn is_all_properties(&self) -> bool {
        *self == Self::ALL_PROPERTIES
    }

    pub fn is_only_writable(&self) -> bool {
        self.has(Self::ONLY_WRITABLE)
    }

    pub fn is_only_enumerable(&self) -> bool {
        self.has(Self::ONLY_ENUMERABLE)
    }

    pub fn is_only_configurable(&self) -> bool {
        self.has(Self::ONLY_CONFIGURABLE)
    }

    pub fn is_skip_strings(&self) -> bool {
        self.has(Self::SKIP_STRINGS)
    }

    pub fn is_skip_symbols(&self) -> bool {
        self.has(Self::SKIP_SYMBOLS)
    }

    fn has(&self, that: Self) -> bool {
        let Self(lhs) = self;
        let Self(rhs) = that;
        0 != lhs & rhs
    }
}

impl Default for PropertyFilter {
    /// The default filter, which is no filter at all
    /// (`v8::PropertyFilter`'s own `Default`). Spelled out rather than derived,
    /// as there: `ALL_PROPERTIES` is the zero value, and naming it says what a
    /// host is asking for.
    fn default() -> Self {
        Self::ALL_PROPERTIES
    }
}

impl BitOr for PropertyFilter {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        Self(self.0 | rhs.0)
    }
}

impl BitOrAssign for PropertyFilter {
    fn bitor_assign(&mut self, rhs: Self) {
        self.0 |= rhs.0;
    }
}

/// How a named property handler's callback answers (`v8::Intercepted`).
///
/// A host installs a handler on an object template and each of its callbacks
/// answers with one of these; the bridge is what reads the answer. This type
/// and [`PropertyHandlerFlags`] are the vocabulary of that mechanism: the
/// configuration, the callbacks' signatures, the template's storage for a
/// handler and the engine-side wiring that invokes them are the rest of it, and
/// are not in this bridge yet.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
// The variant names are the crate we stand in for's, so the lint is the price
// of a host's code naming `v8::Intercepted::kYes` unchanged.
#[allow(non_camel_case_types)]
pub enum Intercepted {
    /// The callback did the operation, and the engine must not.
    kYes,
    /// The callback did not do the operation; the engine's own runs.
    kNo,
    /// The callback did not do the operation, and the engine must keep the
    /// property as it already is rather than running its own.
    kYesKeepExisting,
    /// The operation throws: the callback left a pending exception.
    kThrow,
}

/// Which operations a named property handler is consulted for
/// (`v8::PropertyHandlerFlags`).
///
/// Flags are a set, and a host's are what it was handed: deno's own `vm` module
/// installs one with `NON_MASKING | HAS_NO_SIDE_EFFECT`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PropertyHandlerFlags(u32);

impl PropertyHandlerFlags {
    /// No flags.
    pub const NONE: Self = Self(0);
    /// The handler is consulted for properties the object does not own
    /// (`kAllCanRead`).
    pub const ALL_CAN_READ: Self = Self(1 << 0);
    /// A property on the prototype chain takes precedence over the handler
    /// (`kNonMasking`); without it the handler is consulted first.
    pub const NON_MASKING: Self = Self(1 << 1);
    /// Only string keys reach the handler (`kOnlyInterceptStrings`).
    pub const ONLY_INTERCEPT_STRINGS: Self = Self(1 << 2);
    /// The callbacks have no side effects, so the engine may skip calling them
    /// (`kHasNoSideEffect`).
    pub const HAS_NO_SIDE_EFFECT: Self = Self(1 << 3);

    /// Whether every flag in `that` is set here.
    pub fn has(&self, that: Self) -> bool {
        let Self(lhs) = self;
        let Self(rhs) = that;
        lhs & rhs == rhs
    }
}

impl BitOr for PropertyHandlerFlags {
    type Output = Self;

    fn bitor(self, rhs: Self) -> Self::Output {
        let Self(lhs) = self;
        let Self(rhs) = rhs;
        Self(lhs | rhs)
    }
}

impl BitOrAssign for PropertyHandlerFlags {
    fn bitor_assign(&mut self, rhs: Self) {
        *self = *self | rhs;
    }
}

/// How far an enumeration reaches (`v8::KeyCollectionMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum KeyCollectionMode {
    /// Only the object's own properties.
    OwnOnly,
    /// The prototype chain's keys as well.
    IncludePrototypes,
}

/// Whether integer indices are included (`v8::IndexFilter`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum IndexFilter {
    IncludeIndices,
    SkipIndices,
}

/// How a key is spelled in the result (`v8::KeyConversionMode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
pub enum KeyConversionMode {
    /// Integer indices become strings.
    ConvertToString,
    /// Integer indices stay numbers.
    KeepNumbers,
    NoNumbers,
}

/// A built argument block for a property-name enumeration
/// (`v8::GetPropertyNamesArgs`).
pub struct GetPropertyNamesArgs {
    pub mode: KeyCollectionMode,
    pub property_filter: PropertyFilter,
    pub index_filter: IndexFilter,
    pub key_conversion: KeyConversionMode,
}

impl Default for GetPropertyNamesArgs {
    fn default() -> Self {
        Self {
            mode: KeyCollectionMode::IncludePrototypes,
            property_filter: PropertyFilter::ONLY_ENUMERABLE | PropertyFilter::SKIP_SYMBOLS,
            index_filter: IndexFilter::IncludeIndices,
            key_conversion: KeyConversionMode::KeepNumbers,
        }
    }
}

/// Builds a [`GetPropertyNamesArgs`] (`v8::GetPropertyNamesArgsBuilder`).
pub struct GetPropertyNamesArgsBuilder {
    mode: KeyCollectionMode,
    property_filter: PropertyFilter,
    index_filter: IndexFilter,
    key_conversion: KeyConversionMode,
}

impl Default for GetPropertyNamesArgsBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl GetPropertyNamesArgsBuilder {
    pub fn new() -> Self {
        Self {
            mode: KeyCollectionMode::IncludePrototypes,
            property_filter: PropertyFilter::ONLY_ENUMERABLE | PropertyFilter::SKIP_SYMBOLS,
            index_filter: IndexFilter::IncludeIndices,
            key_conversion: KeyConversionMode::KeepNumbers,
        }
    }

    pub fn build(&self) -> GetPropertyNamesArgs {
        GetPropertyNamesArgs {
            mode: self.mode,
            property_filter: self.property_filter,
            index_filter: self.index_filter,
            key_conversion: self.key_conversion,
        }
    }

    pub fn mode(&mut self, mode: KeyCollectionMode) -> &mut Self {
        self.mode = mode;
        self
    }

    pub fn property_filter(&mut self, property_filter: PropertyFilter) -> &mut Self {
        self.property_filter = property_filter;
        self
    }

    pub fn index_filter(&mut self, index_filter: IndexFilter) -> &mut Self {
        self.index_filter = index_filter;
        self
    }

    pub fn key_conversion(&mut self, key_conversion: KeyConversionMode) -> &mut Self {
        self.key_conversion = key_conversion;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A handler's flags are a set: the pair deno's `vm` installs carries both of
    /// its bits, and neither of the two it left out.
    #[test]
    fn handler_flags_compose() {
        let mut flags = PropertyHandlerFlags::NON_MASKING;
        assert!(flags.has(PropertyHandlerFlags::NON_MASKING));
        assert!(!flags.has(PropertyHandlerFlags::HAS_NO_SIDE_EFFECT));
        flags |= PropertyHandlerFlags::HAS_NO_SIDE_EFFECT;
        assert!(flags.has(PropertyHandlerFlags::NON_MASKING));
        assert!(flags.has(PropertyHandlerFlags::HAS_NO_SIDE_EFFECT));
        assert!(!flags.has(PropertyHandlerFlags::ONLY_INTERCEPT_STRINGS));
        assert!(!flags.has(PropertyHandlerFlags::ALL_CAN_READ));
        // `NONE` is no flags at all, and every set contains it.
        assert!(PropertyHandlerFlags::NONE.has(PropertyHandlerFlags::NONE));
        assert!(!PropertyHandlerFlags::NONE.has(PropertyHandlerFlags::NON_MASKING));
        assert_eq!(
            PropertyHandlerFlags::NONE | PropertyHandlerFlags::ALL_CAN_READ,
            PropertyHandlerFlags::ALL_CAN_READ
        );
    }

    /// The crate we stand in for's own assertions about the attribute set, and
    /// the one place its rule shows: `has` asks whether *any* bit of the mask is
    /// set, so a compound mask is true for a set sharing one of its bits rather
    /// than needing all of them.
    #[test]
    fn the_attributes_are_a_set() {
        assert!(PropertyAttribute::NONE.is_none());
        assert!(!PropertyAttribute::NONE.is_read_only());
        assert!(!PropertyAttribute::NONE.is_dont_enum());
        assert!(!PropertyAttribute::NONE.is_dont_delete());

        assert!(!PropertyAttribute::READ_ONLY.is_none());
        assert!(PropertyAttribute::READ_ONLY.is_read_only());
        assert!(!PropertyAttribute::READ_ONLY.is_dont_enum());
        assert!(!PropertyAttribute::READ_ONLY.is_dont_delete());

        assert!(PropertyAttribute::DONT_ENUM.is_dont_enum());
        assert!(PropertyAttribute::DONT_DELETE.is_dont_delete());

        assert_eq!(PropertyAttribute::NONE, PropertyAttribute::default());
        assert_eq!(
            PropertyAttribute::READ_ONLY,
            PropertyAttribute::NONE | PropertyAttribute::READ_ONLY
        );

        let attr = PropertyAttribute::READ_ONLY | PropertyAttribute::DONT_ENUM;
        assert!(!attr.is_none());
        assert!(attr.is_read_only());
        assert!(attr.is_dont_enum());
        assert!(!attr.is_dont_delete());
        assert_eq!(attr.as_u32(), 0b011);

        // The mask is "any of these", not "all of these".
        assert!(attr.has(PropertyAttribute::READ_ONLY | PropertyAttribute::DONT_DELETE));
        assert!(!attr.has(PropertyAttribute::DONT_DELETE));
        assert!(!PropertyAttribute::NONE.has(PropertyAttribute::READ_ONLY));
    }
}
