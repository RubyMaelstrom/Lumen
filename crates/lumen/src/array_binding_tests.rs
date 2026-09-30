use crate::{bytecode::Tier, Completion, Engine};

fn eval(engine: &mut Engine, source: &str) -> String {
    match engine
        .eval(source, false)
        .expect("array binding fixture parses")
    {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}\n{source}"),
    }
}

#[test]
fn dense_array_bindings_retain_captured_values_and_use_the_closed_prefix() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let before = super::array_binding::TEST_ARRAY_BINDING_OPENS.with(std::cell::Cell::get);
        #[cfg(all(
            any(target_arch = "aarch64", target_arch = "x86_64"),
            any(target_os = "linux", target_os = "macos", target_os = "windows")
        ))]
        let native_before =
            super::array_binding::TEST_JIT_ARRAY_BINDING_OPENS.with(std::cell::Cell::get);
        assert_eq!(
            eval(
                &mut engine,
                r#"
            function bind(records){
                let saved=[];
                for(const [key,value] of records) saved.push(()=>[key,value]);
                return saved;
            }
            var values=[['a',1],['b',{id:7}],['c',undefined]],saved=[];
            for(var n=0;n<120;n++) saved=bind(values);
            var correct=saved[0]()[0]==='a'&&saved[1]()[1]===values[1][1]&&saved[2]()[1]===undefined;
            values=null;
            correct
        "#
            ),
            "true",
            "{tier:?}"
        );
        let opens =
            super::array_binding::TEST_ARRAY_BINDING_OPENS.with(std::cell::Cell::get) - before;
        if tier != Tier::Interp {
            assert!(
                opens >= 300,
                "compiled fresh bindings must enter the path: {tier:?}, {opens}"
            );
        }
        #[cfg(all(
            any(target_arch = "aarch64", target_arch = "x86_64"),
            any(target_os = "linux", target_os = "macos", target_os = "windows")
        ))]
        if tier == Tier::Jit {
            let native = super::array_binding::TEST_JIT_ARRAY_BINDING_OPENS
                .with(std::cell::Cell::get)
                - native_before;
            assert!(
                native >= 300,
                "native entries must use the closed prefix: {native}"
            );
        }
        engine.interp.gc_collect();
        assert_eq!(
            eval(&mut engine, "saved[1]()[1].id+':'+saved[0]()[0]"),
            "7:a"
        );
    }
}

#[test]
fn array_bindings_keep_custom_next_closing_and_index_getter_order() {
    let source = r#"
        function bind(records){let out=[];for(const [key,value] of records)out.push(()=>key+':'+value);return out.map(f=>f()).join(',')}
        var a=[1,2,3],proto=Object.getPrototypeOf(a.values()),original=proto.next,trace='';
        Object.defineProperty(proto,'return',{configurable:true,get(){trace+='r';return function(){trace+='c'+this.next().value;return {}}}});
        var close=bind([a]);delete proto.return;
        Object.defineProperty(a,'0',{configurable:true,get(){trace+='g';a[1]=9;return 5}});
        var getter=bind([a]);
        proto.next=function(){var result=original.call(this);trace+='n';return result};
        var next=bind([[7,8]]);proto.next=original;
        var custom=bind([{[Symbol.iterator](){let n=0;return {next(){return {value:++n,done:false}},return(){trace+='x';return {}}}}}]);
        [close,getter,next,custom,trace].join('|')
    "#;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            eval(&mut engine, source),
            "1:2|5:9|7:8|1:2|rc3gnnnnx",
            "{tier:?}"
        );
    }
}

#[test]
fn array_bindings_preserve_holes_exhaustion_defaults_and_observable_initializations() {
    let source = r#"
        function bind(records){let out=[];for(const [key,value] of records)out.push(()=>[key,value]);return out}
        var a=[,2],proto=Object.getPrototypeOf(a),reads=0;
        Object.defineProperty(proto,'0',{configurable:true,get(){reads++;return 11},set(v){Object.defineProperty(this,'0',{value:v,writable:true,configurable:true,enumerable:true})}});
        var hole=bind([a]);delete proto[0];
        var short=bind([[4]]),empty=bind([[]]),nil=bind([[undefined,null]]);
        var defaults=0;function defaultBind(record){let [a=(defaults++,9),b=10]=record;return ()=>a+b}
        var d=defaultBind([undefined]);
        var observe=[];
        function observeBind(){
            function read(){return a}
            var custom={[Symbol.iterator](){let index=0;return {next(){if(index++)observe.push(read());return {value:index,done:false}},return(){return {}}}}};
            const [a,b]=custom;return ()=>b
        }
        var observed=observeBind()();
        [hole[0]()[0],reads,short[0]()[0],short[0]()[1]===undefined,empty[0]()[0]===undefined,nil[0]()[1]===null,d(),defaults,observed,observe.join(',')].join('|')
    "#;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            eval(&mut engine, source),
            "11|1|4|true|true|true|19|1|2|1",
            "{tier:?}"
        );
    }
}

