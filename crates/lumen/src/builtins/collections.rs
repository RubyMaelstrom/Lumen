//! Split out of builtins/mod.rs (behavior-preserving move).

use super::*;
use std::hash::{Hash, Hasher};

pub(super) fn install_collections(it: &mut Interp) {
    // A unique private object used as the deleted-entry tombstone key (see map_tombstone).
    it.extra_protos.insert("%MapTombstone%", Object::new(None));
    // %MapIteratorPrototype% / %SetIteratorPrototype%: distinct iterator prototypes (proto is
    // %IteratorPrototype%) with the right @@toStringTag and a live `next`.
    for (key, tag) in [
        ("%MapIteratorPrototype%", "Map Iterator"),
        ("%SetIteratorPrototype%", "Set Iterator"),
    ] {
        let proto = Object::new(it.extra_protos.get("%IteratorPrototype%").cloned());
        set_to_string_tag(it, &proto, tag);
        it.def_method(&proto, "next", 0, map_set_iter_next);
        it.extra_protos.insert(key, proto);
    }
    install_map_like(it, "Map", false, map_ctor);
    install_map_like(it, "Set", true, set_ctor);
    install_weak(it, "WeakMap", false, weakmap_ctor);
    install_weak(it, "WeakSet", true, weakset_ctor);
    install_set_methods(it);
    install_map_methods(it);
}

pub(super) fn install_map_methods(it: &mut Interp) {
    let mp = it.extra_protos.get("Map").cloned().unwrap();
    // getOrInsert(key, value): return the existing value, or insert and return `value`.
    it.def_method(&mp, "getOrInsert", 2, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
        let key = canonicalize_map_key(arg(a, 0));
        if let Some(value) = collection_get(i, ptr, &key) {
            return Ok(value);
        }
        let value = arg(a, 1);
        collection_set(i, ptr, key, value.clone());
        Ok(value)
    });
    it.def_method(&mp, "getOrInsertComputed", 2, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
        // CoerceKey: -0 is canonicalized to +0 (so the callback and stored key see +0).
        let key = canonicalize_map_key(arg(a, 0));
        let cb = arg(a, 1);
        if !cb.is_callable() {
            return Err(i.make_error("TypeError", "callback is not callable"));
        }
        if let Some(value) = collection_get(i, ptr, &key) {
            return Ok(value);
        }
        let value = ab(i.call(cb, Value::Undefined, std::slice::from_ref(&key)))?;
        // The callback may have inserted the key; the computed value overwrites that mutation.
        collection_set(i, ptr, key, value.clone());
        Ok(value)
    });
}

/// The receiver Set's values (deduped insertion order). Errors if `this` isn't a Set.
fn set_values(i: &mut Interp, this: &Value) -> Result<Vec<Value>, Value> {
    // Requires a real Set [[SetData]] slot — a Map (which shares the map_data table) is rejected.
    let p = coll_ptr_kind(i, this, Some("Set"))?;
    Ok(i.map_data[&p]
        .iter()
        .filter(|(key, _)| !is_tombstone(i, key))
        .map(|(key, _)| key.clone())
        .collect())
}

/// Find a SameValueZero match in a temporary result using the same hash partitioning as Map/Set.
/// Set operations can otherwise devolve to an O(n²) scan when a set-like iterator yields many
/// values. Hash collisions still go through the full equality check.
fn result_index_find(
    values: &[Value],
    index: &crate::fasthash::FastMap<u64, Vec<usize>>,
    key: &Value,
) -> Option<usize> {
    let hash = collection_key_hash(key);
    index.get(&hash)?.iter().copied().find(|&offset| {
        values
            .get(offset)
            .is_some_and(|candidate| same_value_zero(candidate, key))
    })
}

fn result_index_insert(
    values: &mut Vec<Value>,
    index: &mut crate::fasthash::FastMap<u64, Vec<usize>>,
    value: Value,
) -> bool {
    if result_index_find(values, index, &value).is_some() {
        return false;
    }
    let offset = values.len();
    let hash = collection_key_hash(&value);
    values.push(value);
    index.entry(hash).or_default().push(offset);
    true
}

fn option_result_index_find(
    values: &[Option<Value>],
    index: &crate::fasthash::FastMap<u64, Vec<usize>>,
    key: &Value,
) -> Option<usize> {
    let hash = collection_key_hash(key);
    index.get(&hash)?.iter().copied().find(|&offset| {
        values
            .get(offset)
            .and_then(Option::as_ref)
            .is_some_and(|candidate| same_value_zero(candidate, key))
    })
}

fn option_result_index_insert(
    values: &mut Vec<Option<Value>>,
    index: &mut crate::fasthash::FastMap<u64, Vec<usize>>,
    value: Value,
) -> bool {
    if option_result_index_find(values, index, &value).is_some() {
        return false;
    }
    let offset = values.len();
    let hash = collection_key_hash(&value);
    values.push(Some(value));
    index.entry(hash).or_default().push(offset);
    true
}

