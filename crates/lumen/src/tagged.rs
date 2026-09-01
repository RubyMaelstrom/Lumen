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

/// Canonical bytecode identity used by feedback, safepoints, and deoptimization records. Native
/// instruction addresses are deliberately not part of this identity.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct BytecodePc(u32);

impl BytecodePc {
    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }

    pub(crate) const fn get(self) -> u32 {
        self.0
    }
}

/// Stable index of one published safepoint or deoptimization record.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct SafepointId(u32);

impl SafepointId {
    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct DeoptId(u32);

impl DeoptId {
    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FrameMapError {
    SlotBitmapTooLong,
    OverlappingRootClasses,
    SlotOutOfRange,
    OperandDepthOutOfRange,
    InvalidTaggedWord,
    FrameLengthMismatch,
}

/// Immutable root metadata for one exact execution point. Bitmaps address logical frame slots;
/// the generated tier may keep those slots in registers or spills as long as it publishes this
/// same logical map before a safepoint.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RootMap {
    pub(crate) safepoint: SafepointId,
    pub(crate) bytecode_pc: BytecodePc,
    pub(crate) slot_count: u16,
    pub(crate) operand_depth: u16,
    pub(crate) tagged_registers: u64,
    pub(crate) tagged_slots: Vec<u64>,
    pub(crate) handle_slots: Vec<u64>,
    pub(crate) environment_slots: Vec<u64>,
}

impl RootMap {
    fn bitmap_words(slot_count: u16) -> usize {
        usize::from(slot_count).div_ceil(64)
    }

    fn check_bitmap(bits: &[u64], slot_count: u16) -> Result<(), FrameMapError> {
        let expected = Self::bitmap_words(slot_count);
        if bits.len() != expected {
            return Err(FrameMapError::SlotBitmapTooLong);
        }
        let excess = expected
            .checked_mul(64)
            .and_then(|count| count.checked_sub(usize::from(slot_count)))
            .unwrap_or(0);
        if excess != 0 && bits.last().is_some_and(|word| word >> (64 - excess) != 0) {
            return Err(FrameMapError::SlotOutOfRange);
        }
        Ok(())
    }

    pub(crate) fn validate(&self) -> Result<(), FrameMapError> {
        if self.operand_depth > self.slot_count {
            return Err(FrameMapError::OperandDepthOutOfRange);
        }
        Self::check_bitmap(&self.tagged_slots, self.slot_count)?;
        Self::check_bitmap(&self.handle_slots, self.slot_count)?;
        Self::check_bitmap(&self.environment_slots, self.slot_count)?;
        for ((tagged, handles), environments) in self
            .tagged_slots
            .iter()
            .zip(&self.handle_slots)
            .zip(&self.environment_slots)
        {
            if tagged & handles != 0 || tagged & environments != 0 || handles & environments != 0 {
                return Err(FrameMapError::OverlappingRootClasses);
            }
        }
        Ok(())
    }

    fn is_root_slot(&self, slot: usize) -> bool {
        let word = slot / 64;
        let bit = 1u64 << (slot % 64);
        self.tagged_slots[word] & bit != 0
            || self.handle_slots[word] & bit != 0
            || self.environment_slots[word] & bit != 0
    }
}

/// A temporary tagged shadow frame used by the ABI verifier and migration tests. It has no
/// connection to the live `Value` frames until a complete relocation implementation exists.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TaggedFrame {
    slots: Vec<TaggedValue>,
}

impl TaggedFrame {
    pub(crate) fn new(slots: Vec<TaggedValue>) -> Self {
        Self { slots }
    }

