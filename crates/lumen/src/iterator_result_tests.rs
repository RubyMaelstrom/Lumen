//! ECMA-262 e28783d5: IteratorStepValue, IteratorClose, CreateIteratorResultObject,
//! %ArrayIteratorPrototype%.next and String/Map/Set iterator state transitions.
use crate::{bytecode::Tier, value::Value, Completion, Engine};

fn eval(engine: &mut Engine, source: &str) -> String {
    match engine.eval(source, false).expect("iterator fixture parses") {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}\n{source}"),
    }
}

fn global(engine: &mut Engine, name: &str) -> Value {
    let global = Value::Obj(engine.interp.global.clone());
    engine
        .interp
        .get_member(&global, name)
        .unwrap_or_else(|_| panic!("global {name}"))
}

#[test]
fn iterator_results_remove_wrapper_allocations_but_public_next_keeps_identity() {
    for expression in [
        "[1,2,3].values()",
        "'abc'[Symbol.iterator]()",
        "new Map([[1,10],[2,20],[3,30]]).values()",
        "new Set([1,2,3]).values()",
    ] {
        let mut engine = Engine::new();
        eval(&mut engine, &format!("var iterator={expression};"));
        let iterator = global(&mut engine, "iterator");
        let next = engine
            .interp
            .get_member(&iterator, "next")
            .unwrap_or_else(|_| panic!("next"));
        let before = crate::value::heap_allocated_objects(&engine.interp.gc_heap);
        for _ in 0..3 {
            assert!(engine
                .interp
                .iterator_step(&iterator, &next)
                .unwrap_or_else(|_| panic!("iterator step"))
                .is_some());
        }
        assert!(engine
            .interp
            .iterator_step(&iterator, &next)
            .unwrap_or_else(|_| panic!("iterator exhaustion"))
            .is_none());
        let allocated = crate::value::heap_allocated_objects(&engine.interp.gc_heap) - before;
        assert_eq!(
            allocated,
            if std::env::var("LUMEN_ITERATOR_RESULTS").as_deref() == Ok("0") {
                4
            } else {
                0
            },
            "{expression}"
        );
        // Exhausted calls must still return fresh, ordinary observable objects.
        let first = engine
            .interp
            .call(next.clone(), iterator.clone(), &[])
            .unwrap_or_else(|_| panic!("public next"));
        let second = engine
            .interp
            .call(next, iterator, &[])
            .unwrap_or_else(|_| panic!("public next"));
        assert!(!std::rc::Rc::ptr_eq(
            first.as_obj().unwrap(),
            second.as_obj().unwrap()
        ));
        assert_eq!(
            crate::value::heap_allocated_objects(&engine.interp.gc_heap) - before,
            allocated + 2
        );
    }
}

#[test]
fn iterator_results_preserve_reentrancy_captured_next_close_and_custom_results() {
    let source = r#"
    (function(){
        var trace=[],a=[1,2,3],it=a.values();
        Object.defineProperty(a,'0',{get:function(){trace.push(it.next().value);return 10}});
        var iterable={[Symbol.iterator]:function(){return it}};
        var proto=Object.getPrototypeOf(it), saved=proto.next;
        proto.return=function(){trace.push(this===it);return {done:true}};
        for(var value of iterable){
            trace.push(value);
            proto.next=function(){throw 'must use captured next'};
            break;
        }
        proto.next=saved;delete proto.return;
        trace.push(it.next().value,it.next().done);
        var custom={ [Symbol.iterator]:function(){return this}, next:function(){
            return {get done(){trace.push('done');return false},get value(){trace.push('value');return 7}};
        },return:function(){trace.push('closed');return {}}};
        for(var x of custom){trace.push(x);break}
        var hooked=new Proxy(saved,{apply:function(target,receiver,args){trace.push('apply');return {value:5,done:true}}});
        var obj={[Symbol.iterator]:function(){return {next:hooked}}};
        for(var ignored of obj){throw 'done was ignored'}
        var keys=[0];Object.defineProperty(keys,'0',{get:function(){throw 'keys read element'}});
        trace.push(keys.keys().next().value);
        return trace.join('|');
    })()
    "#;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            eval(&mut engine, source),
            "2|10|true|3|true|done|value|7|closed|apply|0",
            "{tier:?}"
        );
    }
}

#[test]
fn iterator_results_keep_unicode_live_collections_and_growth() {
    let source = r#"
    (function(){
        var chars=[]; for(var ch of 'A\uD83D\uDE00\uD800Z')chars.push(ch.length);
        var a=[1],seen=[];for(var x of a){seen.push(x);if(x===1)a.push(2)}
        var m=new Map([[1,10],[2,20]]),pairs=[];
        for(var pair of m){pairs.push(pair.join(':'));if(pair[0]===1){m.delete(2);m.set(3,30)}}
        var s=new Set([1,2]),set=[];for(var x of s){set.push(x);if(x===1){s.delete(2);s.add(3)}}
        var it=a.values();it.next();it.next();var done=it.next().done;a.push(4);
        return [chars.join(','),seen.join(','),pairs.join(','),set.join(','),done,it.next().done].join('|');
    })()
    "#;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            eval(&mut engine, source),
            "1,2,1,1|1,2|1:10,3:30|1,3|true|true",
            "{tier:?}"
        );
    }
}