fn option_result_index_remove(
    index: &mut crate::fasthash::FastMap<u64, Vec<usize>>,
    key: &Value,
    offset: usize,
) {
    let hash = collection_key_hash(key);
    let mut empty = false;
    if let Some(bucket) = index.get_mut(&hash) {
        bucket.retain(|&candidate| candidate != offset);
        empty = bucket.is_empty();
    }
    if empty {
        index.remove(&hash);
    }
}

/// Build a fresh Set from `values` (deduped via SameValueZero).
fn new_set(i: &mut Interp, values: Vec<Value>) -> Value {
    let obj =
        new_from_ctor(i, "Set").unwrap_or_else(|_| Object::new(i.extra_protos.get("Set").cloned()));
    let ptr = Rc::as_ptr(&obj) as usize;
    i.gc_pin(&obj);
    i.map_data.insert(ptr, Vec::new());
    i.collection_index.insert(ptr, Default::default());
    for value in values {
        let value = canonicalize_map_key(value);
        collection_set(i, ptr, value.clone(), value);
    }
    set_internal(&obj, "__ck", Value::str("Set"));
    Value::Obj(obj)
}
/// GetSetRecord: a set-like `other` exposes a numeric `size`, and callable `has` and `keys`.
fn set_record(i: &mut Interp, other: &Value) -> Result<(Value, Value, f64), Value> {
    if !matches!(other, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "argument is not an object"));
    }
    // GetSetRecord: size → ToNumber (NaN throws TypeError), ToIntegerOrInfinity (negative throws
    // RangeError); then `has` and `keys` must be callable.
    let size_v = ab(i.get_member(other, "size"))?;
    let size = ab(i.to_number(&size_v))?;
    if size.is_nan() {
        return Err(i.make_error("TypeError", "set-like size is NaN"));
    }
    let int_size = if size.is_infinite() {
        size
    } else {
        size.trunc()
    };
    if int_size < 0.0 {
        return Err(i.make_error("RangeError", "set-like size is negative"));
    }
    let has = ab(i.get_member(other, "has"))?;
    if !has.is_callable() {
        return Err(i.make_error("TypeError", "set-like has is not callable"));
    }
    let keys = ab(i.get_member(other, "keys"))?;
    if !keys.is_callable() {
        return Err(i.make_error("TypeError", "set-like keys is not callable"));
    }
    Ok((has, keys, int_size))
}
fn set_like_has(i: &mut Interp, has: &Value, other: &Value, v: &Value) -> Result<bool, Value> {
    let r = ab(i.call(has.clone(), other.clone(), std::slice::from_ref(v)))?;
    Ok(i.to_boolean(&r))
}
/// Open a set-like's keys iterator record: `(iterator, nextMethod)`.
fn set_like_open(i: &mut Interp, keys: &Value, other: &Value) -> Result<(Value, Value), Value> {
    let iter = ab(i.call(keys.clone(), other.clone(), &[]))?;
    let next = ab(i.get_member(&iter, "next"))?;
    if !next.is_callable() {
        return Err(i.make_error("TypeError", "set-like keys iterator has no next method"));
    }
    Ok((iter, next))
}

/// Step a set-like keys iterator: `Some(value)` or `None` when done. `-0` is canonicalized to `+0`.
fn set_like_next(i: &mut Interp, iter: &Value, next: &Value) -> Result<Option<Value>, Value> {
    let r = ab(i.call(next.clone(), iter.clone(), &[]))?;
    if !matches!(r, Value::Obj(_)) {
        return Err(i.make_error("TypeError", "iterator result is not an object"));
    }
    let done = ab(i.get_member(&r, "done"))?;
    if i.to_boolean(&done) {
        Ok(None)
    } else {
        Ok(Some(canonicalize_map_key(ab(i.get_member(&r, "value"))?)))
    }
}

/// IteratorClose a set-like keys iterator on early exit (swallowing errors).
fn set_like_close(i: &mut Interp, iter: &Value) {
    if let Ok(ret) = i.get_member(iter, "return") {
        if ret.is_callable() {
            let _ = i.call(ret, iter.clone(), &[]);
        }
    }
}

fn set_like_keys(i: &mut Interp, keys: &Value, other: &Value) -> Result<Vec<Value>, Value> {
    // `keys` returns an iterator *record*: step its `next` directly rather than calling GetIterator
    // (the result need not be iterable itself).
    let iter = ab(i.call(keys.clone(), other.clone(), &[]))?;
    let next = ab(i.get_member(&iter, "next"))?;
    if !next.is_callable() {
        return Err(i.make_error("TypeError", "set-like keys iterator has no next method"));
    }
    let mut out = Vec::new();
    loop {
        let r = ab(i.call(next.clone(), iter.clone(), &[]))?;
        if !matches!(r, Value::Obj(_)) {
            return Err(i.make_error("TypeError", "iterator result is not an object"));
        }
        let done = ab(i.get_member(&r, "done"))?;
        if i.to_boolean(&done) {
            break;
        }
        out.push(ab(i.get_member(&r, "value"))?);
    }
    Ok(out)
}

/// SetDataHas against the LIVE backing data (skipping tombstones) — set-like callbacks may have
/// mutated the receiver since any snapshot was taken.
fn set_data_has(i: &Interp, ptr: usize, v: &Value) -> bool {
    collection_has(i, ptr, v)
}

