//! BigInts (`v8::BigInt`).

use crux::bigint;
use crux::value::{Value, ValueKind};
use runtime::api;

use crate::data::BigInt;
use crate::handle::Local;
use crate::scope::PinScope;

impl BigInt {
    /// A BigInt from a signed 64-bit integer (`v8::BigInt::NewFromInt64`).
    pub fn new_from_i64<'s>(_scope: &PinScope<'s, '_, ()>, value: i64) -> Local<'s, BigInt> {
        from_engine_int(crux::BigInt::from(value))
    }

    /// A BigInt from an unsigned 64-bit integer
    /// (`v8::BigInt::NewFromUnsigned`).
    pub fn new_from_u64<'s>(_scope: &PinScope<'s, '_, ()>, value: u64) -> Local<'s, BigInt> {
        from_engine_int(crux::BigInt::from(value))
    }

    /// A BigInt from a sign bit and little-endian 64-bit words
    /// (`v8::BigInt::NewFromWords`).
    ///
    /// The engine holds the arbitrary-precision integer itself rather than its
    /// words, so this folds them in from the most significant down. It is the
    /// exact value either way, which is what the crate we stand in for promises
    /// here: the only way this fails there is out of memory.
    pub fn new_from_words<'s>(
        _scope: &PinScope<'s, '_, ()>,
        sign_bit: bool,
        words: &[u64],
    ) -> Option<Local<'s, BigInt>> {
        let magnitude = words.iter().rev().fold(crux::BigInt::zero(), |acc, word| {
            bigint::add(&bigint::left_shift(&acc, 64), &crux::BigInt::from(*word))
        });
        let value = if sign_bit {
            bigint::unary_minus(&magnitude)
        } else {
            magnitude
        };
        Some(from_engine_int(value))
    }
}

impl<'s> Local<'s, BigInt> {
    /// The value as an `i64`, and whether that was lossless
    /// (`v8::BigInt::Int64Value`); one that does not fit comes back wrapped
    /// modulo 2^64.
    pub fn i64_value(&self) -> (i64, bool) {
        with_bigint(self.engine(), |int| {
            let wrapped = int.to_i64_wrapping();
            (wrapped, bigint::equal(int, &crux::BigInt::from(wrapped)))
        })
    }

    /// The value as a `u64`, and whether that was lossless
    /// (`v8::BigInt::Uint64Value`).
    pub fn u64_value(&self) -> (u64, bool) {
        with_bigint(self.engine(), |int| match int.to_u64() {
            Some(value) => (value, true),
            None => (int.to_i64_wrapping() as u64, false),
        })
    }

    /// The number of 64-bit words the value takes (`v8::BigInt::WordCount`).
    pub fn word_count(&self) -> usize {
        with_bigint(self.engine(), |int| int.0.to_u64_digits().1.len())
    }

    /// The sign and the little-endian words of the value
    /// (`v8::BigInt::ToWordsArray`), truncated to `words` when there is not
    /// room for all of them, with the part that was written.
    pub fn to_words_array<'a>(&self, words: &'a mut [u64]) -> (bool, &'a mut [u64]) {
        let (negative, digits) = with_bigint(self.engine(), |int| {
            (
                bigint::less_than(int, &crux::BigInt::zero()),
                int.0.to_u64_digits().1,
            )
        });
        let written = digits.len().min(words.len());
        words[..written].copy_from_slice(&digits[..written]);
        (negative, &mut words[..written])
    }
}

/// Run `question` over the integer a handle names.
fn with_bigint<T>(value: &api::Local, question: impl FnOnce(&crux::BigInt) -> T) -> T {
    match value.value().kind() {
        ValueKind::BigInt(int) => question(&int),
        _ => panic!("bridge bug: a BigInt handle that is not a BigInt"),
    }
}

/// The handle for an engine integer.
pub(crate) fn from_engine_int<'s>(value: crux::BigInt) -> Local<'s, BigInt> {
    Local::from_engine(api::Local::from(Value::BigInt(crux::handle::Handle::new(
        value,
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::in_context;

    #[test]
    fn a_signed_value_round_trips_and_reports_losslessness() {
        in_context!(scope, {
            assert_eq!(BigInt::new_from_i64(scope, -7).i64_value(), (-7, true));
            assert_eq!(
                BigInt::new_from_i64(scope, i64::MIN).i64_value(),
                (i64::MIN, true)
            );

            // One past the range wraps, and says so.
            let big = BigInt::new_from_u64(scope, i64::MAX as u64 + 1);
            assert_eq!(big.i64_value(), (i64::MIN, false));
            assert_eq!(big.u64_value(), (i64::MAX as u64 + 1, true));
        });
    }

    #[test]
    fn an_unsigned_value_reports_losslessness() {
        in_context!(scope, {
            assert_eq!(
                BigInt::new_from_u64(scope, u64::MAX).u64_value(),
                (u64::MAX, true)
            );
            assert_eq!(
                BigInt::new_from_i64(scope, -1).u64_value(),
                (u64::MAX, false)
            );
        });
    }

    #[test]
    fn a_value_larger_than_a_word_reports_its_words() {
        in_context!(scope, {
            // 2^64 + 1, written as two words little-endian.
            let value = BigInt::new_from_words(scope, false, &[1, 1]).expect("bigint");
            let mut words = [0u64; 4];
            let (negative, written) = value.to_words_array(&mut words);
            assert!(!negative);
            assert_eq!(written, [1, 1]);
            assert_eq!(value.word_count(), 2);
            assert!(!value.u64_value().1);

            // The words are the value: reading them back gives it again.
            let rebuilt = BigInt::new_from_words(scope, false, written).expect("bigint");
            assert_eq!(rebuilt.word_count(), 2);
            let mut again = [0u64; 2];
            assert_eq!(rebuilt.to_words_array(&mut again).1, [1, 1]);

            let negative = BigInt::new_from_words(scope, true, &[1, 1]).expect("bigint");
            assert!(negative.to_words_array(&mut words).0);
        });
    }

    /// Too small a buffer truncates to its capacity rather than refusing, which
    /// is what the crate we stand in for does.
    #[test]
    fn a_short_word_buffer_is_filled() {
        in_context!(scope, {
            let value = BigInt::new_from_words(scope, false, &[1, 2, 3]).expect("bigint");
            let mut one = [0u64; 1];
            assert_eq!(value.to_words_array(&mut one).1, [1]);
            assert_eq!(value.word_count(), 3);
        });
    }
}
