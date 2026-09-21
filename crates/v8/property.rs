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

    /// Whether every attribute in `that` is set here.
    pub fn has(&self, that: Self) -> bool {
        let Self(lhs) = self;
        let Self(rhs) = that;
        lhs & rhs == rhs
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