pub(super) fn install_set_methods(it: &mut Interp) {
    let sp = it.extra_protos.get("Set").cloned().unwrap();
    it.def_method(&sp, "union", 1, |i, this, a| {
        // GetSetRecord (which may run `has`/`size`/`keys` getters that mutate this Set) happens
        // BEFORE the result is snapshotted from O.[[SetData]], per spec.
        coll_ptr_kind(i, &this, Some("Set"))?;
        let (_has, keys, _size) = set_record(i, &arg(a, 0))?;
        // GetKeysIterator (keys() call + `next` get) precedes the [[SetData]] copy, so mutations
        // those getters make to the receiver are visible in the result.
        let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
        let mut vals = set_values(i, &this)?;
        let mut result_index = crate::fasthash::FastMap::default();
        for (offset, value) in vals.iter().enumerate() {
            result_index
                .entry(collection_key_hash(value))
                .or_insert_with(Vec::new)
                .push(offset);
        }
        while let Some(k) = set_like_next(i, &iter, &next)? {
            result_index_insert(&mut vals, &mut result_index, k);
        }
        Ok(new_set(i, vals))
    });
    it.def_method(&sp, "intersection", 1, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
        let (has, keys, other_size) = set_record(i, &arg(a, 0))?;
        let mut out = Vec::new();
        let mut result_index = crate::fasthash::FastMap::default();
        if (coll_live_len(i, ptr) as f64) <= other_size {
            // Walk this Set LIVE by index, probing the other's `has` — the callback may delete
            // and re-append entries, and the walk observes that (appended entries are visited).
            let mut idx = 0usize;
            loop {
                let entry = i.map_data.get(&ptr).and_then(|e| e.get(idx).cloned());
                idx += 1;
                let (k, _) = match entry {
                    Some(kv) => kv,
                    None => break,
                };
                if is_tombstone(i, &k) {
                    continue;
                }
                if set_like_has(i, &has, &arg(a, 0), &k)? {
                    result_index_insert(&mut out, &mut result_index, k);
                }
            }
        } else {
            // Iterate the other's keys, probing this Set's LIVE data (no `has` calls on the other).
            let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
            while let Some(k) = set_like_next(i, &iter, &next)? {
                if set_data_has(i, ptr, &k) {
                    result_index_insert(&mut out, &mut result_index, k);
                }
            }
        }
        Ok(new_set(i, out))
    });
    it.def_method(&sp, "difference", 1, |i, this, a| {
        coll_ptr_kind(i, &this, Some("Set"))?;
        let (has, keys, other_size) = set_record(i, &arg(a, 0))?;
        let vals = set_values(i, &this)?;
        if (vals.len() as f64) <= other_size {
            // Iterate this Set, dropping elements the other's `has` reports.
            let mut out = Vec::new();
            for v in vals {
                if !set_like_has(i, &has, &arg(a, 0), &v)? {
                    out.push(v);
                }
            }
            Ok(new_set(i, out))
        } else {
            // Start from this Set and remove each of the other's keys.
            let mut out = vals;
            for k in set_like_keys(i, &keys, &arg(a, 0))? {
                out.retain(|v| !same_value_zero(v, &k));
            }
            Ok(new_set(i, out))
        }
    });
    it.def_method(&sp, "symmetricDifference", 1, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
        let (_has, keys, _size) = set_record(i, &arg(a, 0))?;
        // GetKeysIterator precedes the [[SetData]] copy. For each key of `other`: present in the
        // LIVE receiver → remove it from the result (in both); absent → append if not already in
        // the result (only in other). Removal empties the slot (order is preserved).
        let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
        let mut result: Vec<Option<Value>> = set_values(i, &this)?.into_iter().map(Some).collect();
        let mut result_index = crate::fasthash::FastMap::default();
        for (offset, value) in result.iter().enumerate() {
            if let Some(value) = value {
                result_index
                    .entry(collection_key_hash(value))
                    .or_insert_with(Vec::new)
                    .push(offset);
            }
        }
        while let Some(k) = set_like_next(i, &iter, &next)? {
            let result_offset = option_result_index_find(&result, &result_index, &k);
            if set_data_has(i, ptr, &k) {
                if let Some(offset) = result_offset {
                    result[offset] = None;
                    option_result_index_remove(&mut result_index, &k, offset);
                }
            } else if result_offset.is_none() {
                option_result_index_insert(&mut result, &mut result_index, k);
            }
        }
        let out: Vec<Value> = result.into_iter().flatten().collect();
        Ok(new_set(i, out))
    });
    it.def_method(&sp, "isSubsetOf", 1, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
        let (has, _keys, other_size) = set_record(i, &arg(a, 0))?;
        // A larger set cannot be a subset; otherwise every element must be in the other. The
        // receiver's data is walked LIVE by index (the `has` callback may delete entries).
        if (coll_live_len(i, ptr) as f64) > other_size {
            return Ok(Value::Bool(false));
        }
        let mut idx = 0usize;
        loop {
            let entry = i.map_data.get(&ptr).and_then(|e| e.get(idx).cloned());
            idx += 1;
            let (k, _) = match entry {
                Some(kv) => kv,
                None => break,
            };
            if is_tombstone(i, &k) {
                continue;
            }
            if !set_like_has(i, &has, &arg(a, 0), &k)? {
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    it.def_method(&sp, "isSupersetOf", 1, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
        let (_has, keys, other_size) = set_record(i, &arg(a, 0))?;
        // A smaller set cannot be a superset; otherwise every other key must be in this. The
        // other's keys are iterated lazily against the LIVE receiver data (the iterator may add
        // entries), closing the iterator if a missing key exits early.
        if (coll_live_len(i, ptr) as f64) < other_size {
            return Ok(Value::Bool(false));
        }
        let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
        while let Some(k) = set_like_next(i, &iter, &next)? {
            if !set_data_has(i, ptr, &k) {
                set_like_close(i, &iter);
                return Ok(Value::Bool(false));
            }
        }
        Ok(Value::Bool(true))
    });
    it.def_method(&sp, "isDisjointFrom", 1, |i, this, a| {
        let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
        let (has, keys, other_size) = set_record(i, &arg(a, 0))?;
        if (coll_live_len(i, ptr) as f64) <= other_size {
            // Walk this Set LIVE by index (the `has` callback may mutate it), probing the other.
            let mut idx = 0usize;
            loop {
                let entry = i.map_data.get(&ptr).and_then(|e| e.get(idx).cloned());
                idx += 1;
                let (k, _) = match entry {
                    Some(kv) => kv,
                    None => break,
                };
                if is_tombstone(i, &k) {
                    continue;
                }
                if set_like_has(i, &has, &arg(a, 0), &k)? {
                    return Ok(Value::Bool(false));
                }
            }
        } else {
            // Iterate the other's keys lazily, probing this Set; close the iterator on early exit.
            let (iter, next) = set_like_open(i, &keys, &arg(a, 0))?;
            while let Some(k) = set_like_next(i, &iter, &next)? {
                // SetDataHas is deliberately live: the arbitrary other's iterator can mutate the
                // receiver between steps (ECMA-262 §24.2.4.10).
                if set_data_has(i, ptr, &k) {
                    set_like_close(i, &iter);
                    return Ok(Value::Bool(false));
                }
            }
        }
        Ok(Value::Bool(true))
    });
}

// Non-capturing constructor entry points (native fns must be bare `fn` pointers).
fn map_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, "Map", false)
}
fn set_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, "Set", true)
}
fn weakmap_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, "WeakMap", false)
}
fn weakset_ctor(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    collection_ctor(i, a, "WeakSet", true)
}

