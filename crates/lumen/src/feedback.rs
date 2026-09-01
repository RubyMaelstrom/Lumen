//! Versioned, representation-independent per-function feedback layout.
//!
//! See `FEEDBACK_SCHEMA.md`. This module deliberately contains no `Value` discriminants, shape
//! numbers, object addresses, or IC structs. Those belong to versioned runtime adapters.

use std::cell::{Cell, OnceCell};
use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

/// Numeric encodings below are part of the profile schema and may not be reordered in place.
pub(crate) const SCHEMA_VERSION: u16 = 1;

const PROFILE_MAGIC: &[u8; 8] = b"LUMENFB\0";
const PROFILE_FORMAT_VERSION: u16 = 1;
const PROFILE_SCOPE_VECTOR_LOCAL: u8 = 1;
const PROFILE_HEADER_LEN: usize = 48;

static PROCESS_PROFILE_SESSION: OnceLock<u64> = OnceLock::new();
static NEXT_PROFILE_VECTOR_ID: AtomicU64 = AtomicU64::new(1);

fn process_profile_session() -> u64 {
    *PROCESS_PROFILE_SESSION.get_or_init(|| RandomState::new().build_hasher().finish().max(1))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub(crate) struct SiteId(u32);

impl SiteId {
    pub(crate) fn index(self) -> usize {
        self.0 as usize
    }
}

/// The source-level semantic operation represented by one feedback site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum OperationKind {
    NamedLoad = 1,
    NamedStore = 2,
    ElementLoad = 3,
    ElementStore = 4,
    Call = 5,
    Construct = 6,
    Arithmetic = 7,
    Branch = 8,
    Loop = 9,
    Allocation = 10,
}

/// Representation-independent meaning of one observation payload.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ObservationKind {
    ValueClass = 1,
    ReceiverLayout = 2,
    HolderLayout = 3,
    ElementAccess = 4,
    CallTarget = 5,
    BranchCount = 6,
    Allocation = 7,
}

/// Semantic position of an observation within its site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ObservationRole {
    Operand0 = 1,
    Operand1 = 2,
    Result = 3,
    Receiver = 4,
    Holder = 5,
    Access = 6,
    Target = 7,
    Outcome = 8,
}

/// Runtime-only bridge to the current baseline machinery. These indexes are never serialized.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RuntimeBinding {
    Unbound,
    PropertyIc { first_way: u32, way_count: u8 },
}

/// Stable abstract state stored in an observation word. Numeric encodings are schema data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
pub(crate) enum ObservationState {
    Uninitialized = 0,
    Monomorphic = 1,
    Polymorphic = 2,
    Absent = 3,
    Generic = 4,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(transparent)]
pub(crate) struct ObservationWord(u64);

impl ObservationWord {
    pub(crate) const UNINITIALIZED: Self = Self::new(ObservationState::Uninitialized, 0, 0);

    pub(crate) const fn new(state: ObservationState, payload: u32, flags: u8) -> Self {
        Self((state as u64) | ((flags as u64) << 8) | ((payload as u64) << 16))
    }

    fn decoded_state(self) -> Option<ObservationState> {
        Some(match self.0 as u8 {
            0 => ObservationState::Uninitialized,
            1 => ObservationState::Monomorphic,
            2 => ObservationState::Polymorphic,
            3 => ObservationState::Absent,
            4 => ObservationState::Generic,
            _ => return None,
        })
    }

    fn is_valid(self) -> bool {
        self.0 >> 48 == 0 && self.decoded_state().is_some()
    }

    #[cfg(test)]
    pub(crate) fn state(self) -> ObservationState {
        self.decoded_state()
            .expect("invalid in-process observation state")
    }