#[test]
fn array_binding_declines_due_gc_and_final_old_slot_owners_without_effects() {
    let mut engine = Engine::new();
    eval(&mut engine, "var source=[1,2],old={};");
    let global = crate::value::Value::Obj(engine.interp.global.clone());
    let source = engine
        .interp
        .get_member(&global, "source")
        .unwrap_or_else(|_| panic!("source"));
    let old = engine
        .interp
        .get_member(&global, "old")
        .unwrap_or_else(|_| panic!("old"));
    eval(&mut engine, "old=null;");
    // The temporary plus one simulated slot are the only owners.
    let slot = old.clone();
    assert!(!crate::interpreter::Interp::array_binding_old_slot_is_inert(&old));
    let prior = engine.interp.gc_tick;
    engine.interp.gc_tick = 253;
    assert!(engine
        .interp
        .try_array_binding(
            &source,
            2,
            &crate::value::Value::Undefined,
            &crate::value::Value::Undefined
        )
        .unwrap_or_else(|_| panic!("due boundary decline"))
        .is_none());
    assert_eq!(engine.interp.gc_tick, 253);
    engine.interp.gc_tick = 1;
    assert!(engine
        .interp
        .try_array_binding(&source, 2, &old, &crate::value::Value::Undefined)
        .unwrap_or_else(|_| panic!("owner decline"))
        .is_none());
    assert_eq!(engine.interp.gc_tick, 1);
    let alias_slot = old.clone();
    let alias_temporary = old.clone();
    assert!(crate::interpreter::Interp::array_binding_old_slot_is_inert(
        &old
    ));
    assert!(engine
        .interp
        .try_array_binding(&source, 2, &old, &alias_temporary)
        .unwrap_or_else(|_| panic!("joint owner decline"))
        .is_none());
    assert_eq!(engine.interp.gc_tick, 1);
    drop(alias_slot);
    drop(alias_temporary);
    let prototype = crate::value::Object::new(None);
    let iterator = crate::value::Object::new_with_parts(
        Some(prototype.clone()),
        crate::value::Props::new(),
        crate::value::Exotic::ArrayIterator(Box::new(crate::value::ArrayIteratorState {
            target: source.clone(),
            index: 0,
            kind: 0,
        })),
    );
    let iterator = crate::value::Value::Obj(iterator);
    let iterator_slot = iterator.clone();
    let captured = crate::value::Value::Obj(prototype);
    let captured_slot = captured.clone();
    assert!(crate::interpreter::Interp::array_binding_old_slot_is_inert(
        &iterator
    ));
    assert!(crate::interpreter::Interp::array_binding_old_slot_is_inert(
        &captured
    ));
    assert!(engine
        .interp
        .try_array_binding(&source, 2, &iterator, &captured)
        .unwrap_or_else(|_| panic!("nested joint owner decline"))
        .is_none());
    assert_eq!(engine.interp.gc_tick, 1);
    drop(iterator_slot);
    drop(captured_slot);
    drop(iterator);
    drop(captured);
    let target = crate::value::Object::new(Some(engine.interp.object_proto.clone()));
    let aliased = crate::value::Object::new_with_parts(
        Some(target.clone()),
        crate::value::Props::new(),
        crate::value::Exotic::ArrayIterator(Box::new(crate::value::ArrayIteratorState {
            target: crate::value::Value::Obj(target),
            index: 0,
            kind: 0,
        })),
    );
    let aliased = crate::value::Value::Obj(aliased);
    let aliased_slot = aliased.clone();
    assert!(
        !crate::interpreter::Interp::array_binding_old_slot_is_inert(&aliased),
        "target and prototype owners can both be retired by one shell"
    );
    let target = aliased.as_obj().unwrap().borrow().proto.clone().unwrap();
    let captured = crate::value::Value::Obj(target);
    let captured_slot = captured.clone();
    assert!(crate::interpreter::Interp::array_binding_old_slot_is_inert(
        &aliased
    ));
    assert!(engine
        .interp
        .try_array_binding(&source, 2, &aliased, &captured)
        .unwrap_or_else(|_| panic!("doubled nested owner decline"))
        .is_none());
    assert_eq!(engine.interp.gc_tick, 1);
    drop(aliased_slot);
    drop(captured_slot);
    drop(aliased);
    drop(captured);
    let allocated = crate::value::heap_allocated_objects(&engine.interp.gc_heap);
    let next = engine
        .interp
        .try_array_binding(
            &source,
            2,
            &crate::value::Value::Undefined,
            &crate::value::Value::Undefined,
        )
        .unwrap_or_else(|_| panic!("closed opening"))
        .expect("closed prefix");
    for index in 0..2 {
        let value = engine
            .interp
            .array_binding_step(&source, &next, index)
            .unwrap_or_else(|_| panic!("closed step"));
        assert!(matches!(value,crate::value::Value::Num(n) if n == (index + 1) as f64));
    }
    assert_eq!(
        engine.interp.gc_tick, 4,
        "factory and both next calls retain GC ticks"
    );
    assert_eq!(
        crate::value::heap_allocated_objects(&engine.interp.gc_heap),
        allocated,
        "no iterator shell or result object"
    );
    drop(slot);
    engine.interp.gc_tick = prior;
}

