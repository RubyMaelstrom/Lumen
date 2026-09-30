//! Allocation-free closed Array prefixes for fresh lexical destructuring.
//! ECMA-262 e28783d5: GetIteratorFromMethod, IteratorBindingInitialization, IteratorClose.
use crate::interpreter::{
    execution_stack_exhausted, with_execution_stack, Abrupt, Interp, GC_CALL_POLL_MASK,
};
use crate::value::{Callable, Exotic, Value};
use std::rc::Rc;

impl Interp {
    /// A cloned slot owner is supplied here: two owners can still mean that replacing the
    /// slot will destroy its object when this temporary drops. Keep that observer on the
    /// ordinary protocol, before the next indexed read.
    pub(crate) fn array_binding_old_slot_is_inert(value: &Value) -> bool {
        match value {
            Value::Obj(object) => {
                if Rc::strong_count(object) > 2 {
                    return true;
                }
                let object = object.borrow();
                let Exotic::ArrayIterator(state) = &object.exotic else {
                    return false;
                };
                let shared_edges = match (&object.proto, &state.target) {
                    (Some(proto), Value::Obj(target)) if Rc::ptr_eq(proto, target) => 2,
                    _ => 1,
                };
                // A preceding due-GC miss may leave the actual intrinsic iterator in this
                // slot. Its empty shell is inert if dropping target/prototype cannot be final.
                matches!(object.call, Callable::None)
                    && object.native_typed_array.is_none()
                    && object.props.values().next().is_none()
                    && object
                        .proto
                        .as_ref()
                        .is_none_or(|proto| Rc::strong_count(proto) > shared_edges)
                    && match &state.target {
                        Value::Obj(target) => Rc::strong_count(target) > shared_edges,
                        _ => true,
                    }
            }
            _ => true,
        }
    }

    pub(crate) fn try_array_binding(
        &mut self,
        source: &Value,
        count: usize,
        old_source: &Value,
        old_next: &Value,
    ) -> Result<Option<Value>, Abrupt> {
        // No collector/weak-metadata boundary may run between the opening proof and the
        // last fresh binding. A due boundary takes the original algorithm before any effect.
        if self.pending_tail.is_some()
            || (self.gc_tick & GC_CALL_POLL_MASK) as usize + count + 1 > GC_CALL_POLL_MASK as usize
            || !Self::array_binding_old_slot_is_inert(old_source)
            || !Self::array_binding_old_slot_is_inert(old_next)
        {
            return Ok(None);
        }
        // A custom iterator may also be its own captured next function. Retiring both
        // slots and both temporary clones together must leave an independent owner.
        if matches!((old_source, old_next), (Value::Obj(source), Value::Obj(next))
            if Rc::ptr_eq(source, next) && Rc::strong_count(source) <= 4)
        {
            return Ok(None);
        }
        if let (Value::Obj(source), Value::Obj(next)) = (old_source, old_next) {
            let source = source.borrow();
            if let Exotic::ArrayIterator(state) = &source.exotic {
                let target_edge = usize::from(
                    matches!(&state.target, Value::Obj(target) if Rc::ptr_eq(target, next)),
                );
                let prototype_edge = usize::from(
                    source
                        .proto
                        .as_ref()
                        .is_some_and(|proto| Rc::ptr_eq(proto, next)),
                );
                let retired_edges = target_edge + prototype_edge;
                if retired_edges != 0 && Rc::strong_count(next) <= retired_edges + 2 {
                    return Ok(None);
                }
            }
        }
        let Some((method, next)) =
            crate::builtins::dense_array_binding_methods(self, source, count)
        else {
            return Ok(None);
        };
        let Callable::Native(native) = method.as_obj().expect("native method").borrow().call else {
            unreachable!("binding proof selected native values")
        };
        let started = crate::jit::perf_stage_start();
        let result = self.array_binding_call(native as usize, |_| Ok(()));
        crate::jit::perf_iterator_get_end(started, result.is_ok());
        result?;
        #[cfg(test)]
        TEST_ARRAY_BINDING_OPENS.with(|count| count.set(count.get() + 1));
        Ok(Some(next))
    }

    pub(crate) fn array_binding_step(
        &mut self,
        source: &Value,
        next: &Value,
        index: usize,
    ) -> Result<Value, Abrupt> {
        let Callable::Native(native) = next.as_obj().expect("captured next").borrow().call else {
            unreachable!("binding proof selected native next")
        };
        let started = crate::jit::perf_stage_start();
        let result = self.array_binding_call(native as usize, |i| {
            // The proof covers every own data index and no intervening initialization can
            // execute JS. Keep the checked read available rather than introduce raw layouts.
            i.fast_get_elem(source.as_obj().expect("binding Array"), index as f64)
                .map(Ok)
                .unwrap_or_else(|| i.get_member(source, &index.to_string()))
        });
        crate::jit::perf_iterator_step_end(started, result.is_ok());
        result
    }

    fn array_binding_call<R>(
        &mut self,
        native: usize,
        body: impl FnOnce(&mut Self) -> Result<R, Abrupt>,
    ) -> Result<R, Abrupt> {
        self.depth += 1;
        if execution_stack_exhausted(self.depth) {
            self.depth -= 1;
            return Err(self.throw("RangeError", "Maximum call stack size exceeded"));
        }
        if let Err(error) = self.gc_check_amortized() {
            self.depth -= 1;
            return Err(error);
        }
        let saved_ctor = std::mem::replace(&mut self.constructing, false);
        let saved_target = std::mem::replace(&mut self.new_target, Value::Undefined);
        let result = with_execution_stack(self.depth, || {
            self.interrupt_poll_force()?;
            let started = crate::jit::perf_stage_start();
            let result = body(self);
            crate::jit::perf_native_end(
                started,
                result.is_ok(),
                crate::jit::NativeLabelSrc::Addr(native),
            );
            self.interrupt_poll_force()?;
            result
        });
        self.constructing = saved_ctor;
        self.new_target = saved_target;
        self.depth -= 1;
        result
    }
}

#[cfg(test)]
thread_local! {
    pub(crate) static TEST_ARRAY_BINDING_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(crate) static TEST_JIT_ARRAY_BINDING_OPENS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}