    #[cfg(test)]
    pub(crate) fn payload(self) -> u32 {
        (self.0 >> 16) as u32
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub(crate) struct SlotDescriptor {
    pub(crate) kind: ObservationKind,
    pub(crate) role: ObservationRole,
}

/// Compact immutable description of one semantic site.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(C)]
pub(crate) struct SiteDescriptor {
    pub(crate) bytecode_pc: u32,
    pub(crate) first_slot: u32,
    pub(crate) operation: OperationKind,
    pub(crate) slot_count: u8,
    reserved: u16,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct FeedbackLayout {
    version: u16,
    sites: Box<[SiteDescriptor]>,
    slots: Box<[SlotDescriptor]>,
}

impl FeedbackLayout {
    #[cfg(test)]
    pub(crate) fn version(&self) -> u16 {
        self.version
    }

    pub(crate) fn len(&self) -> usize {
        self.sites.len()
    }

    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.sites.is_empty()
    }

    pub(crate) fn slot_len(&self) -> usize {
        self.slots.len()
    }

    pub(crate) fn site(&self, id: SiteId) -> Option<&SiteDescriptor> {
        self.sites.get(id.index())
    }

    pub(crate) fn slots(&self, id: SiteId) -> Option<&[SlotDescriptor]> {
        let site = self.site(id)?;
        let start = site.first_slot as usize;
        let end = start.checked_add(site.slot_count as usize)?;
        self.slots.get(start..end)
    }

    pub(crate) fn site_ids(&self) -> impl ExactSizeIterator<Item = SiteId> + '_ {
        (0..self.sites.len()).map(|index| SiteId(index as u32))
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.sites
            .len()
            .saturating_mul(std::mem::size_of::<SiteDescriptor>())
            .saturating_add(
                self.slots
                    .len()
                    .saturating_mul(std::mem::size_of::<SlotDescriptor>()),
            )
    }

    /// Hash only the specified numeric schema fields, never Rust padding or discriminants.
    fn stable_hash(&self) -> u64 {
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x100000001b3;

        fn add(hash: &mut u64, bytes: &[u8]) {
            for byte in bytes {
                *hash ^= u64::from(*byte);
                *hash = hash.wrapping_mul(FNV_PRIME);
            }
        }

        let mut hash = FNV_OFFSET;
        add(&mut hash, &self.version.to_le_bytes());
        add(&mut hash, &(self.sites.len() as u64).to_le_bytes());
        add(&mut hash, &(self.slots.len() as u64).to_le_bytes());
        for site in &self.sites {
            add(&mut hash, &site.bytecode_pc.to_le_bytes());
            add(&mut hash, &site.first_slot.to_le_bytes());
            add(&mut hash, &[site.operation as u8, site.slot_count]);
        }
        for slot in &self.slots {
            add(&mut hash, &[slot.kind as u8, slot.role as u8]);
        }
        hash
    }
}

/// Why a serialized profile was deliberately not consumed.
///
/// These distinctions are part of the fail-closed ingestion contract. Callers may report them,
/// but must not reinterpret or repair incompatible observation words themselves.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)] // consumed by the bounded profile-dump/ingestion slice later in Phase 1
pub(crate) enum ProfileDropReason {
    Truncated { minimum: usize, actual: usize },
    InvalidMagic,
    UnsupportedFormatVersion { found: u16 },
    UnsupportedSchemaVersion { found: u16 },
    UnsupportedScope { found: u8 },
    NonzeroReservedHeader,
    InvalidLength { expected: usize, actual: usize },
    DifferentProcessSession,
    DifferentFeedbackVector,
    LayoutMismatch,
    SiteCountMismatch { found: u32, expected: u32 },
    SlotCountMismatch { found: u32, expected: u32 },
    InvalidObservationWord { slot: u32 },
}

#[derive(Default)]
pub(crate) struct LayoutBuilder {
    sites: Vec<SiteDescriptor>,
    slots: Vec<SlotDescriptor>,
}

impl LayoutBuilder {
    pub(crate) fn add_site(
        &mut self,
        bytecode_pc: usize,
        operation: OperationKind,
        slots: &[SlotDescriptor],
    ) -> SiteId {
        assert!(
            !slots.is_empty(),
            "feedback sites require an observation slot"
        );
        let id = SiteId(
            u32::try_from(self.sites.len()).expect("feedback site identity space exhausted"),
        );
        let first_slot =
            u32::try_from(self.slots.len()).expect("feedback slot identity space exhausted");
        let slot_count =
            u8::try_from(slots.len()).expect("one feedback site has more than 255 slots");
        self.sites.push(SiteDescriptor {
            bytecode_pc: u32::try_from(bytecode_pc).expect("baseline bytecode PC exceeds u32"),
            first_slot,
            operation,
            slot_count,
            reserved: 0,
        });
        self.slots.extend_from_slice(slots);
        id
    }