fn collection_ctor(
    i: &mut Interp,
    args: &[Value],
    name: &str,
    is_set: bool,
) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Constructor requires 'new'"));
    }
    let obj = new_from_ctor(i, name)?;
    let ptr = Rc::as_ptr(&obj) as usize;
    i.gc_pin(&obj);
    if name.starts_with("Weak") {
        i.weak_collection_data.insert(ptr, Vec::new());
        i.weak_collection_index.insert(ptr, Default::default());
    } else {
        i.map_data.insert(ptr, Vec::new());
        i.collection_index.insert(ptr, Default::default());
    }
    // Brand the instance so prototype methods can reject cross-collection receivers.
    set_internal(&obj, "__ck", Value::str(name));
    let mv = Value::Obj(obj);
    if let Some(src) = args.first() {
        if !matches!(src, Value::Undefined | Value::Null) {
            let add_fn = ab(i.get_member(&mv, if is_set { "add" } else { "set" }))?;
            if !add_fn.is_callable() {
                return Err(i.make_error("TypeError", "adder is not callable"));
            }
            // Step the source lazily: an error while processing an entry closes the iterator.
            let (iter, next) = ab(i.get_iterator(src))?;
            loop {
                let item = match step_iter_with(i, &iter, &next)? {
                    Some(v) => v,
                    None => break,
                };
                let step = if is_set {
                    i.call(add_fn.clone(), mv.clone(), &[item])
                } else if !matches!(item, Value::Obj(_)) {
                    Err(crate::interpreter::Abrupt::Throw(i.make_error(
                        "TypeError",
                        "iterator value is not an entry object",
                    )))
                } else {
                    i.get_member(&item, "0")
                        .and_then(|k| i.get_member(&item, "1").map(|v| (k, v)))
                        .and_then(|(k, v)| i.call(add_fn.clone(), mv.clone(), &[k, v]))
                };
                if let Err(e) = step {
                    i.iterator_close(&iter);
                    return Err(crate::interpreter::abrupt_value(e));
                }
            }
        }
    }
    Ok(mv)
}

/// The Map/Set tombstone sentinel key: a unique engine-private object placed at a deleted entry's
/// slot so positions stay stable (live iteration observes additions and skips deletions, per spec).
fn map_tombstone(i: &Interp) -> Value {
    match i.extra_protos.get("%MapTombstone%") {
        Some(o) => Value::Obj(o.clone()),
        None => Value::Undefined,
    }
}

