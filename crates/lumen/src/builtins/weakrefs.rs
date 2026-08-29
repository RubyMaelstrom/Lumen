//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;

/// WeakRef / FinalizationRegistry (ECMA-262 §9.9–9.13 and §26.1–26.2). Targets and unregister
/// tokens are real weak handles; held values and cleanup callbacks remain strong edges of a live
/// registry, and successful dereferences enter the Agent's kept-alive list for the current job.
pub(super) fn install_weak_refs(it: &mut Interp) {
    let wr_proto = Object::new(Some(it.object_proto.clone()));
    it.def_method(&wr_proto, "deref", 0, |i, this, _| {
        let Some(ptr) = map_ptr(&this).filter(|ptr| i.weak_refs.contains_key(ptr)) else {
            return Err(i.make_error("TypeError", "deref called on a non-WeakRef"));
        };
        let target = i
            .weak_refs
            .get(&ptr)
            .and_then(Option::as_ref)
            .and_then(crate::interpreter::WeakTarget::upgrade);
        if let Some(target) = target {
            // WeakRefDeref -> AddToKeptObjects. The host clears this at the next job boundary.
            i.kept_alive.push(target.clone());
            Ok(target)
        } else {
            Ok(Value::Undefined)
        }
    });
    let wr_ctor = it.make_native("WeakRef", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "WeakRef requires 'new'"));
        }
        let target = arg(a, 0);
        if !can_be_held_weakly(i, &target) {
            return Err(i.make_error("TypeError", "WeakRef target must be an object or symbol"));
        }
        let obj = new_from_ctor(i, "WeakRef")?;
        let ptr = Rc::as_ptr(&obj) as usize;
        i.gc_pin(&obj);
        i.kept_alive.push(target.clone());
        i.weak_refs.insert(
            ptr,
            Some(
                crate::interpreter::WeakTarget::of(&target)
                    .expect("CanBeHeldWeakly accepted the WeakRef target"),
            ),
        );
        Ok(Value::Obj(obj))
    });
    it.extra_protos.insert("WeakRef", wr_proto.clone());
    wr_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(wr_proto.clone()), false, false, false),
    );
    wr_proto.borrow_mut().props.insert(
        "constructor",
        Property::builtin(Value::Obj(wr_ctor.clone())),
    );
    if let Some(key) = well_known_key(it, "toStringTag") {
        wr_proto.borrow_mut().props.insert(
            key,
            Property::data(Value::str("WeakRef"), false, false, true),
        );
    }
    set_builtin(&it.global, "WeakRef", Value::Obj(wr_ctor));

    let fr_proto = Object::new(Some(it.object_proto.clone()));
    it.def_method(&fr_proto, "register", 2, |i, this, a| {
        // Brand check, then: target must be registerable, distinct from its held value, and any
        // unregister token must itself be registerable.
        let Some(ptr) = map_ptr(&this).filter(|ptr| i.finalization_registries.contains_key(ptr))
        else {
            return Err(i.make_error("TypeError", "register called on a non-FinalizationRegistry"));
        };
        let target = arg(a, 0);
        if !can_be_held_weakly(i, &target) {
            return Err(i.make_error("TypeError", "target cannot be held weakly"));
        }
        if same_value(&target, &arg(a, 1)) {
            return Err(i.make_error("TypeError", "target and held value must not be the same"));
        }
        let token = arg(a, 2);
        if !matches!(token, Value::Undefined) && !can_be_held_weakly(i, &token) {
            return Err(i.make_error("TypeError", "unregister token cannot be held weakly"));
        }
        let state = i
            .finalization_registries
            .get_mut(&ptr)
            .expect("brand check found FinalizationRegistry state");
        state.cells.push(crate::interpreter::FinalizationCell {
            target: crate::interpreter::WeakTarget::of(&target),
            held_value: arg(a, 1),
            unregister_token: (!matches!(token, Value::Undefined))
                .then(|| crate::interpreter::WeakTarget::of(&token))
                .flatten(),
        });
        Ok(Value::Undefined)
    });
    it.def_method(&fr_proto, "unregister", 1, |i, this, a| {
        let Some(ptr) = map_ptr(&this).filter(|ptr| i.finalization_registries.contains_key(ptr))
        else {
            return Err(i.make_error(
                "TypeError",
                "unregister called on a non-FinalizationRegistry",
            ));
        };
        let token = arg(a, 0);
        if !can_be_held_weakly(i, &token) {
            return Err(i.make_error("TypeError", "unregister token cannot be held weakly"));
        }
        let cells = &mut i
            .finalization_registries
            .get_mut(&ptr)
            .expect("brand check found FinalizationRegistry state")
            .cells;
        let before = cells.len();
        cells.retain(|cell| {
            !cell
                .unregister_token
                .as_ref()
                .and_then(crate::interpreter::WeakTarget::upgrade)
                .is_some_and(|registered| same_value(&registered, &token))
        });
        let removed = cells.len() != before;
        Ok(Value::Bool(removed))
    });
    let fr_ctor = it.make_native("FinalizationRegistry", 1, |i, _t, a| {
        if !i.constructing {
            return Err(i.make_error("TypeError", "FinalizationRegistry requires 'new'"));
        }
        if !arg(a, 0).is_callable() {
            return Err(i.make_error("TypeError", "cleanup callback must be callable"));
        }
        let obj = new_from_ctor(i, "FinalizationRegistry")?;
        let ptr = Rc::as_ptr(&obj) as usize;
        i.gc_pin(&obj);
        i.finalization_registries.insert(
            ptr,
            crate::interpreter::FinalizationState {
                cleanup_callback: arg(a, 0),
                cells: Vec::new(),
                cleanup_scheduled: false,
            },
        );
        Ok(Value::Obj(obj))
    });
    it.extra_protos
        .insert("FinalizationRegistry", fr_proto.clone());
    fr_ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(fr_proto.clone()), false, false, false),
    );
    fr_proto.borrow_mut().props.insert(
        "constructor",
        Property::builtin(Value::Obj(fr_ctor.clone())),
    );
    if let Some(key) = well_known_key(it, "toStringTag") {
        fr_proto.borrow_mut().props.insert(
            key,
            Property::data(Value::str("FinalizationRegistry"), false, false, true),
        );
    }
    set_builtin(&it.global, "FinalizationRegistry", Value::Obj(fr_ctor));
}