#[test]
fn iterator_results_builtin_consumers_preserve_mapping_close_and_getter_order() {
    let source = r#"
    (function(){
        var trace=[],it=[1,2,3].values();
        it.return=function(){trace.push(this===it?'close':'wrong receiver');return {}};
        try { Array.from(it,function(v,k){
            trace.push(v+':'+k);
            it.next=function(){throw 'next was re-read'};
            if(k===1)throw 19;
            return v;
        }); } catch(e) { trace.push(e); }
        var s=new Set([2,4,6]);
        trace.push(Array.from(s.values(),function(v){return v+1}).join(','));
        trace.push(s.values().reduce(function(a,v){return a+v},0));
        var helper=[7,8].values();
        helper.return=function(){trace.push('helper close');return {}};
        trace.push(helper.some(function(v){return v===7}));
        var calls=0,custom={ [Symbol.iterator]:function(){return this},get next(){
            trace.push('next');return function(){
                var done=calls++>0;
                return {get done(){trace.push('done'+done);return done},
                    get value(){if(done)throw 'exhausted value read';trace.push('value');return 11}};
            };
        }};
        trace.push(Array.from(custom).join(','));
        return trace.join('|');
    })()
    "#;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            eval(&mut engine, source),
            "1:0|2:1|close|19|3,5,7|12|helper close|true|next|donefalse|value|donetrue|11",
            "{tier:?}"
        );
    }
}

#[test]
fn iterator_results_keep_realm_errors_and_new_target_after_nested_calls() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            eval(
                &mut engine,
                r#"
        (function(){
            var other=$262.createRealm().global;
            var next=other.Array.prototype.values.call([]).next;
            var iterable={[Symbol.iterator]:function(){return {next:next}}};
            var realm=false;try{for(var x of iterable){}}catch(e){realm=e instanceof other.TypeError}
            function C(){
                var a=[1],seen=false;
                Object.defineProperty(a,'0',{get:function(){seen=new.target===undefined;return 9}});
                for(var x of a){this.v=x}
                this.valid=seen && new.target===C;
            }
            var c=new C;return [realm,c.v,c.valid].join('|');
        })()
        "#
            ),
            "true|9|true",
            "{tier:?}"
        );
    }
}

#[test]
fn iterator_results_private_slots_survive_freeze_and_reject_forged_brands() {
    let source = r#"
    (function(){
        var trace=[],a=[3,5].values(),s='A\uD83D\uDE00'[Symbol.iterator]();
        trace.push(Reflect.ownKeys(a).length,Reflect.ownKeys(s).length);
        a.__ai_index=100;a.__ai_target=[];a.__ai_kind=1;
        s.__si_index=100;s.__si_str='wrong';
        Object.freeze(a);Object.freeze(s);
        trace.push(a.next().value,a.next().value,a.next().done);
        trace.push(s.next().value,s.next().value.length,s.next().done);
        var anext=[].values().next,snext=''[Symbol.iterator]().next;
        for(var pair of [
            [anext,{__ai_target:[],__ai_index:0,__ai_kind:0}],
            [snext,{__si_str:'forged',__si_index:0}],
            [anext,new Proxy([].values(),{})],
            [snext,new Proxy(''[Symbol.iterator](),{})],
            [snext,TypeError.prototype]
        ]) {try{pair[0].call(pair[1]);trace.push(false)}catch(e){trace.push(e instanceof TypeError)}}
        return trace.join('|');
    })()
    "#;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            eval(&mut engine, source),
            "0|0|3|5|true|A|2|true|true|true|true|true|true",
            "{tier:?}"
        );
    }
}

#[test]
fn iterator_results_private_target_is_traced_and_collectable() {
    for budget in [0, usize::MAX] {
        let mut engine = Engine::new();
        eval(&mut engine, "var target=[{answer:42}];target.push(target);var iterator=target.values();target=null;");
        let iterator = global(&mut engine, "iterator");
        let target = {
            let object = iterator.as_obj().unwrap().borrow();
            let crate::value::Exotic::ArrayIterator(state) = &object.exotic else {
                panic!("private iterator slots")
            };
            std::rc::Rc::downgrade(state.target.as_obj().unwrap())
        };
        engine
            .interp
            .gc_collect_with_edge_budget(crate::value::GcCause::Explicit, budget);
        assert!(target.upgrade().is_some());
        assert_eq!(eval(&mut engine, "iterator.next().value.answer"), "42");
        eval(&mut engine, "iterator=null;");
        drop(iterator);
        engine
            .interp
            .gc_collect_with_edge_budget(crate::value::GcCause::Explicit, budget);
        assert!(
            target.upgrade().is_none(),
            "unreachable cycle retained, budget={budget}"
        );
    }
}