/// Count the live (non-tombstone) entries of a collection.
fn coll_live_len(i: &Interp, ptr: usize) -> usize {
    i.collection_index
        .get(&ptr)
        .map_or(0, crate::fasthash::FastMap::len)
}

/// Locate a live ordered entry through the SameValueZero hash index.
fn collection_entry_index(i: &Interp, ptr: usize, key: &Value) -> Option<usize> {
    let hash = collection_key_hash(key);
    let bucket = i.collection_index.get(&ptr)?.get(&hash)?;
    let entries = i.map_data.get(&ptr)?;
    let matches = |offset: usize| {
        entries
            .get(offset)
            .is_some_and(|(candidate, _)| same_value_zero(candidate, key))
    };
    match bucket {
        crate::interpreter::CollectionBucket::One(offset) => matches(*offset).then_some(*offset),
        crate::interpreter::CollectionBucket::Many(offsets) => {
            offsets.iter().copied().find(|offset| matches(*offset))
        }
    }
}

fn collection_get(i: &Interp, ptr: usize, key: &Value) -> Option<Value> {
    let index = collection_entry_index(i, ptr, key)?;
    i.map_data
        .get(&ptr)?
        .get(index)
        .map(|(_, value)| value.clone())
}

fn collection_has(i: &Interp, ptr: usize, key: &Value) -> bool {
    collection_entry_index(i, ptr, key).is_some()
}

/// Set an existing ordered entry or append a new one. Deleted slots are never reused: delete then
/// reinsert must move the key to the end, and live iterators must observe that append.
fn collection_set(i: &mut Interp, ptr: usize, key: Value, value: Value) {
    let key = canonicalize_map_key(key);
    if let Some(index) = collection_entry_index(i, ptr, &key) {
        if let Some(entry) = i
            .map_data
            .get_mut(&ptr)
            .and_then(|entries| entries.get_mut(index))
        {
            entry.1 = value;
        }
        return;
    }
    let entries = i.map_data.entry(ptr).or_default();
    let index = entries.len();
    entries.push((key, value));
    let hash = collection_key_hash(&entries[index].0);
    use crate::interpreter::CollectionBucket;
    match i.collection_index.entry(ptr).or_default().entry(hash) {
        std::collections::hash_map::Entry::Vacant(entry) => {
            entry.insert(CollectionBucket::One(index));
        }
        std::collections::hash_map::Entry::Occupied(mut entry) => match entry.get_mut() {
            CollectionBucket::One(previous) => {
                *entry.get_mut() = CollectionBucket::Many(vec![*previous, index]);
            }
            CollectionBucket::Many(offsets) => offsets.push(index),
        },
    }
}

fn collection_delete(i: &mut Interp, ptr: usize, key: &Value) -> bool {
    let Some(index) = collection_entry_index(i, ptr, key) else {
        return false;
    };
    let hash = collection_key_hash(key);
    let mut remove_bucket = false;
    if let Some(bucket) = i
        .collection_index
        .get_mut(&ptr)
        .and_then(|collection| collection.get_mut(&hash))
    {
        match bucket {
            crate::interpreter::CollectionBucket::One(_) => remove_bucket = true,
            crate::interpreter::CollectionBucket::Many(offsets) => {
                offsets.retain(|offset| *offset != index);
                if offsets.len() == 1 {
                    *bucket = crate::interpreter::CollectionBucket::One(offsets[0]);
                }
            }
        }
    }
    if remove_bucket {
        if let Some(collection) = i.collection_index.get_mut(&ptr) {
            collection.remove(&hash);
        }
    }
    let tombstone = map_tombstone(i);
    if let Some(entry) = i
        .map_data
        .get_mut(&ptr)
        .and_then(|entries| entries.get_mut(index))
    {
        entry.0 = tombstone;
        entry.1 = Value::Undefined;
    }
    true
}

/// Hash a Map/Set key according to SameValueZero. A bucket hit is always checked with the full
/// equality relation, so ordinary hash collisions cannot alias distinct JavaScript keys. Keeping
/// only the hash in the parallel index also avoids retaining a second String/BigInt owner.
fn collection_key_hash(value: &Value) -> u64 {
    let mut state = crate::fasthash::FxHasher::default();
    match value {
        Value::Undefined => state.write_u8(0),
        Value::Empty => state.write_u8(1),
        Value::Null => state.write_u8(2),
        Value::Bool(value) => {
            state.write_u8(3);
            value.hash(&mut state);
        }
        Value::Num(value) => {
            state.write_u8(4);
            let bits = if *value == 0.0 {
                0.0f64.to_bits()
            } else if value.is_nan() {
                f64::NAN.to_bits()
            } else {
                value.to_bits()
            };
            state.write_u64(bits);
        }
        Value::BigInt(value) => {
            state.write_u8(5);
            value.hash(&mut state);
        }
        Value::Str(value) => {
            state.write_u8(6);
            value.hash(&mut state);
        }
        Value::Sym(value) => {
            state.write_u8(7);
            state.write_u64(value.id);
        }
        Value::Obj(value) => {
            state.write_u8(8);
            state.write_usize(Rc::as_ptr(value) as usize);
        }
    }
    state.finish()
}

