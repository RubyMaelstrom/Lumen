//! Observations of the production Rc heap, never a shadow ownership graph.
//! ECMA-262 e28783d5 #sec-weakref-processing-model: records contain integers and
//! static Rust allocation locations only. They cannot extend JavaScript liveness.
//! Feature-gated out of ordinary builds. Lifetimes use allocation ordinals and GC
//! generations, not a clock per allocation; initial families are explicitly labelled.
use super::{Callable, Exotic, GcHeap};
use std::collections::BTreeMap;
use std::panic::Location;

const MAX_SITES: usize = 256;
const MAX_SLOTS: usize = 262_144;
const FAMILIES: [&str; 7] = [
    "ordinary",
    "callable",
    "array",
    "arguments",
    "error",
    "wrapper",
    "side_table",
];

#[derive(Clone, Copy)]
struct Birth {
    site: usize,
    ordinal: u64,
    generation: u64,
}
#[derive(Default)]
struct Site {
    allocations: u64,
    deaths: u64,
    deaths_before_collection: u64,
    requested_named_capacity: u64,
    lifetime_allocations: [u64; 8],
}
#[derive(Default)]
pub(super) struct State {
    sites: BTreeMap<(&'static str, u32, &'static str), usize>,
    counts: Vec<Site>,
    births: Vec<Option<Birth>>,
    generation: u64,
    ordinal: u64,
    untracked_allocations: u64,
}

fn initial_family(exotic: &Exotic) -> &'static str {
    match exotic {
        Exotic::None => "ordinary",
        Exotic::Array => "array",
        Exotic::Arguments => "arguments",
        Exotic::Error(_) => "error",
        _ => "wrapper",
    }
}

impl State {
    pub(super) fn born(
        &mut self,
        slot: usize,
        caller: &'static Location<'static>,
        exotic: &Exotic,
        capacity: usize,
    ) {
        self.ordinal = self.ordinal.wrapping_add(1);
        let key = (caller.file(), caller.line(), initial_family(exotic));
        let index = self.sites.get(&key).copied().or_else(|| {
            if self.sites.len() == MAX_SITES {
                return None;
            }
            let index = self.counts.len();
            self.sites.insert(key, index);
            self.counts.push(Site::default());
            Some(index)
        });
        let Some(site) = index.filter(|_| slot < MAX_SLOTS) else {
            self.untracked_allocations += 1;
            return;
        };
        self.counts[site].allocations += 1;
        self.counts[site].requested_named_capacity += capacity as u64;
        self.births.resize(self.births.len().max(slot + 1), None);
        assert!(
            self.births[slot].is_none(),
            "registry slot is not already alive"
        );
        self.births[slot] = Some(Birth {
            site,
            ordinal: self.ordinal,
            generation: self.generation,
        });
    }

    pub(super) fn died(&mut self, slot: usize) {
        let Some(birth) = self.births.get_mut(slot).and_then(Option::take) else {
            return;
        };
        let age = self.ordinal.wrapping_sub(birth.ordinal);
        let bucket = [0, 1, 16, 256, 4096, 65536, 1_048_576].partition_point(|limit| age > *limit);
        let site = &mut self.counts[birth.site];
        site.deaths += 1;
        site.deaths_before_collection += u64::from(birth.generation == self.generation);
        site.lifetime_allocations[bucket] += 1;
    }

    pub(super) fn finish_generation(&mut self) {
        self.generation += 1;
    }

    fn json(&self, heap_id: u64, live: [u64; 7], borrowed: u64) -> String {
        // Compiler-provided Rust locations contain no user data. Escape file paths for JSON
        // using the same small explicit routine on all hosts (Windows paths include '\\').
        fn quoted(value: &str) -> String {
            let mut output = String::from("\"");
            for ch in value.chars() {
                match ch {
                    '"' => output.push_str("\\\""),
                    '\\' => output.push_str("\\\\"),
                    ch if ch < ' ' => {
                        use std::fmt::Write;
                        let _ = write!(output, "\\u{:04x}", ch as u32);
                    }
                    ch => output.push(ch),
                }
            }
            output.push('"');
            output
        }
        let sites = self.sites.iter().map(|((file,line,family),index)| {
            let site=&self.counts[*index];
            format!("{{\"file\":{},\"line\":{line},\"initial_family\":{},\"allocations\":{},\"deaths\":{},\"deaths_before_completed_collection\":{},\"requested_named_capacity\":{},\"lifetime_allocations\":{:?}}}",
                quoted(file), quoted(family), site.allocations, site.deaths, site.deaths_before_collection, site.requested_named_capacity, site.lifetime_allocations)
        }).collect::<Vec<_>>().join(",");
        let live = FAMILIES
            .iter()
            .zip(live)
            .map(|(name, count)| format!("\"{name}\":{count}"))
            .collect::<Vec<_>>()
            .join(",");
        format!("{{\"heap\":{heap_id},\"allocations\":{},\"collections\":{},\"untracked_allocations\":{},\"borrowed_live_objects\":{borrowed},\"live_families\":{{{live}}},\"lifetime_bucket_upper_allocations\":[0,1,16,256,4096,65536,1048576,null],\"sites\":[{sites}]}}",self.ordinal,self.generation,self.untracked_allocations)
    }
}

pub(super) fn report(heap: &GcHeap) {
    let mut live = [0; 7];
    let mut borrowed = 0;
    let registry = heap.registry.borrow();
    for &object in registry.entries.iter().filter(|object| !object.is_null()) {
        // SAFETY: a non-null registry entry names a live object; `Object::drop` clears it first.
        let object = unsafe { &*object };
        let Ok(object) = object.try_borrow() else {
            borrowed += 1;
            continue;
        };
        let family = if !object.ic_plain.get() {
            6
        } else if !matches!(object.call, Callable::None) {
            1
        } else {
            match object.exotic {
                Exotic::None => 0,
                Exotic::Array => 2,
                Exotic::Arguments => 3,
                Exotic::Error(_) => 4,
                _ => 5,
            }
        };
        live[family] += 1;
    }
    eprintln!(
        "[allocation-diagnostic] {}",
        heap.diagnostics.borrow().json(heap.heap_id, live, borrowed)
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn slot_reuse_and_collection_age_do_not_retain_objects() {
        let mut state = State::default();
        state.born(0, Location::caller(), &Exotic::None, 4);
        state.died(0);
        state.born(0, Location::caller(), &Exotic::Array, 0);
        state.finish_generation();
        state.died(0);
        assert_eq!(state.counts.iter().map(|s| s.allocations).sum::<u64>(), 2);
        assert_eq!(state.counts.iter().map(|s| s.deaths).sum::<u64>(), 2);
        assert_eq!(
            state
                .counts
                .iter()
                .map(|s| s.deaths_before_collection)
                .sum::<u64>(),
            1
        );
        assert!(state.births.iter().all(Option::is_none));
        state.born(MAX_SLOTS, Location::caller(), &Exotic::None, 0);
        assert_eq!(state.untracked_allocations, 1);
    }
}
