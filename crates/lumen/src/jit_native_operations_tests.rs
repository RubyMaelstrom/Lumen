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