fn collection_clear(i: &mut Interp, ptr: usize) {
    let tombstone = map_tombstone(i);
    if let Some(entries) = i.map_data.get_mut(&ptr) {
        for entry in entries {
            entry.0 = tombstone.clone();
            entry.1 = Value::Undefined;
        }
    }
    if let Some(index) = i.collection_index.get_mut(&ptr) {
        index.clear();
    }
}

fn map_size(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
    Ok(Value::Num(coll_live_len(i, ptr) as f64))
}

fn set_size(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
    Ok(Value::Num(coll_live_len(i, ptr) as f64))
}

/// CoerceKey for Map/Set: `-0` is canonicalized to `+0` so a stored key (and any key handed to a
/// callback or iterated) is `+0`, per spec.
fn canonicalize_map_key(k: Value) -> Value {
    match k {
        Value::Num(n) if n == 0.0 && n.is_sign_negative() => Value::Num(0.0),
        other => other,
    }
}

/// Map and Set share almost everything; `is_set` flips key/value handling and method names.
pub(super) fn install_map_like(
    it: &mut Interp,
    name: &'static str,
    is_set: bool,
    ctor_fn: NativeFn,
) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert(name, proto.clone());

    let adder: NativeFn = if is_set {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
            let key = canonicalize_map_key(arg(a, 0));
            collection_set(i, ptr, key.clone(), key);
            Ok(this)
        }
    } else {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let (key, val) = (canonicalize_map_key(arg(a, 0)), arg(a, 1));
            collection_set(i, ptr, key, val);
            Ok(this)
        }
    };
    it.def_method(
        &proto,
        if is_set { "add" } else { "set" },
        if is_set { 1 } else { 2 },
        adder,
    );
    if !is_set {
        it.def_method(&proto, "get", 1, |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let key = canonicalize_map_key(arg(a, 0));
            Ok(collection_get(i, ptr, &key).unwrap_or(Value::Undefined))
        });
    }
    // has/delete are shared but brand-check the exact kind via kind-specific fn pointers.
    let has_fn: NativeFn = if is_set {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
            let key = canonicalize_map_key(arg(a, 0));
            Ok(Value::Bool(collection_has(i, ptr, &key)))
        }
    } else {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let key = canonicalize_map_key(arg(a, 0));
            Ok(Value::Bool(collection_has(i, ptr, &key)))
        }
    };
    it.def_method(&proto, "has", 1, has_fn);
    // Delete marks the matching entry with a tombstone (keeping its slot) so a concurrent forEach /
    // iterator sees stable positions; the entry is otherwise treated as absent everywhere.
    let delete_fn: NativeFn = if is_set {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
            let key = canonicalize_map_key(arg(a, 0));
            Ok(Value::Bool(collection_delete(i, ptr, &key)))
        }
    } else {
        |i, this, a| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            let key = canonicalize_map_key(arg(a, 0));
            Ok(Value::Bool(collection_delete(i, ptr, &key)))
        }
    };
    it.def_method(&proto, "delete", 1, delete_fn);
    // clear/forEach/values/keys/entries/size are shared shapes but must brand-check the exact kind
    // (Set.prototype.clear rejects a Map and vice-versa), so select a kind-specific fn pointer.
    let clear_fn: NativeFn = if is_set {
        |i, this, _| {
            let ptr = coll_ptr_kind(i, &this, Some("Set"))?;
            collection_clear(i, ptr);
            Ok(Value::Undefined)
        }
    } else {
        |i, this, _| {
            let ptr = coll_ptr_kind(i, &this, Some("Map"))?;
            collection_clear(i, ptr);
            Ok(Value::Undefined)
        }
    };
    it.def_method(&proto, "clear", 0, clear_fn);
    let for_each_fn: NativeFn = if is_set {
        |i, this, a| collection_for_each(i, this, a, Some("Set"))
    } else {
        |i, this, a| collection_for_each(i, this, a, Some("Map"))
    };
    it.def_method(&proto, "forEach", 1, for_each_fn);
    let (values_fn, keys_fn, entries_fn): (NativeFn, NativeFn, NativeFn) = if is_set {
        (
            |i, this, _| collection_iter_kind(i, &this, 0, "Set"),
            |i, this, _| collection_iter_kind(i, &this, 1, "Set"),
            |i, this, _| collection_iter_kind(i, &this, 2, "Set"),
        )
    } else {
        (
            |i, this, _| collection_iter_kind(i, &this, 0, "Map"),
            |i, this, _| collection_iter_kind(i, &this, 1, "Map"),
            |i, this, _| collection_iter_kind(i, &this, 2, "Map"),
        )
    };
    it.def_method(&proto, "values", 0, values_fn);
    it.def_method(&proto, "entries", 0, entries_fn);
    if is_set {
        // Set.prototype.keys is the *same* function object as Set.prototype.values.
        let _ = keys_fn;
        let values_prop = proto.borrow().props.get("values").cloned();
        if let Some(p) = values_prop {
            proto.borrow_mut().props.insert("keys", p);
        }
    } else {
        it.def_method(&proto, "keys", 0, keys_fn);
    }

    // `size` accessor.
    // Map.prototype.size and Set.prototype.size each brand-check their own kind (a Set passed to
    // Map.prototype.size, or vice versa, is a TypeError — it lacks the right internal slot).
    let size_getter = it.make_native("get size", 0, if is_set { set_size } else { map_size });
    proto.borrow_mut().props.insert(
        "size",
        Property::accessor_prop(Some(Value::Obj(size_getter)), None, false, true),
    );
    // @@iterator: Set -> values, Map -> entries.
    if let Some(sym) = it.iterator_sym.clone() {
        let default = if is_set { "values" } else { "entries" };
        let f = proto
            .borrow()
            .props
            .get(default)
            .map(|p| p.value())
            .unwrap();
        proto
            .borrow_mut()
            .props
            .insert(Interp::sym_key(&sym), Property::builtin(f));
    }

    let ctor = it.make_native(name, 0, ctor_fn);
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    if !is_set {
        // Map.groupBy(items, cb) -> a Map of key -> [items...].
        it.def_method(&ctor, "groupBy", 2, |i, _t, a| {
            let cb = arg(a, 1);
            if !cb.is_callable() {
                return Err(i.make_error("TypeError", "Map.groupBy callback is not callable"));
            }
            let elems = ab(i.iterate(&arg(a, 0)))?;
            let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
            // GroupBy uses SameValueZero keys, so partition the temporary groups by the same
            // collision-safe hash used by Map/Set instead of rescanning every prior group.
            let mut group_index: crate::fasthash::FastMap<u64, Vec<usize>> = Default::default();
            for (idx, el) in elems.into_iter().enumerate() {
                let key = ab(i.call(
                    cb.clone(),
                    Value::Undefined,
                    &[el.clone(), Value::Num(idx as f64)],
                ))?;
                let hash = collection_key_hash(&key);
                let existing = group_index.get(&hash).and_then(|bucket| {
                    bucket
                        .iter()
                        .copied()
                        .find(|&offset| same_value_zero(&groups[offset].0, &key))
                });
                if let Some(offset) = existing {
                    groups[offset].1.push(el);
                } else {
                    let offset = groups.len();
                    groups.push((key, vec![el]));
                    group_index.entry(hash).or_default().push(offset);
                }
            }
            let m = Object::new(i.extra_protos.get("Map").cloned());
            let ptr = Rc::as_ptr(&m) as usize;
            i.gc_pin(&m);
            i.map_data.insert(ptr, Vec::new());
            i.collection_index.insert(ptr, Default::default());
            for (key, values) in groups {
                let values = i.make_array(values);
                collection_set(i, ptr, key, values);
            }
            set_internal(&m, "__ck", Value::str("Map"));
            Ok(Value::Obj(m))
        });
    }
    install_species(it, &ctor); // Map/Set carry @@species
    set_to_string_tag(it, &proto, name);
    set_builtin(&it.global, name, Value::Obj(ctor));
}

