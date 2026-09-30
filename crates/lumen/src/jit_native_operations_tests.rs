//! ECMA-262 e28783d5, Declarative Environment Records, GetValue/PutValue,
//! OrdinaryGet and EvaluateCall. Region publication must precede observations.
use super::*;
use crate::{bytecode::Tier, Completion, Engine};

fn evaluate(engine: &mut Engine, source: &str) -> String {
    match engine
        .eval(source, false)
        .expect("native operation fixture parses")
    {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

fn prepared(tier: Tier, source: &str) -> Engine {
    let mut engine = Engine::new();
    engine.set_tier(if tier == Tier::Jit {
        Tier::Bytecode
    } else {
        tier
    });
    engine.set_tier_threshold(0);
    evaluate(&mut engine, source);
    engine.set_tier(tier);
    engine
}

#[test]
fn local_typeof_literals_avoid_strings_and_use_native_scalar_classification() {
    let mut engine = prepared(
        Tier::Jit,
        r#"
        function scalarTypes(v){
            return [typeof v==='number',typeof v!=='string',typeof v=='boolean',
                typeof v!='symbol',typeof v==='bigint',typeof v==='other'].join(',');
        }
        scalarTypes(7);
    "#,
    );
    evaluate(&mut engine, "for(var warm=0;warm<64;warm++)scalarTypes(7);");
    let env = engine.interp.global_env.clone();
    let function = engine
        .interp
        .get_var("scalarTypes", &env)
        .unwrap_or_else(|_| panic!("fixture binding exists"));
    let crate::value::Value::Obj(object) = function else {
        panic!("fixture is callable")
    };
    let object = object.borrow();
    let crate::value::Callable::User(user) = &object.call else {
        panic!("fixture is an ordinary function")
    };
    let chunk = user.func.execution_code().and_then(Option::as_ref).unwrap();
    assert!(chunk.jit.get().is_some_and(|code| code.is_some()));
    drop(object);
    let before = crate::bytecode::TEST_JIT_TYPEOF_HELPERS.with(std::cell::Cell::get);
    let checked = crate::bytecode::TEST_JIT_TYPEOF_IS_HELPERS.with(std::cell::Cell::get);
    assert_eq!(
        evaluate(
            &mut engine,
            r#"
            var values=[undefined,null,false,7,NaN,-0,Infinity,-Infinity,'é',Symbol(),1n,{},function(){}];
            var names=['undefined','object','boolean','number','number','number','number','number',
                'string','symbol','bigint','object','function'];
            var ok=true;
            for(var k=0;k<values.length;k++){
                var type=names[k];
                var expected=[type==='number',type!=='string',type==='boolean',
                    type!=='symbol',type==='bigint',false].join(',');
                ok=ok && scalarTypes(values[k])===expected;
            }
            ok
        "#,
        ),
        "true"
    );
    assert_eq!(
        crate::bytecode::TEST_JIT_TYPEOF_HELPERS.with(std::cell::Cell::get),
        before,
        "the executing native body consumes no typeof result strings"
    );
    assert!(
        crate::bytecode::TEST_JIT_TYPEOF_IS_HELPERS.with(std::cell::Cell::get) - checked <= 6,
        "scalar tests use native classification; only BigInt destruction stays checked"
    );
}

#[test]
fn local_typeof_tests_preserve_tdz_htmldda_and_effect_order() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = prepared(
            tier,
            r#"
            function numeric(v){if(typeof v==='number')return 1;return 2;}
            function defined(v){if(typeof v!='undefined')return 3;return 4;}
            function reverse(v){return ['number'==typeof v,'object'==typeof v,
                'function'==typeof v].join(',');}
            function uninitialized(){try{if(typeof hidden==='number')return 1;}
                catch(e){return e.name;}let hidden=7;return 0;}
            for(var warm=0;warm<64;warm++){numeric(7);defined(null);uninitialized();}
        "#,
        );
        let dda = engine.interp.make_html_dda();
        engine
            .interp
            .global
            .borrow_mut()
            .props
            .insert("dda", crate::value::Property::plain(dda));
        assert_eq!(
            evaluate(
                &mut engine,
                r#"
                var trace='',value={get n(){trace+='g';return NaN}};
                var revoked=Proxy.revocable(function(){},{});revoked.revoke();
                [numeric(value.n),trace,numeric(revoked.proxy),defined(dda),defined({}),
                    defined(undefined),uninitialized(),typeof absentTypeofGlobal,
                    reverse(null),reverse(revoked.proxy),reverse(dda)].join('|')
            "#,
            ),
            "1|g|2|4|3|4|ReferenceError|undefined|false,true,false|false,false,true|false,false,false",
            "{tier:?}"
        );
        engine.interp.gc_collect();
        assert_eq!(
            evaluate(&mut engine, "numeric(dda)+'|'+defined(dda)"),
            "2|4"
        );
        assert_eq!(
            evaluate(&mut engine, "var marker={},trace='',same=false;try{typeof {get fail(){trace+='g';throw marker}}.fail==='unknown';}catch(e){same=e===marker;}same+'|'+trace"),
            "true|g"
        );
    }
}

