//! Checked central-heap nucleus for the tagged-value migration.
//!
//! This handle-table prototype is deliberately not connected to `Value` or the live collector.
//! It provides the ownership and relocation invariants that later object-family migrations must
//! satisfy without exposing Rust allocation addresses to generated code.

#![allow(dead_code)]

use crate::tagged::{HeapRef, TaggedValue};

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
    RequestTooLarge,
    ReferenceSpaceExhausted,
}

/// A checked Agent-local handle table. Index zero is permanently reserved so a zero tagged
/// payload can never accidentally resolve to an object. Slots are not reused in this nucleus;
/// generation/reuse cookies belong to the production table once its relocation verifier exists.
pub(crate) struct CentralHeap {
    objects: Vec<Option<HeapObject>>,
    requested_bytes: usize,
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
        }
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
        Ok(HeapRef::new(index).expect("central heap never publishes a zero handle"))
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

    pub(crate) fn requested_bytes(&self) -> usize {
        self.requested_bytes
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
        heap.tagged_fields_mut(source).unwrap()[0] = TaggedValue::heap(source);
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
}