    pub(crate) fn finish(self) -> FeedbackLayout {
        FeedbackLayout {
            version: SCHEMA_VERSION,
            sites: self.sites.into_boxed_slice(),
            slots: self.slots.into_boxed_slice(),
        }
    }
}

/// The payload is intentionally lazy. Schema construction is always on; detailed observation is
/// opt-in and does not allocate a word array until an adapter requests it.
pub(crate) struct FeedbackVector {
    layout: FeedbackLayout,
    bindings: Box<[RuntimeBinding]>,
    words: OnceCell<Box<[Cell<u64>]>>,
    /// Assigned only when this vector participates in profile serialization or ingestion.
    profile_vector_id: Cell<u64>,
}

impl FeedbackVector {
    pub(crate) fn new(layout: FeedbackLayout, bindings: Box<[RuntimeBinding]>) -> Self {
        assert_eq!(layout.len(), bindings.len());
        Self {
            layout,
            bindings,
            words: OnceCell::new(),
            profile_vector_id: Cell::new(0),
        }
    }

    pub(crate) fn unbound(layout: FeedbackLayout) -> Self {
        let bindings = vec![RuntimeBinding::Unbound; layout.len()].into_boxed_slice();
        Self::new(layout, bindings)
    }

    pub(crate) fn layout(&self) -> &FeedbackLayout {
        &self.layout
    }

    pub(crate) fn retained_bytes(&self) -> usize {
        self.layout
            .retained_bytes()
            .saturating_add(
                self.bindings
                    .len()
                    .saturating_mul(std::mem::size_of::<RuntimeBinding>()),
            )
            .saturating_add(self.words.get().map_or(0, |words| {
                words.len().saturating_mul(std::mem::size_of::<Cell<u64>>())
            }))
    }

    pub(crate) fn sites(&self) -> impl ExactSizeIterator<Item = (SiteId, RuntimeBinding)> + '_ {
        self.layout.site_ids().zip(self.bindings.iter().copied())
    }

    pub(crate) fn write(
        &self,
        site: SiteId,
        kind: ObservationKind,
        role: ObservationRole,
        value: ObservationWord,
    ) {
        let Some(descriptors) = self.layout.slots(site) else {
            return;
        };
        let Some(offset) = descriptors
            .iter()
            .position(|slot| slot.kind == kind && slot.role == role)
        else {
            return;
        };
        let first = self.layout.site(site).unwrap().first_slot as usize;
        self.words()[first + offset].set(value.0);
    }

    fn words(&self) -> &[Cell<u64>] {
        self.words.get_or_init(|| {
            (0..self.layout.slot_len())
                .map(|_| Cell::new(ObservationWord::UNINITIALIZED.0))
                .collect::<Vec<_>>()
                .into_boxed_slice()
        })
    }