#[test]
fn native_operations55_regions_keep_native_lexical_initialization() {
    let mut engine = prepared(
        Tier::Jit,
        r#"
        var fields={first:3,second:4};
        function localRegion(fields,n) {
            let sum=0;
            for(let k=0;k<n;k++) {
                let a=fields.first;
                let b=fields.second;
                { const c=a+b; sum+=c; }
            }
            return sum;
        }
        for(var warm=0;warm<40;warm++)localRegion(fields,5);
    "#,
    );
    let regions = EXECUTED_REGIONS.with(std::cell::Cell::get);
    let helpers = crate::bytecode::TEST_JIT_LOCAL_RESET_HELPERS.with(std::cell::Cell::get);
    assert_eq!(evaluate(&mut engine, "localRegion(fields,30)"), "210");
    assert!(
        EXECUTED_REGIONS.with(std::cell::Cell::get) > regions,
        "exercise native SSA regions"
    );
    let after = crate::bytecode::TEST_JIT_LOCAL_RESET_HELPERS.with(std::cell::Cell::get);
    if std::env::var("LUMEN_SHARED_NATIVE_OPERATIONS").as_deref() == Ok("0") {
        assert!(
            after > helpers,
            "the old region lowering uses the checked reset path"
        );
    } else {
        assert_eq!(
            after, helpers,
            "native regions retain baseline reset capability"
        );
    }
}

#[test]
fn native_operations55_local_owners_and_tdz_survive_scope_reentry() {
    let source = r#"
        var marker={},caught=0,reads=0;
        function scopeReentry(n) {
            let saved=marker;
            for(let k=0;k<n;k++) {
                if(k===2) {
                    try { typeof hidden; } catch(e) { if(e instanceof ReferenceError)caught++; }
                    let hidden=k;
                    saved=hidden;
                } else {
                    let hidden={get value(){reads++;return k+1}};
                    saved=hidden.value;
                }
            }
            return saved;
        }
        for(var warm=0;warm<40;warm++)scopeReentry(4);
        caught=0;reads=0;
    "#;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = prepared(tier, source);
        assert_eq!(
            evaluate(
                &mut engine,
                "[scopeReentry(4),caught,reads,marker===marker].join('|')"
            ),
            "4|1|3|true",
            "{tier:?}"
        );
    }
}

#[test]
fn native_operations55_effects_keep_receiver_order_and_abrupt_completion() {
    let source = r#"
        var trace='',fail=false,token={};
        var target={value:3,get next(){trace+='g';if(fail)throw token;return this.value;},
            set next(v){trace+='s';this.value=v;}};
        var key={toString(){trace+='k';return 'next'}};
        function observed(o,key,n) {
            let start=n+1;
            let result=o[key];
            o.next=result+start;
            return o.value+start;
        }
        for(var warm=0;warm<40;warm++){target.value=3;observed(target,key,2);}
        target.value=3;trace='';
    "#;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = prepared(tier, source);
        assert_eq!(
            evaluate(
                &mut engine,
                "[observed(target,key,2),trace,target.value].join('|')"
            ),
            "9|kgs|6",
            "{tier:?}"
        );
        assert_eq!(evaluate(&mut engine, "trace='';fail=true;var same=false;try{observed(target,key,2)}catch(e){same=e===token}[same,trace,target.value].join('|')"), "true|kg|6", "{tier:?}");
    }
}

#[test]
fn native_void_releases_scalars_and_shared_owners_without_a_general_helper() {
    let mut engine = prepared(
        Tier::Jit,
        "function nativeVoid(v){return void v;}nativeVoid(7);",
    );
    evaluate(&mut engine, "for(var warm=0;warm<64;warm++)nativeVoid(7);");
    let env = engine.interp.global_env.clone();
    let crate::value::Value::Obj(object) = engine
        .interp
        .get_var("nativeVoid", &env)
        .unwrap_or_else(|_| panic!("fixture binding exists"))
    else {
        panic!("fixture is callable")
    };
    let object = object.borrow();
    let crate::value::Callable::User(user) = &object.call else {
        panic!("ordinary fixture")
    };
    let chunk = user.func.execution_code().and_then(Option::as_ref).unwrap();
    assert!(chunk.jit.get().is_some_and(|code| code.is_some()));
    drop(object);
    let before = crate::bytecode::TEST_JIT_VOID_HELPERS.with(std::cell::Cell::get);
    assert_eq!(evaluate(&mut engine,
        "var xs=[undefined,null,true,7,NaN,-0,Infinity,-Infinity,'wide é',Symbol(),{},function(){}];var ok=true;for(var k=0;k<xs.length;k++)ok=ok&&nativeVoid(xs[k])===undefined;ok"),"true");
    assert_eq!(
        crate::bytecode::TEST_JIT_VOID_HELPERS.with(std::cell::Cell::get),
        before
    );
}

