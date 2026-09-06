//! Bounded, collection-local strong-edge snapshots. No graph mutation or JS runs between
//! reference counting and marking. Reusing those edges avoids decoding every live object's
//! properties and interpreter side tables twice. These are temporary collector owners, not
//! roots: the counting pass must account for BOTH the real edge and its cached Rc clone.

use crate::fasthash::FastMap;
use crate::interpreter::Env;
use crate::value::Gc;
use std::mem::size_of;
use std::rc::Rc;

pub(crate) const EDGE_CACHE_BYTES: usize = 32 * 1024 * 1024;

#[derive(Clone, Copy)]
struct Edges {
    object_start: u32,
    object_len: u32,
    scope_start: u32,
    scope_len: u32,
}

impl Edges {
    const UNCACHED: Self = Self {
        object_start: 0,
        object_len: u32::MAX,
        scope_start: 0,
        scope_len: 0,
    };
}

pub(crate) struct GcEdgeCache {
    entries: Vec<Edges>,
    objects: Vec<Gc>,
    scopes: Vec<(usize, Env)>,
    budget: usize,
    enabled: bool,
}

impl GcEdgeCache {
    pub(crate) fn new(object_count: usize, budget: usize) -> Self {
        let enabled = object_count < u32::MAX as usize
            && object_count.saturating_mul(size_of::<Edges>()) <= budget
            && budget != 0;
        Self {
            entries: Vec::with_capacity(if enabled { object_count } else { 0 }),
            objects: Vec::new(),
            scopes: Vec::new(),
            budget,
            enabled,
        }
    }

    /// Reserve geometrically, but cap requested backing capacities (not just populated bytes).
    /// A large graph can partly use the cache; uncached owners keep the original tracing path.
    pub(crate) fn prepare(&mut self, objects: usize, scopes: usize) -> bool {
        if !self.enabled {
            return false;
        }
        let capacity = |len: usize, cap: usize, extra: usize| {
            let need = len.saturating_add(extra);
            if need > cap {
                need.max(cap.saturating_mul(2)).max(4)
            } else {
                cap
            }
        };
        let object_cap = capacity(self.objects.len(), self.objects.capacity(), objects);
        let scope_cap = capacity(self.scopes.len(), self.scopes.capacity(), scopes);
        let bytes = self
            .entries
            .capacity()
            .saturating_mul(size_of::<Edges>())
            .saturating_add(object_cap.saturating_mul(size_of::<Gc>()))
            .saturating_add(scope_cap.saturating_mul(size_of::<(usize, Env)>()));
        if bytes > self.budget || object_cap >= u32::MAX as usize || scope_cap >= u32::MAX as usize
        {
            return false;
        }
        if object_cap > self.objects.capacity() {
            self.objects.reserve_exact(object_cap - self.objects.len());
        }
        if scope_cap > self.scopes.capacity() {
            self.scopes.reserve_exact(scope_cap - self.scopes.len());
        }
        true
    }

    /// Move the scratch handles, never clone them again. Missing scope indices are foreign to
    /// this snapshot and were ignored by the original collector too.
    pub(crate) fn record(
        &mut self,
        cached: bool,
        objects: &mut Vec<Gc>,
        scopes: &mut Vec<Env>,
        scope_indices: &FastMap<usize, usize>,
    ) {
        if cached {
            let object_start = self.objects.len();
            let scope_start = self.scopes.len();
            self.objects.append(objects);
            self.scopes.extend(scopes.drain(..).filter_map(|env| {
                scope_indices
                    .get(&(Rc::as_ptr(&env) as usize))
                    .map(|&index| (index, env))
            }));
            self.entries.push(Edges {
                object_start: object_start as u32,
                object_len: (self.objects.len() - object_start) as u32,
                scope_start: scope_start as u32,
                scope_len: (self.scopes.len() - scope_start) as u32,
            });
        } else {
            objects.clear();
            scopes.clear();
            if self.enabled {
                self.entries.push(Edges::UNCACHED);
            }
        }
    }

