//! Ignored release-mode measurements of the raw object allocation/destruction cost, isolated from
//! JavaScript dispatch. Run with
//! `cargo test --release -p lumen --lib object_alloc_bench -- --ignored --nocapture`.

use crate::value::{Exotic, Object, Property, Props, Value};
use std::rc::Rc;

fn time_per_op(label: &str, iterations: u32, mut op: impl FnMut(u32)) {
    for i in 0..iterations / 10 {
        op(i);
    }
    let started = std::time::Instant::now();
    for i in 0..iterations {
        op(i);
    }
    let nanos = started.elapsed().as_nanos() as f64 / f64::from(iterations);
    println!("{label:40} {nanos:8.1} ns/op");
}

#[test]
#[ignore]
fn object_alloc_bench() {
    let heap = crate::value::new_gc_heap();
    let symbols = crate::value::new_symbol_agent();
    let _active = crate::value::enter_agent(&heap, &symbols);
    let proto = Object::new(None);
    const N: u32 = 5_000_000;

    time_per_op("Object::new(proto) + drop", N, |_| {
        std::hint::black_box(Object::new(Some(proto.clone())));
    });

    let mut template = Props::new();
    template.insert(Rc::from("x"), Property::plain(Value::Undefined));
    template.insert(Rc::from("y"), Property::plain(Value::Undefined));
    time_per_op("templated {x,y} + drop", N, |i| {
        let props = template.instantiate_plain_packed(
            [
                crate::value::PackedValue::pack(Value::Num(f64::from(i))),
                crate::value::PackedValue::pack(Value::Num(1.0)),
            ]
            .into_iter(),
        );
        std::hint::black_box(Object::new_with_parts(
            Some(proto.clone()),
            props,
            Exotic::None,
        ));
    });

    let mut kept = Vec::with_capacity(N as usize);
    time_per_op("Object::new(proto) retained", N, |_| {
        kept.push(Object::new(Some(proto.clone())));
    });
    let started = std::time::Instant::now();
    drop(kept);
    println!(
        "{:40} {:8.1} ns/op",
        "drop retained",
        started.elapsed().as_nanos() as f64 / f64::from(N)
    );

    time_per_op("Rc::new(RefCell<[u8;136]>) + drop", N, |_| {
        std::hint::black_box(Rc::new(std::cell::RefCell::new([0u8; 136])));
    });
    time_per_op("Rc::downgrade + drop weak", N, |_| {
        std::hint::black_box(Rc::downgrade(&proto));
    });
    #[cfg(not(target_arch = "wasm32"))]
    time_per_op("stacker::remaining_stack()", N, |_| {
        std::hint::black_box(stacker::remaining_stack());
    });
    time_per_op("registry reserve+publish+remove", N, |_| {
        crate::value::bench_registry_cycle(&heap, proto.as_ptr());
    });
    time_per_op("gc_heap Rc clone+drop", N, |_| {
        std::hint::black_box(heap.clone());
    });
    time_per_op("Props::new() + drop", N, |_| {
        std::hint::black_box(Props::new());
    });
    time_per_op("active_gc_heap()", N, |_| {
        std::hint::black_box(crate::value::active_gc_heap());
    });
}

#[test]
#[ignore]
fn object_layout_sizes() {
    use crate::value::*;
    println!(
        "Object {} Props {} Callable {} Exotic {} Property {} RefCell<Object> {}",
        std::mem::size_of::<Object>(),
        std::mem::size_of::<Props>(),
        std::mem::size_of::<Callable>(),
        std::mem::size_of::<Exotic>(),
        std::mem::size_of::<Property>(),
        std::mem::size_of::<std::cell::RefCell<Object>>()
    );
}