#[test]
fn native_void_preserves_getvalue_effects_and_checked_final_destruction() {
    use std::{cell::Cell, rc::Rc};
    struct Probe(Rc<Cell<usize>>);
    impl Drop for Probe {
        fn drop(&mut self) {
            self.0.set(self.0.get() + 1);
        }
    }
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = prepared(
            tier,
            r#"
            function nativeVoid(v){return void v;}
            function observedVoid(o,k){return void o[k];}
            function temporaryVoid(){return void makeVoidTemp();}
            for(var warm=0;warm<64;warm++)nativeVoid(7);
        "#,
        );
        let drops = Rc::new(Cell::new(0));
        let count = drops.clone();
        let make = engine.interp.make_native_closure(
            "makeVoidTemp",
            0,
            Rc::new(move |i, _, _| {
                let probe = Probe(count.clone());
                let object = crate::value::Object::new(Some(i.object_proto.clone()));
                object.borrow_mut().call =
                    crate::value::Callable::NativeData(Rc::new(crate::value::NativeCallable {
                        body: crate::value::NativeCallableBody::Opaque(Rc::new(move |_, _, _| {
                            let _ = &probe;
                            Ok(crate::value::Value::Undefined)
                        })),
                        retained: None,
                        identity: Rc::from("void-owner-test"),
                    }));
                Ok(crate::value::Value::Obj(object))
            }),
        );
        engine.interp.global.borrow_mut().props.insert(
            "makeVoidTemp",
            crate::value::Property::plain(crate::value::Value::Obj(make)),
        );
        assert_eq!(
            evaluate(
                &mut engine,
                r#"
            var trace='',marker={},fail=false;
            var o={get value(){trace+='g';if(fail)throw marker;return {valueOf(){trace+='v';return 1}};}};
            var key={toString(){trace+='k';return 'value'}};
            var first=observedVoid(o,key),same=false;
            fail=true;try{observedVoid(o,key)}catch(e){same=e===marker;}
            [first===undefined,same,trace,nativeVoid(1n)===undefined].join('|')
        "#
            ),
            "true|true|kgkg|true",
            "{tier:?}"
        );
        let before = crate::bytecode::TEST_JIT_VOID_HELPERS.with(Cell::get);
        assert_eq!(evaluate(&mut engine, "temporaryVoid()===undefined"), "true");
        assert_eq!(drops.get(), 1, "last owner is destroyed at void, {tier:?}");
        if tier == Tier::Jit {
            assert!(
                crate::bytecode::TEST_JIT_VOID_HELPERS.with(Cell::get) > before,
                "the last owner uses checked destruction"
            );
        }
        engine.interp.gc_collect();
        assert_eq!(drops.get(), 1);
    }
}

#[test]
fn native_string_identity_and_destructuring_guards_preserve_observers() {
    let source = r#"
        function plainTemplate(v){return `${v}`;}
        function emptyPattern(v){const {}=v;return 3;}
        for(var warm=0;warm<64;warm++){plainTemplate('warm é');emptyPattern({});}
    "#;
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = prepared(tier, source);
        let strings = crate::bytecode::TEST_JIT_TO_STR_HELPERS.with(std::cell::Cell::get);
        let guards = crate::bytecode::TEST_JIT_DESTRUCTURE_GUARD_HELPERS.with(std::cell::Cell::get);
        assert_eq!(
            evaluate(
                &mut engine,
                r#"
            var xs=[false,7,NaN,'é',Symbol(),1n,{},function(){}];var ok=true;
            for(var k=0;k<xs.length;k++)ok=ok&&emptyPattern(xs[k])===3;
            ok&&plainTemplate('A\uD800\uDFFFé')==='A\uD800\uDFFFé'
        "#
            ),
            "true",
            "{tier:?}"
        );
        if tier == Tier::Jit {
            assert_eq!(
                crate::bytecode::TEST_JIT_TO_STR_HELPERS.with(std::cell::Cell::get),
                strings
            );
            assert_eq!(
                crate::bytecode::TEST_JIT_DESTRUCTURE_GUARD_HELPERS.with(std::cell::Cell::get),
                guards
            );
        }
        assert_eq!(
            evaluate(
                &mut engine,
                r#"
            var trace='',marker={},failure=false;
            var converting={[Symbol.toPrimitive](hint){trace+=hint;if(failure)throw marker;return 'converted';}};
            var first=plainTemplate(converting),same=false;
            failure=true;try{plainTemplate(converting)}catch(e){same=e===marker;}
            var nullish=0,symbols=0;
            for(var v of [undefined,null])try{emptyPattern(v)}catch(e){if(e instanceof TypeError)nullish++;}
            try{plainTemplate(Symbol())}catch(e){if(e instanceof TypeError)symbols++;}
            [first,same,trace,nullish,symbols,plainTemplate(-0),plainTemplate(1n)].join('|')
        "#
            ),
            "converted|true|stringstring|2|1|0|1",
            "{tier:?}"
        );
        engine.interp.gc_collect();
        assert_eq!(
            evaluate(&mut engine, "emptyPattern({})+plainTemplate('')"),
            "3"
        );
    }
}