#[test]
fn array_binding_slot_replacement_runs_final_host_owners_before_the_next_read() {
    use crate::value::{Gc, Property, Value};
    use std::{cell::Cell, rc::Rc};
    struct Witness {
        source: Gc,
        drops: Rc<Cell<usize>>,
    }
    impl Drop for Witness {
        fn drop(&mut self) {
            self.drops.set(self.drops.get() + 1);
            self.source
                .borrow_mut()
                .props
                .insert("0", Property::data(Value::Num(23.0), true, true, true));
        }
    }
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        eval(&mut engine, "var nextPair=[3,4];");
        let global = Value::Obj(engine.interp.global.clone());
        let source = engine
            .interp
            .get_member(&global, "nextPair")
            .unwrap_or_else(|_| panic!("next pair"));
        let source = source.as_obj().unwrap().clone();
        let drops = Rc::new(Cell::new(0usize));
        let count = drops.clone();
        let maker = engine.interp.make_native_closure(
            "makeBindingWitness",
            0,
            Rc::new(move |i, _, _| {
                let witness = Witness {
                    source: source.clone(),
                    drops: count.clone(),
                };
                let object = i.make_native_closure(
                    "bindingWitness",
                    0,
                    Rc::new(move |_, _, _| {
                        let _ = &witness;
                        Ok(Value::Undefined)
                    }),
                );
                Ok(Value::Obj(object))
            }),
        );
        engine.interp.global.borrow_mut().props.insert(
            "makeBindingWitness",
            Property::data(Value::Obj(maker), true, true, true),
        );
        assert_eq!(
            eval(
                &mut engine,
                r#"
            function protoBindingSource(){
                var pair=[1,2,makeBindingWitness()],iterator=pair.values();
                pair.next=iterator.next;Object.setPrototypeOf(iterator,pair);
                return iterator;
            }
            function bind(prototypeTarget){
                let saved=[],n=0;
                const records={[Symbol.iterator](){return {next(){
                    if(n++===0)return {value:prototypeTarget?{[Symbol.iterator]:protoBindingSource}:[1,2,makeBindingWitness()],done:false};
                    if(n===2)return {value:nextPair,done:false};
                    return {done:true};
                }}}};
                for(const [key,value] of records)saved.push(()=>key+':'+value);
                return saved.map(f=>f()).join(',');
            }
            var direct=bind(false);nextPair[0]=3;
            direct+'|'+bind(true)
        "#
            ),
            "1:2,23:4|1:2,23:4",
            "{tier:?}"
        );
        assert_eq!(drops.get(), 2, "{tier:?}");
    }
}