    fn profile_vector_id(&self) -> u64 {
        let current = self.profile_vector_id.get();
        if current != 0 {
            return current;
        }
        let assigned = NEXT_PROFILE_VECTOR_ID
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                next.checked_add(1)
            })
            .expect("feedback profile vector identity space exhausted");
        self.profile_vector_id.set(assigned);
        assigned
    }

    /// Serialize a versioned compatibility envelope around this live vector's observations.
    ///
    /// Format version 1 is intentionally vector-local. The payload contains abstract observation
    /// words, but their current layout tokens are meaningful only to this exact vector.
    #[allow(dead_code)] // exposed to the bounded profile-dump slice later in Phase 1
    pub(crate) fn serialize_profile(&self) -> Vec<u8> {
        let site_count =
            u32::try_from(self.layout.len()).expect("feedback profile has more than u32 sites");
        let slot_count = u32::try_from(self.layout.slot_len())
            .expect("feedback profile has more than u32 slots");
        let mut bytes = Vec::with_capacity(
            PROFILE_HEADER_LEN.saturating_add(self.layout.slot_len().saturating_mul(8)),
        );
        bytes.extend_from_slice(PROFILE_MAGIC);
        bytes.extend_from_slice(&PROFILE_FORMAT_VERSION.to_le_bytes());
        bytes.extend_from_slice(&SCHEMA_VERSION.to_le_bytes());
        bytes.push(PROFILE_SCOPE_VECTOR_LOCAL);
        bytes.extend_from_slice(&[0; 3]);
        bytes.extend_from_slice(&process_profile_session().to_le_bytes());
        bytes.extend_from_slice(&self.profile_vector_id().to_le_bytes());
        bytes.extend_from_slice(&self.layout.stable_hash().to_le_bytes());
        bytes.extend_from_slice(&site_count.to_le_bytes());
        bytes.extend_from_slice(&slot_count.to_le_bytes());
        for word in self.words() {
            bytes.extend_from_slice(&word.get().to_le_bytes());
        }
        bytes
    }

    /// Validate and monotonically merge a profile produced by [`Self::serialize_profile`].
    ///
    /// No version-1 mismatch is upgraded heuristically. In particular, matching layout hashes do
    /// not permit data from another vector, because profile-local layout tokens could differ.
    #[allow(dead_code)] // exposed to the bounded profile-ingestion slice later in Phase 1
    pub(crate) fn merge_serialized_profile(&self, bytes: &[u8]) -> Result<(), ProfileDropReason> {
        if bytes.len() < PROFILE_HEADER_LEN {
            return Err(ProfileDropReason::Truncated {
                minimum: PROFILE_HEADER_LEN,
                actual: bytes.len(),
            });
        }
        if &bytes[0..8] != PROFILE_MAGIC {
            return Err(ProfileDropReason::InvalidMagic);
        }

        let format_version = u16::from_le_bytes([bytes[8], bytes[9]]);
        if format_version != PROFILE_FORMAT_VERSION {
            return Err(ProfileDropReason::UnsupportedFormatVersion {
                found: format_version,
            });
        }
        let schema_version = u16::from_le_bytes([bytes[10], bytes[11]]);
        if schema_version != SCHEMA_VERSION {
            return Err(ProfileDropReason::UnsupportedSchemaVersion {
                found: schema_version,
            });
        }
        if bytes[12] != PROFILE_SCOPE_VECTOR_LOCAL {
            return Err(ProfileDropReason::UnsupportedScope { found: bytes[12] });
        }
        if bytes[13..16] != [0; 3] {
            return Err(ProfileDropReason::NonzeroReservedHeader);
        }

        let session = read_u64(bytes, 16);
        if session != process_profile_session() {
            return Err(ProfileDropReason::DifferentProcessSession);
        }
        let vector_id = read_u64(bytes, 24);
        if vector_id != self.profile_vector_id() {
            return Err(ProfileDropReason::DifferentFeedbackVector);
        }
        if read_u64(bytes, 32) != self.layout.stable_hash() {
            return Err(ProfileDropReason::LayoutMismatch);
        }

        let site_count = read_u32(bytes, 40);
        let expected_sites =
            u32::try_from(self.layout.len()).expect("feedback profile has more than u32 sites");
        if site_count != expected_sites {
            return Err(ProfileDropReason::SiteCountMismatch {
                found: site_count,
                expected: expected_sites,
            });
        }
        let slot_count = read_u32(bytes, 44);
        let expected_slots = u32::try_from(self.layout.slot_len())
            .expect("feedback profile has more than u32 slots");
        if slot_count != expected_slots {
            return Err(ProfileDropReason::SlotCountMismatch {
                found: slot_count,
                expected: expected_slots,
            });
        }
        let payload_len = (slot_count as usize)
            .checked_mul(8)
            .expect("feedback profile payload length overflow");
        let expected_len = PROFILE_HEADER_LEN
            .checked_add(payload_len)
            .expect("feedback profile length overflow");
        if bytes.len() != expected_len {
            return Err(ProfileDropReason::InvalidLength {
                expected: expected_len,
                actual: bytes.len(),
            });
        }

        let (payload_words, remainder) = bytes[PROFILE_HEADER_LEN..].as_chunks::<8>();
        debug_assert!(remainder.is_empty());
        let incoming = payload_words
            .iter()
            .enumerate()
            .map(|(slot, bytes)| {
                let word = ObservationWord(u64::from_le_bytes(*bytes));
                if word.is_valid() {
                    Ok(word)
                } else {
                    Err(ProfileDropReason::InvalidObservationWord { slot: slot as u32 })
                }
            })
            .collect::<Result<Vec<_>, _>>()?;

        for (cell, incoming) in self.words().iter().zip(incoming) {
            let current = ObservationWord(cell.get());
            cell.set(merge_observation_words(current, incoming).0);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn read(
        &self,
        site: SiteId,
        kind: ObservationKind,
        role: ObservationRole,
    ) -> ObservationWord {
        let descriptors = self.layout.slots(site).unwrap();
        let offset = descriptors
            .iter()
            .position(|slot| slot.kind == kind && slot.role == role)
            .unwrap();
        let first = self.layout.site(site).unwrap().first_slot as usize;
        ObservationWord(self.words()[first + offset].get())
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn read_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn merge_observation_words(current: ObservationWord, incoming: ObservationWord) -> ObservationWord {
    debug_assert!(current.is_valid());
    debug_assert!(incoming.is_valid());
    if current == incoming || incoming == ObservationWord::UNINITIALIZED {
        return current;
    }
    if current == ObservationWord::UNINITIALIZED {
        return incoming;
    }
    let current_state = current.decoded_state().unwrap();
    let incoming_state = incoming.decoded_state().unwrap();
    if current_state == ObservationState::Generic || incoming_state == ObservationState::Generic {
        ObservationWord::new(ObservationState::Generic, 0, 0)
    } else {
        ObservationWord::new(ObservationState::Polymorphic, 0, 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RECEIVER: SlotDescriptor = SlotDescriptor {
        kind: ObservationKind::ReceiverLayout,
        role: ObservationRole::Receiver,
    };
    const HOLDER: SlotDescriptor = SlotDescriptor {
        kind: ObservationKind::HolderLayout,
        role: ObservationRole::Holder,
    };

    fn property_vector() -> FeedbackVector {
        let mut builder = LayoutBuilder::default();
        builder.add_site(4, OperationKind::NamedLoad, &[RECEIVER, HOLDER]);
        FeedbackVector::new(
            builder.finish(),
            vec![RuntimeBinding::Unbound].into_boxed_slice(),
        )
    }

    #[test]
    fn layout_assigns_dense_stable_site_and_slot_ids() {
        let mut builder = LayoutBuilder::default();
        let first = builder.add_site(4, OperationKind::NamedLoad, &[RECEIVER, HOLDER]);
        let second = builder.add_site(
            9,
            OperationKind::Call,
            &[SlotDescriptor {
                kind: ObservationKind::CallTarget,
                role: ObservationRole::Target,
            }],
        );
        let layout = builder.finish();

        assert_eq!(layout.version(), 1);
        assert_eq!((first.index(), second.index()), (0, 1));
        assert_eq!(layout.site(first).unwrap().bytecode_pc, 4);
        assert_eq!(layout.slots(first).unwrap(), &[RECEIVER, HOLDER]);
        assert_eq!(layout.site(second).unwrap().first_slot, 2);
        assert_eq!(layout.site_ids().collect::<Vec<_>>(), vec![first, second]);
    }

    #[test]
    fn observation_words_are_lazy_and_layout_sized() {
        let vector = property_vector();
        let layout_bytes = vector.retained_bytes();

        vector.write(
            SiteId(0),
            ObservationKind::ReceiverLayout,
            ObservationRole::Receiver,
            ObservationWord::new(ObservationState::Monomorphic, 7, 0),
        );
        assert_eq!(vector.words().len(), 2);
        assert_eq!(
            vector
                .read(
                    SiteId(0),
                    ObservationKind::ReceiverLayout,
                    ObservationRole::Receiver,
                )
                .payload(),
            7
        );
        assert_eq!(
            vector.retained_bytes(),
            layout_bytes + 2 * std::mem::size_of::<Cell<u64>>()
        );
    }

    #[test]
    fn schema_numeric_encodings_are_frozen() {
        assert_eq!(OperationKind::NamedLoad as u8, 1);
        assert_eq!(OperationKind::Allocation as u8, 10);
        assert_eq!(ObservationKind::ValueClass as u8, 1);
        assert_eq!(ObservationKind::Allocation as u8, 7);
        assert_eq!(ObservationRole::Operand0 as u8, 1);
        assert_eq!(ObservationRole::Outcome as u8, 8);
        assert_eq!(ObservationState::Uninitialized as u8, 0);
        assert_eq!(ObservationState::Generic as u8, 4);
        assert_eq!(std::mem::size_of::<SlotDescriptor>(), 2);
        assert_eq!(std::mem::size_of::<SiteDescriptor>(), 12);
    }

    #[test]
    fn profile_envelope_encoding_is_frozen() {
        let vector = property_vector();
        let profile = vector.serialize_profile();

        assert_eq!(&profile[0..8], b"LUMENFB\0");
        assert_eq!(u16::from_le_bytes([profile[8], profile[9]]), 1);
        assert_eq!(u16::from_le_bytes([profile[10], profile[11]]), 1);
        assert_eq!(profile[12], PROFILE_SCOPE_VECTOR_LOCAL);
        assert_eq!(&profile[13..16], &[0; 3]);
        assert_ne!(read_u64(&profile, 16), 0);
        assert_ne!(read_u64(&profile, 24), 0);
        assert_eq!(read_u64(&profile, 32), vector.layout.stable_hash());
        assert_eq!(vector.layout.stable_hash(), 0x2528_6bec_cecd_210a);
        assert_eq!(read_u32(&profile, 40), 1);
        assert_eq!(read_u32(&profile, 44), 2);
        assert_eq!(profile.len(), PROFILE_HEADER_LEN + 16);
    }

    #[test]
    fn profile_round_trip_merges_without_narrowing() {
        let vector = property_vector();
        vector.write(
            SiteId(0),
            ObservationKind::ReceiverLayout,
            ObservationRole::Receiver,
            ObservationWord::new(ObservationState::Monomorphic, 7, 0),
        );
        let profile = vector.serialize_profile();

        vector.write(
            SiteId(0),
            ObservationKind::ReceiverLayout,
            ObservationRole::Receiver,
            ObservationWord::UNINITIALIZED,
        );
        vector.merge_serialized_profile(&profile).unwrap();
        assert_eq!(
            vector
                .read(
                    SiteId(0),
                    ObservationKind::ReceiverLayout,
                    ObservationRole::Receiver,
                )
                .payload(),
            7
        );

        vector.write(
            SiteId(0),
            ObservationKind::ReceiverLayout,
            ObservationRole::Receiver,
            ObservationWord::new(ObservationState::Monomorphic, 8, 0),
        );
        vector.merge_serialized_profile(&profile).unwrap();
        assert_eq!(
            vector
                .read(
                    SiteId(0),
                    ObservationKind::ReceiverLayout,
                    ObservationRole::Receiver,
                )
                .state(),
            ObservationState::Polymorphic
        );

        vector.write(
            SiteId(0),
            ObservationKind::ReceiverLayout,
            ObservationRole::Receiver,
            ObservationWord::new(ObservationState::Generic, 0, 0),
        );
        vector.merge_serialized_profile(&profile).unwrap();
        assert_eq!(
            vector
                .read(
                    SiteId(0),
                    ObservationKind::ReceiverLayout,
                    ObservationRole::Receiver,
                )
                .state(),
            ObservationState::Generic
        );
    }

    #[test]
    fn profile_envelope_drops_every_incompatible_identity() {
        let vector = property_vector();
        let profile = vector.serialize_profile();

        let mut changed = profile.clone();
        changed[0] ^= 1;
        assert_eq!(
            vector.merge_serialized_profile(&changed),
            Err(ProfileDropReason::InvalidMagic)
        );

        let mut changed = profile.clone();
        changed[8..10].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            vector.merge_serialized_profile(&changed),
            Err(ProfileDropReason::UnsupportedFormatVersion { found: 2 })
        );

        let mut changed = profile.clone();
        changed[10..12].copy_from_slice(&2_u16.to_le_bytes());
        assert_eq!(
            vector.merge_serialized_profile(&changed),
            Err(ProfileDropReason::UnsupportedSchemaVersion { found: 2 })
        );

        let mut changed = profile.clone();
        changed[12] = 2;
        assert_eq!(
            vector.merge_serialized_profile(&changed),
            Err(ProfileDropReason::UnsupportedScope { found: 2 })
        );

        let mut changed = profile.clone();
        changed[13] = 1;
        assert_eq!(
            vector.merge_serialized_profile(&changed),
            Err(ProfileDropReason::NonzeroReservedHeader)
        );

        let mut changed = profile.clone();
        changed[16..24].copy_from_slice(&read_u64(&profile, 16).wrapping_add(1).to_le_bytes());
        assert_eq!(
            vector.merge_serialized_profile(&changed),
            Err(ProfileDropReason::DifferentProcessSession)
        );

        assert_eq!(
            property_vector().merge_serialized_profile(&profile),
            Err(ProfileDropReason::DifferentFeedbackVector)
        );

        let mut changed = profile.clone();
        changed[32] ^= 1;
        assert_eq!(
            vector.merge_serialized_profile(&changed),
            Err(ProfileDropReason::LayoutMismatch)
        );

        let mut changed = profile.clone();
        changed[40..44].copy_from_slice(&2_u32.to_le_bytes());
        assert_eq!(
            vector.merge_serialized_profile(&changed),
            Err(ProfileDropReason::SiteCountMismatch {
                found: 2,
                expected: 1,
            })
        );

        let mut changed = profile;
        changed[44..48].copy_from_slice(&3_u32.to_le_bytes());
        assert_eq!(
            vector.merge_serialized_profile(&changed),
            Err(ProfileDropReason::SlotCountMismatch {
                found: 3,
                expected: 2,
            })
        );
    }

    #[test]
    fn profile_envelope_rejects_malformed_payload_before_mutating() {
        let vector = property_vector();
        let profile = vector.serialize_profile();

        assert_eq!(
            vector.merge_serialized_profile(&profile[..PROFILE_HEADER_LEN - 1]),
            Err(ProfileDropReason::Truncated {
                minimum: PROFILE_HEADER_LEN,
                actual: PROFILE_HEADER_LEN - 1,
            })
        );

        let mut changed = profile.clone();
        changed.push(0);
        assert_eq!(
            vector.merge_serialized_profile(&changed),
            Err(ProfileDropReason::InvalidLength {
                expected: profile.len(),
                actual: profile.len() + 1,
            })
        );

        let mut changed = profile;
        changed[PROFILE_HEADER_LEN] = 0xff;
        assert_eq!(
            vector.merge_serialized_profile(&changed),
            Err(ProfileDropReason::InvalidObservationWord { slot: 0 })
        );
        assert_eq!(
            vector
                .read(
                    SiteId(0),
                    ObservationKind::ReceiverLayout,
                    ObservationRole::Receiver,
                )
                .state(),
            ObservationState::Uninitialized
        );
    }
}