/// WeakMap/WeakSet: like Map/Set but keys must be objects or unregistered symbols and there is no
/// iteration/size. ECMA-262 requires average sublinear access, so all operations use the parallel
/// identity index instead of scanning the specification's conceptual List.
/// Resolve the backing-store pointer for a weak-collection receiver, enforcing its brand: `want` is
/// the exact kind ("WeakMap"/"WeakSet") for kind-specific methods, or "Weak" to accept either for
/// the methods (has/delete) shared by both.
fn weak_brand_ptr(i: &mut Interp, this: &Value, want: &str) -> Result<usize, Value> {
    let ptr = map_ptr(this)
        .filter(|p| i.weak_collection_data.contains_key(p))
        .ok_or_else(|| i.make_error("TypeError", "method called on incompatible receiver"))?;
    let kind = this
        .as_obj()
        .and_then(|o| o.borrow().props.get("__ck").map(|p| p.value()));
    let ok = match &kind {
        Some(Value::Str(s)) if want == "Weak" => s.starts_with("Weak"),
        Some(Value::Str(s)) => &**s == want,
        _ => false,
    };
    if !ok {
        return Err(i.make_error("TypeError", "method called on incompatible receiver"));
    }
    Ok(ptr)
}

fn weak_entry_index(i: &Interp, ptr: usize, key: &Value) -> Option<usize> {
    let identity = crate::interpreter::WeakKey::of(key)?;
    i.weak_collection_index.get(&ptr)?.get(&identity).copied()
}

fn weak_insert(i: &mut Interp, ptr: usize, key: Value, value: Value) {
    let target = crate::interpreter::WeakTarget::of(&key)
        .expect("WeakMap and WeakSet entries have weakly holdable keys");
    let identity = target.key();
    if let Some(index) = i
        .weak_collection_index
        .get(&ptr)
        .and_then(|index| index.get(&identity))
        .copied()
    {
        i.weak_collection_data
            .get_mut(&ptr)
            .expect("a weak collection has backing data")[index]
            .1 = value;
        return;
    }
    let entries = i
        .weak_collection_data
        .get_mut(&ptr)
        .expect("a weak collection has backing data");
    let index = entries.len();
    entries.push((target, value));
    i.weak_collection_index
        .entry(ptr)
        .or_default()
        .insert(identity, index);
}

