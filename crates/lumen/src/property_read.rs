//! Side-effect-free OrdinaryGet resolution, followed by the actual Call boundary.
//!
//! ECMA-262 e28783d5, https://tc39.es/ecma262/#sec-ordinaryget: a cached layout
//! never caches a getter's identity or result. Read its live descriptor, root the
//! selected getter, release every object/cache borrow, then Call with the original
//! receiver. Reentrant mutation or collection cannot invalidate that owned snapshot.

use super::{Abrupt, Interp};
use crate::feedback::{CurrentPropertyTrace, PropertyOutcome};
use crate::value::{PackedValue, Property, Value};

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ReadKind {
    Data,
    Accessor,
    MissingGetter,
}

/// One owned value plus lookup metadata fits in the same two words as `Value`.
/// The kind's unused discriminants also keep `Option<CachedGet>` at two words.
/// In particular, a successful data read must not acquire an out-of-line getter
/// dispatch call or a larger indirect-return buffer merely because getters cache.
pub(super) struct CachedGet {
    value: PackedValue,
    holder_shape: u32,
    depth: u8,
    array_key_check: bool,
    kind: ReadKind,
}

impl CachedGet {
    #[inline(always)]
    pub(super) fn undefined() -> Self {
        Self::from_data(PackedValue::pack(Value::Undefined))
    }

    #[inline(always)]
    fn from_data(value: PackedValue) -> Self {
        Self {
            value,
            holder_shape: 0,
            depth: 0,
            array_key_check: false,
            kind: ReadKind::Data,
        }
    }

    /// Speculative constructor forwarding must not execute author code before
    /// committing its argument ownership. A resolved getter is still a miss there.
    #[inline(always)]
    pub(super) fn data(self) -> Option<Value> {
        if self.kind == ReadKind::Data {
            Some(self.value.into_value())
        } else {
            None
        }
    }
}

impl Interp {
    #[inline(always)]
    pub(super) fn snapshot_cached_get(
        &self,
        property: &Property,
        name: &str,
        holder_shape: u32,
        depth: u8,
        array_key_check: bool,
    ) -> Option<CachedGet> {
        if !property.accessor() {
            return Some(CachedGet::from_data(property.clone_value_packed()));
        }
        self.snapshot_cached_accessor(property, name, holder_shape, depth, array_key_check)
    }

    #[inline(never)]
    fn snapshot_cached_accessor(
        &self,
        property: &Property,
        name: &str,
        holder_shape: u32,
        depth: u8,
        array_key_check: bool,
    ) -> Option<CachedGet> {
        // Lumen's legacy function reflection is handled by get_from_chain, not
        // by Call(%ThrowTypeError%). Retain that extension and its strict-caller
        // censorship; ordinary user-defined getters with these names can cache.
        let getter = property.getter().cloned();
        if matches!(name, "caller" | "arguments") && self.is_throw_type_error(&getter) {
            return None;
        }
        let kind = if getter.is_some() {
            ReadKind::Accessor
        } else {
            ReadKind::MissingGetter
        };
        Some(CachedGet {
            value: PackedValue::pack(getter.unwrap_or(Value::Undefined)),
            holder_shape,
            depth,
            array_key_check,
            kind,
        })
    }

    #[inline(always)]
    pub(super) fn finish_cached_get(
        &mut self,
        read: CachedGet,
        receiver: &Value,
        trace: Option<&mut CurrentPropertyTrace>,
    ) -> Result<Value, Abrupt> {
        if read.kind == ReadKind::Data {
            Ok(read.value.into_value())
        } else {
            self.finish_cached_accessor(read, receiver, trace)
        }
    }

    #[inline(never)]
    fn finish_cached_accessor(
        &mut self,
        read: CachedGet,
        receiver: &Value,
        trace: Option<&mut CurrentPropertyTrace>,
    ) -> Result<Value, Abrupt> {
        if let Some(trace) = trace {
            trace.record(
                PropertyOutcome::Accessor,
                Some(read.holder_shape),
                read.depth,
                None,
            );
            trace.array_key_check = read.array_key_check;
        }
        if read.kind == ReadKind::MissingGetter {
            Ok(Value::Undefined)
        } else {
            let getter = read.value.into_value();
            if let Some(result) = self.try_record_getter(&getter, receiver) {
                return result;
            }
            self.call_callback(getter, receiver.clone(), &[])
        }
    }

    /// Resolve the live getter first, then prove its complete body and every
    /// receiver field. No observer can see an omitted logical frame on a hit;
    /// getters, proxies, conversions, throws and all other bodies keep Call.
    fn try_record_getter(
        &mut self,
        getter: &Value,
        receiver: &Value,
    ) -> Option<Result<Value, Abrupt>> {
        use crate::interpreter::{
            execution_stack_exhausted, with_execution_stack, GC_CALL_POLL_MASK,
        };
        use crate::value::{Callable, Exotic};
        use std::rc::Rc;
        if self.tier != crate::bytecode::Tier::Jit || self.pending_tail.is_some() {
            return None;
        }
        let getter = getter.as_obj()?;
        let receiver = receiver.as_obj()?;
        let key = Rc::as_ptr(getter) as usize;
        if !self.ordinary_get_ptr(key) {
            return None;
        }
        let chunk = {
            let getter = getter.borrow();
            let Callable::User(user) = &getter.call else {
                return None;
            };
            let f = &user.func;
            if user.realm != Rc::as_ptr(&self.global) as usize
                || !f.params.is_empty()
                || f.is_arrow
                || f.is_async
                || f.is_generator
            {
                return None;
            }
            let chunk = f.code.get()?.as_ref()?;
            // Most getters have another body. Reject the cached negative proof
            // before acquiring a chunk owner or inspecting call/receiver state.
            if !chunk.is_record_getter() {
                return None;
            }
            chunk.clone()
        };
        if !self.ordinary_get_ptr(Rc::as_ptr(receiver) as usize)
            || (!self.class_info.is_empty() && self.class_info.contains_key(&key))
            || self.gc_tick.wrapping_add(1) & GC_CALL_POLL_MASK == 0
            || crate::value::heap_live_objects(&self.gc_heap) > self.gc_next
            || execution_stack_exhausted(self.depth + 1)
        {
            return None;
        }
        let values = {
            let receiver = receiver.borrow();
            if !receiver.ic_plain.get() || !matches!(receiver.exotic, Exotic::None) {
                return None;
            }
            chunk.record_getter_values(&receiver.props)?
        };
        self.depth += 1;
        self.gc_tick = self.gc_tick.wrapping_add(1);
        let result = with_execution_stack(self.depth, || {
            self.interrupt_poll_force()?;
            #[cfg(test)]
            TEST_RECORD_GETTERS.with(|count| count.set(count.get() + 1));
            Ok(chunk.make_record_getter_result(self, values))
        });
        self.depth -= 1;
        Some(result)
    }
}

#[cfg(test)]
thread_local! {
    pub(super) static TEST_RECORD_GETTERS: std::cell::Cell<usize> = const {
        std::cell::Cell::new(0)
    };
}