    pub(crate) fn get(&self, index: usize) -> Option<(&[Gc], &[(usize, Env)])> {
        let entry = self.entries.get(index)?;
        if entry.object_len == u32::MAX {
            return None;
        }
        let object_start = entry.object_start as usize;
        let scope_start = entry.scope_start as usize;
        Some((
            &self.objects[object_start..object_start + entry.object_len as usize],
            &self.scopes[scope_start..scope_start + entry.scope_len as usize],
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bytecode::Tier;
    use crate::value::Object;
    use crate::{Completion, Engine};

    fn value(engine: &mut Engine, source: &str) -> String {
        match engine.eval(source, false).expect("GC fixture parses") {
            Completion::Value(value) => value,
            Completion::Throw { name, message } => panic!("{name}: {message}"),
        }
    }

    #[test]
    fn cached_and_uncached_collectors_agree_on_strong_and_weak_graphs() {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            // No cache, metadata plus a handful of edges (mixed fallback), and normal budget.
            for mode in 0..3 {
                let mut engine = Engine::new();
                engine.set_tier(tier);
                engine.set_tier_threshold(0);
                value(
                    &mut engine,
                    r#"
                    var keep, weak, dead, wm = new WeakMap();
                    (() => {
                        const target = {n: 41};
                        const buffer = new ArrayBuffer(8);
                        const typed = new Uint8Array(buffer); typed[0] = 17;
                        function closure() { return target.n + 1; }
                        function args(x) { return [arguments, () => x]; }
                        class Holder { value = target; read() { return this.value.n; } }
                        const accessor = {get value() { return target; }};
                        const proxy = new Proxy(target, {get(t,k) { return t[k]; }});
                        keep = {closure, bound: closure.bind(target), accessor, proxy,
                            args: args(target), map: new Map([[target, {target}]]),
                            set: new Set([target]), typed, view: new DataView(buffer),
                            holder: new Holder(), promise: Promise.resolve(target)};
                        keep.self = keep;
                        weak = new WeakRef(target);
                    })();
                    // A separate scope is essential: retained closures conservatively retain
                    // their whole environment, including otherwise unused local bindings.
                    (() => {
                        const key = {}, cyclic = {key}; key.self = key;
                        wm.set(key, cyclic);
                        dead = [new WeakRef(key), new WeakRef(cyclic)];
                    })();
                    'ready'
                "#,
                );
                let budget = match mode {
                    0 => 0,
                    1 => {
                        crate::value::heap_gc_snapshot(&engine.interp.gc_heap).len()
                            * size_of::<Edges>()
                            + 512
                    }
                    _ => EDGE_CACHE_BYTES,
                };
                for _ in 0..3 {
                    engine
                        .interp
                        .gc_collect_with_edge_budget(crate::value::GcCause::Explicit, budget);
                    assert_eq!(
                        value(
                            &mut engine,
                            r#"
                        [keep.closure(), keep.bound(), keep.accessor.value.n, keep.proxy.n,
                         keep.args[0][0].n, keep.args[1]().n,
                         keep.map.get(weak.deref()).target.n, keep.set.has(weak.deref()),
                         keep.typed[0], keep.view.getUint8(0), keep.holder.read(),
                         dead.every(ref => ref.deref() === undefined)].join(',')
                    "#
                        ),
                        "42,42,41,41,41,41,41,true,17,17,41,true",
                        "{tier:?}, cache mode {mode}"
                    );
                }
                assert_eq!(
                    value(
                        &mut engine,
                        "var promiseValue; keep.promise.then(x => promiseValue = x.n); 'queued'"
                    ),
                    "queued"
                );
                assert_eq!(value(&mut engine, "promiseValue"), "41");
                value(&mut engine, "keep = null; 'released'");
                engine
                    .interp
                    .gc_collect_with_edge_budget(crate::value::GcCause::Explicit, budget);
                assert_eq!(
                    value(&mut engine, "weak.deref() === undefined"),
                    "true",
                    "{tier:?}, cache mode {mode}"
                );
                // Allocation/drop after every collection also exercises restored registry slots.
                value(
                    &mut engine,
                    "for (var i=0; i<200; i++) { const x={}; x.self=x; } 'done'",
                );
                engine
                    .interp
                    .gc_collect_with_edge_budget(crate::value::GcCause::Explicit, budget);
            }
        }
    }

    #[test]
    fn cache_does_not_root_finalization_targets_or_truncate_ephemeron_chains() {
        for budget in [0, EDGE_CACHE_BYTES] {
            let mut engine = Engine::new();
            value(
                &mut engine,
                r#"
                var cleaned = [], heldWeak, registry = new FinalizationRegistry(x => cleaned.push(x.tag));
                (() => {
                    const target = {}, held = {tag: 'held'};
                    target.self = target; heldWeak = new WeakRef(held);
                    registry.register(target, held, target);
                })();
                var chain = new WeakMap(), head = {}, cursor = head;
                for (var i=0; i<2000; i++) { const next={}; chain.set(cursor,next); cursor=next; }
                cursor = null;
                'ready'
            "#,
            );
            engine
                .interp
                .gc_collect_with_edge_budget(crate::value::GcCause::Explicit, budget);
            // Collection queues cleanup but must not run it synchronously.
            assert_eq!(value(&mut engine, "var before = cleaned.length; var count=0; for(var p=head; p; p=chain.get(p)) count++; before + ':' + count"), "0:2001");
            assert_eq!(value(&mut engine, "cleaned.join(',')"), "held");
            engine
                .interp
                .gc_collect_with_edge_budget(crate::value::GcCause::Explicit, budget);
            assert_eq!(value(&mut engine, "heldWeak.deref() === undefined"), "true");
        }
    }

    #[test]
    fn cache_moves_handles_and_falls_back_within_its_capacity_budget() {
        let object = Object::new(None);
        let env = crate::interpreter::new_scope(None);
        let indices = [(Rc::as_ptr(&env) as usize, 0)].into_iter().collect();
        let mut cache = GcEdgeCache::new(3, 160);
        let mut objects = vec![object.clone()];
        let mut scopes = vec![env.clone()];
        let count = Rc::strong_count(&object);
        assert!(cache.prepare(objects.len(), scopes.len()));
        cache.record(true, &mut objects, &mut scopes, &indices);
        assert!(objects.is_empty() && scopes.is_empty());
        assert_eq!(Rc::strong_count(&object), count);
        assert!(Rc::ptr_eq(&cache.get(0).unwrap().0[0], &object));
        assert_eq!(cache.get(0).unwrap().1[0].0, 0);
        objects.extend((0..20).map(|_| object.clone()));
        assert!(!cache.prepare(objects.len(), 0));
        cache.record(false, &mut objects, &mut scopes, &indices);
        assert!(cache.get(1).is_none());
        assert_eq!(Rc::strong_count(&object), count);
        assert!(cache.prepare(0, 0));
        cache.record(true, &mut objects, &mut scopes, &indices);
        assert!(cache.get(2).unwrap().0.is_empty());
        drop(cache);
        assert_eq!(Rc::strong_count(&object), count - 1);
    }

    #[test]
    fn zero_budget_never_retains_edges() {
        let object = Object::new(None);
        let mut cache = GcEdgeCache::new(1, 0);
        let mut objects = vec![object.clone()];
        let mut scopes = Vec::new();
        assert!(!cache.prepare(1, 0));
        cache.record(false, &mut objects, &mut scopes, &FastMap::default());
        assert!(cache.get(0).is_none());
        assert_eq!(Rc::strong_count(&object), 1);
    }
}