fn weak_delete(i: &mut Interp, ptr: usize, key: &Value) -> bool {
    let Some(identity) = crate::interpreter::WeakKey::of(key) else {
        return false;
    };
    let Some(index) = i
        .weak_collection_index
        .get_mut(&ptr)
        .and_then(|entries| entries.remove(&identity))
    else {
        return false;
    };
    // Weak collection order cannot be observed, so compact immediately. Repair the moved entry's
    // offset after swap_remove to keep all subsequent operations constant-time.
    let entries = i
        .weak_collection_data
        .get_mut(&ptr)
        .expect("a weak collection has backing data");
    entries.swap_remove(index);
    if let Some((moved_key, _)) = entries.get(index) {
        let moved_identity = moved_key.key();
        i.weak_collection_index
            .get_mut(&ptr)
            .expect("a weak collection has an identity index")
            .insert(moved_identity, index);
    }
    true
}

pub(super) fn install_weak(it: &mut Interp, name: &'static str, is_set: bool, ctor_fn: NativeFn) {
    let proto = Object::new(Some(it.object_proto.clone()));
    it.extra_protos.insert(name, proto.clone());
    let adder: NativeFn = if is_set {
        |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, "WeakSet")?;
            let key = arg(a, 0);
            if !can_be_held_weakly(i, &key) {
                return Err(i.make_error("TypeError", "Invalid value used in weak set"));
            }
            if weak_entry_index(i, ptr, &key).is_none() {
                weak_insert(i, ptr, key.clone(), key);
            }
            Ok(this)
        }
    } else {
        |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, "WeakMap")?;
            let (key, val) = (arg(a, 0), arg(a, 1));
            if !can_be_held_weakly(i, &key) {
                return Err(i.make_error("TypeError", "Invalid value used as weak map key"));
            }
            weak_insert(i, ptr, key, val);
            Ok(this)
        }
    };
    it.def_method(
        &proto,
        if is_set { "add" } else { "set" },
        if is_set { 1 } else { 2 },
        adder,
    );
    if !is_set {
        it.def_method(&proto, "get", 1, |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, "WeakMap")?;
            let key = arg(a, 0);
            Ok(weak_entry_index(i, ptr, &key)
                .and_then(|index| i.weak_collection_data.get(&ptr)?.get(index))
                .map(|(_, value)| value.clone())
                .unwrap_or(Value::Undefined))
        });
        // Upsert proposal: getOrInsert(key, value) / getOrInsertComputed(key, callbackfn).
        it.def_method(&proto, "getOrInsert", 2, |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, "WeakMap")?;
            let key = arg(a, 0);
            if !can_be_held_weakly(i, &key) {
                return Err(i.make_error("TypeError", "Invalid value used as weak map key"));
            }
            if let Some(index) = weak_entry_index(i, ptr, &key) {
                return Ok(i.weak_collection_data[&ptr][index].1.clone());
            }
            let value = arg(a, 1);
            weak_insert(i, ptr, key, value.clone());
            Ok(value)
        });
        it.def_method(&proto, "getOrInsertComputed", 2, |i, this, a| {
            let ptr = weak_brand_ptr(i, &this, "WeakMap")?;
            let key = arg(a, 0);
            if !can_be_held_weakly(i, &key) {
                return Err(i.make_error("TypeError", "Invalid value used as weak map key"));
            }
            let cb = arg(a, 1);
            if !cb.is_callable() {
                return Err(i.make_error("TypeError", "callback is not callable"));
            }
            if let Some(index) = weak_entry_index(i, ptr, &key) {
                return Ok(i.weak_collection_data[&ptr][index].1.clone());
            }
            let value = ab(i.call(cb, Value::Undefined, std::slice::from_ref(&key)))?;
            // The callback may have inserted the key; the computed value overwrites that mutation.
            weak_insert(i, ptr, key, value.clone());
            Ok(value)
        });
    }
    it.def_method(&proto, "has", 1, |i, this, a| {
        let ptr = weak_brand_ptr(i, &this, "Weak")?;
        let key = arg(a, 0);
        Ok(Value::Bool(weak_entry_index(i, ptr, &key).is_some()))
    });
    it.def_method(&proto, "delete", 1, |i, this, a| {
        let ptr = weak_brand_ptr(i, &this, "Weak")?;
        let key = arg(a, 0);
        Ok(Value::Bool(weak_delete(i, ptr, &key)))
    });
    let ctor = it.make_native(name, 0, ctor_fn);
    ctor.borrow_mut().props.insert(
        "prototype",
        Property::data(Value::Obj(proto.clone()), false, false, false),
    );
    proto
        .borrow_mut()
        .props
        .insert("constructor", Property::builtin(Value::Obj(ctor.clone())));
    set_to_string_tag(it, &proto, name);
    set_builtin(&it.global, name, Value::Obj(ctor));
}
