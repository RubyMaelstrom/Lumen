//! Engine-owned tagged execution words.
//!
//! This is the Phase 2 ABI nucleus. It is intentionally not connected to the current `Value`
//! enum or `PackedValue` storage yet: those still carry `Rc` ownership and need the central heap
//! before a heap reference can be resolved safely. Keeping this module independent lets bit-level
//! invariants and invalid-word handling be tested before any execution tier migrates.

#![allow(dead_code)]

const PAYLOAD_MASK: u64 = 0x0000_ffff_ffff_ffff;
const TAG_MASK: u64 = !PAYLOAD_MASK;
const TAG_CANON_NAN: u64 = 0x7ff8_0000_0000_0000;
const TAG_UNDEFINED: u64 = 0x7ff9_0000_0000_0000;
const TAG_EMPTY: u64 = 0x7ffa_0000_0000_0000;
const TAG_NULL: u64 = 0x7ffb_0000_0000_0000;
const TAG_BOOLEAN: u64 = 0x7ffc_0000_0000_0000;
const TAG_HEAP_REF: u64 = 0xfff9_0000_0000_0000;

/// A non-zero 32-bit offset/index owned by the active Agent's heap model.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct HeapRef(u32);

impl HeapRef {
    /// Construct a reference. Zero is reserved as an invalid/null reference.
    pub(crate) const fn new(raw: u32) -> Option<Self> {
        if raw == 0 {
            None
        } else {
            Some(Self(raw))
        }
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}

/// One engine-owned 64-bit execution value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub(crate) struct TaggedValue(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InvalidTaggedValue {
    ReservedTag,
    InvalidBooleanPayload,
    NullHeapReference,
    WideHeapReference,
    NonCanonicalNan,
}

impl TaggedValue {
    pub(crate) const fn undefined() -> Self {
        Self(TAG_UNDEFINED)
    }

    pub(crate) const fn empty() -> Self {
        Self(TAG_EMPTY)
    }

    pub(crate) const fn null() -> Self {
        Self(TAG_NULL)
    }

    pub(crate) const fn boolean(value: bool) -> Self {
        Self(TAG_BOOLEAN | value as u64)
    }

    /// Preserve all non-NaN IEEE-754 bits, including `-0` and infinities. NaN payloads are
    /// canonicalized because arbitrary quiet-NaN payloads overlap the reserved tag range.
    pub(crate) fn number(value: f64) -> Self {
        Self(if value.is_nan() {
            TAG_CANON_NAN
        } else {
            value.to_bits()
        })
    }

    /// Construct a heap reference without embedding a native pointer. The upper payload bits stay
    /// zero so a future cage offset and the handle-table fallback share one word format.
    pub(crate) fn heap(reference: HeapRef) -> Self {
        Self(TAG_HEAP_REF | reference.get() as u64)
    }

    pub(crate) const fn raw(self) -> u64 {
        self.0
    }

    pub(crate) fn validate(self) -> Result<(), InvalidTaggedValue> {
        let tag = self.0 & TAG_MASK;
        match tag {
            TAG_UNDEFINED | TAG_EMPTY | TAG_NULL => {
                if self.0 & PAYLOAD_MASK == 0 {
                    Ok(())
                } else {
                    Err(InvalidTaggedValue::ReservedTag)
                }
            }
            TAG_BOOLEAN => {
                if self.0 & PAYLOAD_MASK <= 1 {
                    Ok(())
                } else {
                    Err(InvalidTaggedValue::InvalidBooleanPayload)
                }
            }
            TAG_HEAP_REF => {
                if self.0 & 0x0000_ffff_0000_0000 != 0 {
                    return Err(InvalidTaggedValue::WideHeapReference);
                }
                if self.0 & 0xffff_ffff == 0 {
                    Err(InvalidTaggedValue::NullHeapReference)
                } else {
                    Ok(())
                }
            }
            TAG_CANON_NAN => {
                if self.0 == TAG_CANON_NAN {
                    Ok(())
                } else {
                    Err(InvalidTaggedValue::NonCanonicalNan)
                }
            }
            _ if f64::from_bits(self.0).is_nan() => Err(InvalidTaggedValue::NonCanonicalNan),
            // Every non-NaN IEEE-754 bit pattern is a Number, including infinities and
            // subnormals whose exponent bits do not match one of the reserved tagged prefixes.
            _ => Ok(()),
        }
    }

    pub(crate) fn as_boolean(self) -> Option<bool> {
        (self.0 & TAG_MASK == TAG_BOOLEAN)
            .then_some(self.0 & PAYLOAD_MASK)
            .and_then(|payload| match payload {
                0 => Some(false),
                1 => Some(true),
                _ => None,
            })
    }

    pub(crate) fn as_number(self) -> Option<f64> {
        (self.0 & TAG_MASK != TAG_UNDEFINED)
            .then_some(f64::from_bits(self.0))
            .filter(|value| !value.is_nan() || self.0 == TAG_CANON_NAN)
    }

    pub(crate) fn as_heap(self) -> Option<HeapRef> {
        if self.0 & TAG_MASK != TAG_HEAP_REF || self.0 & 0xffff_ffff == 0 {
            return None;
        }
        if self.0 & 0x0000_ffff_0000_0000 != 0 {
            return None;
        }
        HeapRef::new(self.0 as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tagged_word_is_one_word_and_preserves_immediates() {
        assert_eq!(std::mem::size_of::<TaggedValue>(), 8);
        assert_eq!(TaggedValue::undefined().validate(), Ok(()));
        assert_eq!(TaggedValue::empty().validate(), Ok(()));
        assert_eq!(TaggedValue::null().validate(), Ok(()));
        assert_eq!(TaggedValue::boolean(false).as_boolean(), Some(false));
        assert_eq!(TaggedValue::boolean(true).as_boolean(), Some(true));
    }

    #[test]
    fn numbers_preserve_signed_zero_and_infinities_and_canonicalize_nan() {
        for value in [0.0, -0.0, f64::INFINITY, f64::NEG_INFINITY, 42.5] {
            let tagged = TaggedValue::number(value);
            assert_eq!(tagged.validate(), Ok(()));
            assert_eq!(tagged.as_number().unwrap().to_bits(), value.to_bits());
        }
        let nan = TaggedValue::number(f64::from_bits(0x7ff8_0000_0000_0001));
        assert_eq!(nan.raw(), TAG_CANON_NAN);
        assert_eq!(nan.validate(), Ok(()));
        assert!(nan.as_number().unwrap().is_nan());
    }

    #[test]
    fn heap_refs_are_nonzero_32_bit_payloads() {
        assert_eq!(HeapRef::new(0), None);
        let reference = HeapRef::new(u32::MAX).unwrap();
        let tagged = TaggedValue::heap(reference);
        assert_eq!(tagged.validate(), Ok(()));
        assert_eq!(tagged.as_heap(), Some(reference));
    }

    #[test]
    fn invalid_tagged_words_are_rejected() {
        assert_eq!(
            TaggedValue(TAG_BOOLEAN | 2).validate(),
            Err(InvalidTaggedValue::InvalidBooleanPayload)
        );
        assert_eq!(
            TaggedValue(TAG_HEAP_REF).validate(),
            Err(InvalidTaggedValue::NullHeapReference)
        );
        assert_eq!(
            TaggedValue(TAG_HEAP_REF | 0x1_0000_0000).validate(),
            Err(InvalidTaggedValue::WideHeapReference)
        );
        assert_eq!(
            TaggedValue(TAG_CANON_NAN | 1).validate(),
            Err(InvalidTaggedValue::NonCanonicalNan)
        );
    }
}
