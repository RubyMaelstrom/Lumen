//! Versioned, representation-independent per-function feedback layout.
//!
//! See `FEEDBACK_SCHEMA.md`. This module deliberately contains no `Value` discriminants, shape
//! numbers, object addresses, or IC structs. Those belong to versioned runtime adapters.

use std::cell::{Cell, OnceCell};

/// Numeric encodings below are part of the profile schema and may not be reordered in place.
pub(crate) const SCHEMA_VERSION: u16 = 1;

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

    #[cfg(test)]
    pub(crate) fn state(self) -> ObservationState {
        match self.0 as u8 {
            0 => ObservationState::Uninitialized,
            1 => ObservationState::Monomorphic,
            2 => ObservationState::Polymorphic,
            3 => ObservationState::Absent,
            4 => ObservationState::Generic,
            _ => unreachable!("invalid in-process observation state"),
        }
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
}

impl FeedbackVector {
    pub(crate) fn new(layout: FeedbackLayout, bindings: Box<[RuntimeBinding]>) -> Self {
        assert_eq!(layout.len(), bindings.len());
        Self {
            layout,
            bindings,
            words: OnceCell::new(),
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
        let mut builder = LayoutBuilder::default();
        builder.add_site(0, OperationKind::NamedLoad, &[RECEIVER, HOLDER]);
        let vector = FeedbackVector::new(
            builder.finish(),
            vec![RuntimeBinding::Unbound].into_boxed_slice(),
        );
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
}
