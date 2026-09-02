//! Checked central-heap nucleus for the tagged-value migration.
//!
//! This handle-table prototype is deliberately not connected to `Value` or the live collector.
//! It provides the ownership and relocation invariants that later object-family migrations must
//! satisfy without exposing Rust allocation addresses to generated code.

#![allow(dead_code)]

use crate::tagged::{HeapRef, NoGcError, NoGcScope, NoGcState, RootSet, TaggedValue};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapGeneration {
    Young,
    Old,
    Large,
    Pinned,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct LayoutId(u32);

impl LayoutId {
    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ObjectHeader {
    pub(crate) size_units: u32,
    pub(crate) layout: LayoutId,
    pub(crate) generation: HeapGeneration,
    pub(crate) marked: bool,
    pub(crate) forwarding: Option<HeapRef>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct HeapObject {
    header: ObjectHeader,
    storage: HeapStorage,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum HeapStorage {
    Bytes(Box<[u8]>),
    Tagged(Box<[TaggedValue]>),
}

impl HeapStorage {
    fn requested_bytes(&self) -> usize {
        match self {
            Self::Bytes(payload) => payload.len(),
            Self::Tagged(fields) => fields
                .len()
                .saturating_mul(std::mem::size_of::<TaggedValue>()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum HeapError {
    InvalidReference,
    ForwardedReference,
    AlreadyForwarded,
    NotForwarded,
    PayloadKindMismatch,
    InvalidTaggedField,
    FieldOutOfBounds,
    HeaderMismatch,
    RememberedSetMismatch,
    RequestTooLarge,
    ReferenceSpaceExhausted,
}

/// A checked Agent-local handle table. Index zero is permanently reserved so a zero tagged
/// payload can never accidentally resolve to an object. Slots are not reused in this nucleus;
/// generation/reuse cookies belong to the production table once its relocation verifier exists.
pub(crate) struct CentralHeap {
    objects: Vec<Option<HeapObject>>,
    requested_bytes: usize,
    no_gc: NoGcState,
    remembered: Vec<HeapRef>,
}

impl Default for CentralHeap {
    fn default() -> Self {
        Self::new()
    }
}

impl CentralHeap {
    pub(crate) fn new() -> Self {
        Self {
            objects: vec![None],
            requested_bytes: 0,
            no_gc: NoGcState::default(),
            remembered: Vec::new(),
        }
    }

    /// Enter a short raw-pointer region. The returned guard borrows this heap, so mutable
    /// allocation, relocation, and reclamation cannot be called while the region is live.
    pub(crate) fn enter_no_gc(&self) -> Result<NoGcScope<'_>, NoGcError> {
        self.no_gc.enter()
    }

    pub(crate) fn require_safepoint(&self) -> Result<(), NoGcError> {
        self.no_gc.require_safepoint()
    }

    pub(crate) fn allocate(
        &mut self,
        layout: LayoutId,
        size_units: usize,
        generation: HeapGeneration,
    ) -> Result<HeapRef, HeapError> {
        let size_units = u32::try_from(size_units).map_err(|_| HeapError::RequestTooLarge)?;
        let index =
            u32::try_from(self.objects.len()).map_err(|_| HeapError::ReferenceSpaceExhausted)?;
        let payload = vec![0; usize::try_from(size_units).unwrap()].into_boxed_slice();
        self.requested_bytes = self.requested_bytes.saturating_add(payload.len());
        self.objects.push(Some(HeapObject {
            header: ObjectHeader {
                size_units,
                layout,
                generation,
                marked: false,
                forwarding: None,
            },
            storage: HeapStorage::Bytes(payload),
        }));
        // `index` is at least one because slot zero is reserved by `new`.
        Ok(HeapRef::new(index).expect("central heap never publishes a zero handle"))
    }

    pub(crate) fn allocate_tagged_fields(
        &mut self,
        layout: LayoutId,
        fields: Vec<TaggedValue>,
        generation: HeapGeneration,
    ) -> Result<HeapRef, HeapError> {
        let size_units = u32::try_from(fields.len()).map_err(|_| HeapError::RequestTooLarge)?;
        if fields.iter().any(|value| value.validate().is_err()) {
            return Err(HeapError::InvalidTaggedField);
        }
        let storage = HeapStorage::Tagged(fields.into_boxed_slice());
        self.requested_bytes = self
            .requested_bytes
            .saturating_add(storage.requested_bytes());
        let index =
            u32::try_from(self.objects.len()).map_err(|_| HeapError::ReferenceSpaceExhausted)?;
        self.objects.push(Some(HeapObject {
            header: ObjectHeader {
                size_units,
                layout,
                generation,
                marked: false,
                forwarding: None,
            },
            storage,
        }));
        let reference = HeapRef::new(index).expect("central heap never publishes a zero handle");
        self.refresh_remembered_reference(reference)?;
        Ok(reference)
    }

    fn object(&self, reference: HeapRef) -> Result<&HeapObject, HeapError> {
        let object = self
            .objects
            .get(reference.get() as usize)
            .and_then(Option::as_ref)
            .ok_or(HeapError::InvalidReference)?;
        if object.header.forwarding.is_some() {
            return Err(HeapError::ForwardedReference);
        }
        Ok(object)
    }

    fn object_mut(&mut self, reference: HeapRef) -> Result<&mut HeapObject, HeapError> {
        let object = self
            .objects
            .get_mut(reference.get() as usize)
            .and_then(Option::as_mut)
            .ok_or(HeapError::InvalidReference)?;
        if object.header.forwarding.is_some() {
            return Err(HeapError::ForwardedReference);
        }
        Ok(object)
    }

    pub(crate) fn header(&self, reference: HeapRef) -> Result<ObjectHeader, HeapError> {
        Ok(self.object(reference)?.header)
    }

    pub(crate) fn payload(&self, reference: HeapRef) -> Result<&[u8], HeapError> {
        match &self.object(reference)?.storage {
            HeapStorage::Bytes(payload) => Ok(payload),
            HeapStorage::Tagged(_) => Err(HeapError::PayloadKindMismatch),
        }
    }

    pub(crate) fn payload_mut(&mut self, reference: HeapRef) -> Result<&mut [u8], HeapError> {
        match &mut self.object_mut(reference)?.storage {
            HeapStorage::Bytes(payload) => Ok(payload),
            HeapStorage::Tagged(_) => Err(HeapError::PayloadKindMismatch),
        }
    }

    pub(crate) fn tagged_fields(&self, reference: HeapRef) -> Result<&[TaggedValue], HeapError> {
        match &self.object(reference)?.storage {
            HeapStorage::Tagged(fields) => Ok(fields),
            HeapStorage::Bytes(_) => Err(HeapError::PayloadKindMismatch),
        }
    }

    pub(crate) fn tagged_fields_mut(
        &mut self,
        reference: HeapRef,
    ) -> Result<&mut [TaggedValue], HeapError> {
        match &mut self.object_mut(reference)?.storage {
            HeapStorage::Tagged(fields) => Ok(fields),
            HeapStorage::Bytes(_) => Err(HeapError::PayloadKindMismatch),
        }
    }

    fn has_young_edge(&self, reference: HeapRef) -> Result<bool, HeapError> {
        let object = self.object(reference)?;
        if object.header.generation == HeapGeneration::Young {
            return Ok(false);
        }
        let HeapStorage::Tagged(fields) = &object.storage else {
            return Ok(false);
        };
        for field in fields {
            let Some(child) = field.as_heap() else {
                continue;
            };
            if self.object(child)?.header.generation == HeapGeneration::Young {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn refresh_remembered_reference(&mut self, reference: HeapRef) -> Result<(), HeapError> {
        self.remembered.retain(|entry| *entry != reference);
        if self.has_young_edge(reference)? {
            self.remembered.push(reference);
        }
        Ok(())
    }

    /// Store through the barriered tagged-field primitive. Direct mutable slices are retained for
    /// verifier fixtures, but migrated object families must use this method for every write.
    pub(crate) fn store_tagged_field(
        &mut self,
        reference: HeapRef,
        index: usize,
        value: TaggedValue,
    ) -> Result<(), HeapError> {
        value
            .validate()
            .map_err(|_| HeapError::InvalidTaggedField)?;
        if let Some(child) = value.as_heap() {
            self.object(child)?;
        }
        let fields = self.tagged_fields_mut(reference)?;
        let field = fields.get_mut(index).ok_or(HeapError::FieldOutOfBounds)?;
        *field = value;
        self.refresh_remembered_reference(reference)
    }

    pub(crate) fn remembered_handles(&self) -> Vec<HeapRef> {
        self.remembered.clone()
    }

    /// Recompute old-to-young edges independently of the incremental barrier state.
    pub(crate) fn recompute_remembered_set(&self) -> Result<Vec<HeapRef>, HeapError> {
        self.validate_all()?;
        let mut recomputed = Vec::new();
        for (index, slot) in self.objects.iter().enumerate().skip(1) {
            let Some(object) = slot else { continue };
            if object.header.forwarding.is_some()
                || object.header.generation == HeapGeneration::Young
            {
                continue;
            }
            let HeapStorage::Tagged(fields) = &object.storage else {
                continue;
            };
            let mut has_young_edge = false;
            for field in fields {
                let Some(child) = field.as_heap() else {
                    continue;
                };
                if self.object(child)?.header.generation == HeapGeneration::Young {
                    has_young_edge = true;
                    break;
                }
            }
            if has_young_edge {
                let index = u32::try_from(index).expect("heap index is bounded by the table");
                recomputed.push(HeapRef::new(index).expect("slot zero is reserved"));
            }
        }
        Ok(recomputed)
    }

    pub(crate) fn verify_remembered_set(&self) -> Result<(), HeapError> {
        let mut recorded = self.remembered.clone();
        let mut recomputed = self.recompute_remembered_set()?;
        recorded.sort_by_key(|reference| reference.get());
        recomputed.sort_by_key(|reference| reference.get());
        if recorded == recomputed {
            Ok(())
        } else {
            Err(HeapError::RememberedSetMismatch)
        }
    }

    /// Rewrite every tagged field in the table after a relocation. The caller must rewrite its
    /// external roots through the corresponding `RootSet` operation before reclaiming from-space.
    pub(crate) fn rewrite_tagged_references(&mut self, from: HeapRef, to: HeapRef) -> usize {
        let replacement = TaggedValue::heap(to);
        let mut rewritten = 0;
        for object in self.objects.iter_mut().flatten() {
            let HeapStorage::Tagged(fields) = &mut object.storage else {
                continue;
            };
            for field in fields.iter_mut() {
                if field.as_heap() == Some(from) {
                    *field = replacement;
                    rewritten += 1;
                }
            }
        }
        rewritten
    }

    pub(crate) fn mark(&mut self, reference: HeapRef) -> Result<(), HeapError> {
        self.object_mut(reference)?.header.marked = true;
        Ok(())
    }

    /// Mark the transitive strong closure of tagged roots. The fixture heap has no weak or
    /// external families yet, so every tagged child is a strong edge; those families must add
    /// their own tracing policy before they are admitted to this walk.
    pub(crate) fn mark_roots(&mut self, roots: &RootSet) -> Result<usize, HeapError> {
        self.validate_all()?;
        let mut pending = roots
            .snapshot()
            .map_err(|_| HeapError::InvalidTaggedField)?
            .into_iter()
            .filter_map(TaggedValue::as_heap)
            .collect::<Vec<_>>();
        let mut marked = 0;
        while let Some(reference) = pending.pop() {
            let children = match &self.object(reference)?.storage {
                HeapStorage::Bytes(_) => Vec::new(),
                HeapStorage::Tagged(fields) => fields
                    .iter()
                    .filter_map(|field| field.as_heap())
                    .collect::<Vec<_>>(),
            };
            let object = self.object_mut(reference)?;
            if object.header.marked {
                continue;
            }
            object.header.marked = true;
            marked += 1;
            pending.extend(children);
        }
        Ok(marked)
    }

    pub(crate) fn promote(
        &mut self,
        reference: HeapRef,
        generation: HeapGeneration,
    ) -> Result<(), HeapError> {
        self.object_mut(reference)?.header.generation = generation;
        self.refresh_remembered_reference(reference)
    }

    pub(crate) fn requested_bytes(&self) -> usize {
        self.requested_bytes
    }

    /// Verify every live slot, header/storage size, forwarding edge, and tagged child reference.
    /// This is intentionally an independent walk rather than a fast-path assertion so stress
    /// collectors can run it before and after every relocation.
    pub(crate) fn validate_all(&self) -> Result<(), HeapError> {
        for (index, object) in self.objects.iter().enumerate().skip(1) {
            let Some(object) = object else { continue };
            let expected_units = match &object.storage {
                HeapStorage::Bytes(payload) => payload.len(),
                HeapStorage::Tagged(fields) => fields.len(),
            };
            if object.header.size_units as usize != expected_units {
                return Err(HeapError::HeaderMismatch);
            }
            if let Some(target) = object.header.forwarding {
                if target.get() as usize == index {
                    return Err(HeapError::HeaderMismatch);
                }
                let target_object = self
                    .objects
                    .get(target.get() as usize)
                    .and_then(Option::as_ref)
                    .ok_or(HeapError::InvalidReference)?;
                if target_object.header.forwarding.is_some() {
                    return Err(HeapError::HeaderMismatch);
                }
            }
            let HeapStorage::Tagged(fields) = &object.storage else {
                continue;
            };
            for field in fields {
                field
                    .validate()
                    .map_err(|_| HeapError::InvalidTaggedField)?;
                let Some(child) = field.as_heap() else {
                    continue;
                };
                let child_object = self
                    .objects
                    .get(child.get() as usize)
                    .and_then(Option::as_ref)
                    .ok_or(HeapError::InvalidReference)?;
                if child_object.header.forwarding.is_some() {
                    return Err(HeapError::ForwardedReference);
                }
            }
        }
        Ok(())
    }

    /// Deterministically reclaim unmarked, non-forwarded slots. Mark bits are cleared for the
    /// survivors so the next collection starts from a clean white set. Forwarded sources remain
    /// until their explicit relocation handshake calls `reclaim_forwarded`.
    pub(crate) fn sweep_unmarked(&mut self) -> usize {
        let mut reclaimed = 0;
        let mut released_bytes: usize = 0;
        for slot in self.objects.iter_mut().skip(1) {
            let remove = slot
                .as_ref()
                .is_some_and(|object| !object.header.marked && object.header.forwarding.is_none());
            if remove {
                if let Some(object) = slot.take() {
                    released_bytes =
                        released_bytes.saturating_add(object.storage.requested_bytes());
                    reclaimed += 1;
                }
            } else if let Some(object) = slot {
                object.header.marked = false;
            }
        }
        self.requested_bytes = self.requested_bytes.saturating_sub(released_bytes);
        let remembered = std::mem::take(&mut self.remembered);
        self.remembered = remembered
            .into_iter()
            .filter(|reference| {
                self.objects
                    .get(reference.get() as usize)
                    .and_then(Option::as_ref)
                    .is_some_and(|object| object.header.forwarding.is_none())
            })
            .collect();
        reclaimed
    }

    pub(crate) fn live_handles(&self) -> Vec<HeapRef> {
        self.objects
            .iter()
            .enumerate()
            .skip(1)
            .filter_map(|(index, object)| {
                object
                    .as_ref()
                    .and_then(|_| u32::try_from(index).ok().and_then(HeapRef::new))
            })
            .collect()
    }

    /// Copy one object to a new slot and publish a forwarding edge on the source. The source is
    /// inaccessible through ordinary accessors until `reclaim_forwarded` runs, forcing callers to
    /// update every root and traced field before releasing from-space.
    pub(crate) fn relocate(&mut self, source: HeapRef) -> Result<HeapRef, HeapError> {
        let source_object = self
            .objects
            .get(source.get() as usize)
            .and_then(Option::as_ref)
            .ok_or(HeapError::InvalidReference)?;
        if source_object.header.forwarding.is_some() {
            return Err(HeapError::AlreadyForwarded);
        }
        let mut moved = source_object.clone();
        moved.header.forwarding = None;
        let target_index =
            u32::try_from(self.objects.len()).map_err(|_| HeapError::ReferenceSpaceExhausted)?;
        self.requested_bytes = self
            .requested_bytes
            .saturating_add(moved.storage.requested_bytes());
        self.objects.push(Some(moved));
        let target = HeapRef::new(target_index).expect("central heap never publishes zero handle");
        self.refresh_remembered_reference(target)?;
        let source_object = self
            .objects
            .get_mut(source.get() as usize)
            .and_then(Option::as_mut)
            .ok_or(HeapError::InvalidReference)?;
        source_object.header.forwarding = Some(target);
        Ok(target)
    }

    /// Reclaim a source only after its forwarding target has been published and all external
    /// references have been rewritten. The old handle then fails closed as invalid.
    pub(crate) fn reclaim_forwarded(&mut self, source: HeapRef) -> Result<HeapRef, HeapError> {
        let object = self
            .objects
            .get_mut(source.get() as usize)
            .and_then(Option::as_mut)
            .ok_or(HeapError::InvalidReference)?;
        let target = object.header.forwarding.ok_or(HeapError::NotForwarded)?;
        let released = self
            .objects
            .get_mut(source.get() as usize)
            .and_then(Option::take)
            .ok_or(HeapError::InvalidReference)?;
        self.requested_bytes = self
            .requested_bytes
            .saturating_sub(released.storage.requested_bytes());
        self.remembered.retain(|entry| *entry != source);
        Ok(target)
    }

    /// Remove one unreachable object. This path is intentionally separate from relocation so a
    /// collector cannot accidentally reclaim an object while a forwarding edge is still active.
    pub(crate) fn free(&mut self, reference: HeapRef) -> Result<(), HeapError> {
        let object = self
            .objects
            .get_mut(reference.get() as usize)
            .and_then(Option::as_mut)
            .ok_or(HeapError::InvalidReference)?;
        if object.header.forwarding.is_some() {
            return Err(HeapError::ForwardedReference);
        }
        let released = self
            .objects
            .get_mut(reference.get() as usize)
            .and_then(Option::take)
            .ok_or(HeapError::InvalidReference)?;
        self.requested_bytes = self
            .requested_bytes
            .saturating_sub(released.storage.requested_bytes());
        self.remembered.retain(|entry| *entry != reference);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tagged::RootSet;

    #[test]
    fn allocation_uses_checked_handles_and_separate_payload_accounting() {
        let mut heap = CentralHeap::new();
        let reference = heap
            .allocate(LayoutId::new(3), 12, HeapGeneration::Young)
            .unwrap();
        assert_eq!(reference.get(), 1);
        assert_eq!(heap.requested_bytes(), 12);
        assert_eq!(heap.header(reference).unwrap().size_units, 12);
        assert_eq!(
            heap.header(reference).unwrap().generation,
            HeapGeneration::Young
        );
        assert_eq!(heap.payload(reference).unwrap().len(), 12);
        heap.payload_mut(reference).unwrap()[4] = 0xa5;
        assert_eq!(heap.payload(reference).unwrap()[4], 0xa5);
        heap.mark(reference).unwrap();
        assert!(heap.header(reference).unwrap().marked);
    }

    #[test]
    fn no_gc_guard_is_owned_by_the_heap_and_blocks_nested_safepoints() {
        let heap = CentralHeap::new();
        let guard = heap.enter_no_gc().unwrap();
        guard.assert_active();
        assert_eq!(heap.require_safepoint(), Err(NoGcError::SafepointForbidden));
        assert!(matches!(heap.enter_no_gc(), Err(NoGcError::AlreadyActive)));
        drop(guard);
        assert_eq!(heap.require_safepoint(), Ok(()));
    }

    #[test]
    fn invalid_and_freed_handles_fail_closed() {
        let mut heap = CentralHeap::new();
        assert_eq!(HeapRef::new(0), None);
        let reference = heap
            .allocate(LayoutId::new(1), 1, HeapGeneration::Old)
            .unwrap();
        heap.free(reference).unwrap();
        assert_eq!(heap.payload(reference), Err(HeapError::InvalidReference));
        assert_eq!(heap.requested_bytes(), 0);
    }

    #[test]
    fn relocation_requires_rewriting_before_reclaiming_from_space() {
        let mut heap = CentralHeap::new();
        let source = heap
            .allocate(LayoutId::new(9), 4, HeapGeneration::Young)
            .unwrap();
        heap.payload_mut(source)
            .unwrap()
            .copy_from_slice(&[1, 2, 3, 4]);
        let target = heap.relocate(source).unwrap();
        assert_eq!(heap.payload(source), Err(HeapError::ForwardedReference));
        assert_eq!(heap.payload(target).unwrap(), &[1, 2, 3, 4]);
        assert_eq!(heap.relocate(source), Err(HeapError::AlreadyForwarded));
        assert_eq!(heap.reclaim_forwarded(source), Ok(target));
        assert_eq!(heap.payload(source), Err(HeapError::InvalidReference));
        assert_eq!(heap.requested_bytes(), 4);
    }

    #[test]
    fn ordinary_free_does_not_accept_a_forwarded_source() {
        let mut heap = CentralHeap::new();
        let source = heap
            .allocate(LayoutId::new(2), 2, HeapGeneration::Young)
            .unwrap();
        let _target = heap.relocate(source).unwrap();
        assert_eq!(heap.free(source), Err(HeapError::ForwardedReference));
    }

    #[test]
    fn tagged_leaf_rewrites_roots_and_self_references_before_reclaim() {
        let mut heap = CentralHeap::new();
        let source = heap
            .allocate_tagged_fields(
                LayoutId::new(11),
                vec![TaggedValue::undefined(), TaggedValue::null()],
                HeapGeneration::Young,
            )
            .unwrap();
        heap.store_tagged_field(source, 0, TaggedValue::heap(source))
            .unwrap();
        let roots = RootSet::new();
        let root = roots.root(TaggedValue::heap(source)).unwrap();

        let target = heap.relocate(source).unwrap();
        assert_eq!(
            heap.rewrite_tagged_references(source, target),
            2,
            "one self-edge in from-space and one copied self-edge must be rewritten"
        );
        assert_eq!(roots.rewrite_heap_reference(source, target), 1);
        assert_eq!(heap.reclaim_forwarded(source), Ok(target));
        assert_eq!(
            heap.tagged_fields(target).unwrap()[0].as_heap(),
            Some(target)
        );
        assert_eq!(root.value().and_then(TaggedValue::as_heap), Some(target));
    }

    #[test]
    fn tagged_leaf_rejects_invalid_fields_and_keeps_byte_payloads_distinct() {
        let mut heap = CentralHeap::new();
        assert_eq!(
            heap.allocate_tagged_fields(
                LayoutId::new(1),
                vec![TaggedValue::from_raw(0x7ffc_0000_0000_0002)],
                HeapGeneration::Young,
            ),
            Err(HeapError::InvalidTaggedField)
        );
        let bytes = heap
            .allocate(LayoutId::new(2), 1, HeapGeneration::Old)
            .unwrap();
        assert_eq!(
            heap.tagged_fields(bytes),
            Err(HeapError::PayloadKindMismatch)
        );
    }

    #[test]
    fn validation_and_sweep_preserve_marked_generation_and_accounting() {
        let mut heap = CentralHeap::new();
        let keep = heap
            .allocate(LayoutId::new(4), 3, HeapGeneration::Young)
            .unwrap();
        let discard = heap
            .allocate(LayoutId::new(5), 7, HeapGeneration::Old)
            .unwrap();
        assert_eq!(heap.validate_all(), Ok(()));
        heap.promote(keep, HeapGeneration::Old).unwrap();
        heap.mark(keep).unwrap();
        assert_eq!(heap.header(keep).unwrap().generation, HeapGeneration::Old);
        assert_eq!(heap.sweep_unmarked(), 1);
        assert_eq!(heap.requested_bytes(), 3);
        assert_eq!(heap.live_handles(), vec![keep]);
        assert_eq!(heap.payload(discard), Err(HeapError::InvalidReference));
        assert!(!heap.header(keep).unwrap().marked);
        assert_eq!(heap.validate_all(), Ok(()));
    }

    #[test]
    fn root_mark_walk_reaches_tagged_children_before_sweeping() {
        let mut heap = CentralHeap::new();
        let child = heap
            .allocate_tagged_fields(
                LayoutId::new(20),
                vec![TaggedValue::null()],
                HeapGeneration::Young,
            )
            .unwrap();
        let parent = heap
            .allocate_tagged_fields(
                LayoutId::new(21),
                vec![TaggedValue::heap(child)],
                HeapGeneration::Old,
            )
            .unwrap();
        let unreachable = heap
            .allocate(LayoutId::new(22), 5, HeapGeneration::Young)
            .unwrap();
        let roots = RootSet::new();
        let _root = roots.root(TaggedValue::heap(parent)).unwrap();

        assert_eq!(heap.remembered_handles(), vec![parent]);
        assert_eq!(heap.verify_remembered_set(), Ok(()));
        assert_eq!(heap.mark_roots(&roots), Ok(2));
        assert!(heap.header(parent).unwrap().marked);
        assert!(heap.header(child).unwrap().marked);
        assert_eq!(heap.sweep_unmarked(), 1);
        assert_eq!(heap.payload(unreachable), Err(HeapError::InvalidReference));
        assert_eq!(heap.verify_remembered_set(), Ok(()));
        assert_eq!(heap.validate_all(), Ok(()));
    }

    #[test]
    fn old_to_young_barrier_tracks_and_recomputes_edges() {
        let mut heap = CentralHeap::new();
        let old = heap
            .allocate_tagged_fields(
                LayoutId::new(30),
                vec![TaggedValue::null()],
                HeapGeneration::Old,
            )
            .unwrap();
        let young = heap
            .allocate_tagged_fields(
                LayoutId::new(31),
                vec![TaggedValue::undefined()],
                HeapGeneration::Young,
            )
            .unwrap();
        assert!(heap.remembered_handles().is_empty());
        heap.store_tagged_field(old, 0, TaggedValue::heap(young))
            .unwrap();
        assert_eq!(heap.remembered_handles(), vec![old]);
        assert_eq!(heap.verify_remembered_set(), Ok(()));

        heap.store_tagged_field(old, 0, TaggedValue::null())
            .unwrap();
        assert!(heap.remembered_handles().is_empty());
        assert_eq!(heap.verify_remembered_set(), Ok(()));

        heap.store_tagged_field(old, 0, TaggedValue::heap(young))
            .unwrap();
        heap.promote(old, HeapGeneration::Young).unwrap();
        assert!(heap.remembered_handles().is_empty());
        assert_eq!(heap.verify_remembered_set(), Ok(()));
    }

    #[test]
    fn validation_requires_relocation_edges_to_be_rewritten() {
        let mut heap = CentralHeap::new();
        let source = heap
            .allocate_tagged_fields(
                LayoutId::new(12),
                vec![TaggedValue::undefined()],
                HeapGeneration::Young,
            )
            .unwrap();
        heap.store_tagged_field(source, 0, TaggedValue::heap(source))
            .unwrap();
        let target = heap.relocate(source).unwrap();
        assert_eq!(heap.validate_all(), Err(HeapError::ForwardedReference));
        assert_eq!(heap.rewrite_tagged_references(source, target), 2);
        assert_eq!(heap.validate_all(), Ok(()));
        assert_eq!(heap.reclaim_forwarded(source), Ok(target));
        assert_eq!(heap.validate_all(), Ok(()));
    }
}