    pub(crate) fn roots(&self, map: &RootMap) -> Result<Vec<TaggedValue>, FrameMapError> {
        map.validate()?;
        if self.slots.len() != usize::from(map.slot_count) {
            return Err(FrameMapError::FrameLengthMismatch);
        }
        let mut roots = Vec::new();
        for (slot, value) in self.slots.iter().enumerate() {
            if !map.is_root_slot(slot) {
                continue;
            }
            value
                .validate()
                .map_err(|_| FrameMapError::InvalidTaggedWord)?;
            roots.push(*value);
        }
        Ok(roots)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum MaterializationRecipe {
    CopySlot(u16),
    Constant(TaggedValue),
    BoxNumber(u16),
    Handle(u16),
    Duplicate(usize),
    VirtualObject { layout: u32, fields: Vec<usize> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DeoptRecordError {
    InvalidRootMap(FrameMapError),
    ValueCountOverflow,
    RecipeDependencyOutOfOrder,
    RecipeSlotOutOfRange,
    InvalidConstant,
}

/// Deoptimization metadata is validated before publication. Recipes refer only to prior recipes
/// or logical frame slots, which makes reconstruction deterministic and prevents hidden native
/// pointers from entering a resumed frame.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DeoptRecord {
    pub(crate) id: DeoptId,
    pub(crate) bytecode_pc: BytecodePc,
    pub(crate) root_map: RootMap,
    pub(crate) recipes: Vec<MaterializationRecipe>,
}

impl DeoptRecord {
    pub(crate) fn validate(&self) -> Result<(), DeoptRecordError> {
        self.root_map
            .validate()
            .map_err(DeoptRecordError::InvalidRootMap)?;
        if self.recipes.len() > usize::from(u16::MAX) {
            return Err(DeoptRecordError::ValueCountOverflow);
        }
        let slots = self.root_map.slot_count;
        for (index, recipe) in self.recipes.iter().enumerate() {
            match recipe {
                MaterializationRecipe::CopySlot(slot)
                | MaterializationRecipe::BoxNumber(slot)
                | MaterializationRecipe::Handle(slot)
                    if *slot >= slots =>
                {
                    return Err(DeoptRecordError::RecipeSlotOutOfRange);
                }
                MaterializationRecipe::Constant(value) => value
                    .validate()
                    .map_err(|_| DeoptRecordError::InvalidConstant)?,
                MaterializationRecipe::Duplicate(source) if *source >= index => {
                    return Err(DeoptRecordError::RecipeDependencyOutOfOrder);
                }
                MaterializationRecipe::VirtualObject { fields, .. }
                    if fields.iter().any(|source| *source >= index) =>
                {
                    return Err(DeoptRecordError::RecipeDependencyOutOfOrder);
                }
                _ => {}
            }
        }
        Ok(())
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

    #[test]
    fn root_map_extracts_only_declared_and_validated_slots() {
        let map = RootMap {
            safepoint: SafepointId::new(3),
            bytecode_pc: BytecodePc::new(17),
            slot_count: 4,
            operand_depth: 2,
            tagged_registers: 0b101,
            tagged_slots: vec![0b0001],
            handle_slots: vec![0b0010],
            environment_slots: vec![0b1000],
        };
        let frame = TaggedFrame::new(vec![
            TaggedValue::heap(HeapRef::new(1).unwrap()),
            TaggedValue::number(42.0),
            TaggedValue::boolean(true),
            TaggedValue::null(),
        ]);
        assert_eq!(
            frame.roots(&map).unwrap(),
            vec![
                TaggedValue::heap(HeapRef::new(1).unwrap()),
                TaggedValue::number(42.0),
                TaggedValue::null(),
            ]
        );
    }

    #[test]
    fn root_map_rejects_bad_bitmap_and_overlapping_classes() {
        let mut map = RootMap {
            safepoint: SafepointId::new(0),
            bytecode_pc: BytecodePc::new(0),
            slot_count: 1,
            operand_depth: 0,
            tagged_registers: 0,
            tagged_slots: vec![0b11],
            handle_slots: vec![0],
            environment_slots: vec![0],
        };
        assert_eq!(map.validate(), Err(FrameMapError::SlotOutOfRange));
        map.tagged_slots[0] = 1;
        map.handle_slots[0] = 1;
        assert_eq!(map.validate(), Err(FrameMapError::OverlappingRootClasses));
    }

    #[test]
    fn deopt_recipes_are_forward_only_and_validate_constants() {
        let root_map = RootMap {
            safepoint: SafepointId::new(1),
            bytecode_pc: BytecodePc::new(8),
            slot_count: 2,
            operand_depth: 1,
            tagged_registers: 0,
            tagged_slots: vec![0b01],
            handle_slots: vec![0b10],
            environment_slots: vec![0],
        };
        let valid = DeoptRecord {
            id: DeoptId::new(4),
            bytecode_pc: BytecodePc::new(8),
            root_map: root_map.clone(),
            recipes: vec![
                MaterializationRecipe::Constant(TaggedValue::number(-0.0)),
                MaterializationRecipe::Duplicate(0),
                MaterializationRecipe::VirtualObject {
                    layout: 7,
                    fields: vec![0, 1],
                },
            ],
        };
        assert_eq!(valid.validate(), Ok(()));

        let mut bad = valid.clone();
        bad.recipes[1] = MaterializationRecipe::Duplicate(2);
        assert_eq!(
            bad.validate(),
            Err(DeoptRecordError::RecipeDependencyOutOfOrder)
        );

        let mut bad_constant = valid;
        bad_constant.recipes[0] = MaterializationRecipe::Constant(TaggedValue(TAG_BOOLEAN | 2));
        assert_eq!(
            bad_constant.validate(),
            Err(DeoptRecordError::InvalidConstant)
        );
    }
}
