//! Inlining must retain the callee's execution-context semantics. ECMA-262
//! GetThisEnvironment, GetSuperBase, MakeSuperPropertyReference and ResolveBinding;
//! official local snapshot e28783d5fc9dc12b3de905961e2c71410b38a202 (2026-09-06).

use crate::{ast::Function, bytecode::Tier, value::Callable, Completion, Engine};
use std::rc::Rc;

fn evaluate(engine: &mut Engine, source: &str) -> String {
    match engine
        .eval(source, false)
        .expect("inline context fixture parses")
    {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

fn function(engine: &mut Engine, name: &str) -> Rc<Function> {
    let env = engine.interp.global_env.clone();
    let value = engine
        .interp
        .get_var(name, &env)
        .unwrap_or_else(|_| panic!("missing fixture function {name}"));
    let object = value.as_obj().expect("fixture function object").borrow();
    let Callable::User(user) = &object.call else {
        panic!("fixture user function")
    };
    user.func.clone()
}

fn require_hot_native(engine: &mut Engine, name: &str, inlined: bool) {
    let function = function(engine, name);
    let chunk = function
        .code
        .get()
        .and_then(Option::as_ref)
        .expect("hot fixture must compile");
    if cfg!(all(
        any(target_arch = "aarch64", target_arch = "x86_64"),
        any(
            target_os = "macos",
            target_os = "linux",
            target_os = "windows"
        )
    )) {
        assert!(
            chunk.jit.get().flatten().is_some(),
            "{name} must retain ordinary native execution"
        );
        if crate::bytecode::inline_recompile_at() == 100 {
            assert!(
                chunk.inline_attempted.get(),
                "{name} must cross the hot inline checkpoint"
            );
            assert_eq!(
                function.code2.get().and_then(Option::as_ref).is_some(),
                inlined,
                "{name} must respect the splice context capability"
            );
        }
    }
}

#[test]
fn hot_super_method_keeps_home_object_in_an_ordinary_caller() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            evaluate(
                &mut engine,
                r#"
            'use strict';
            class Base {value(x){return x+1;}}
            class Derived extends Base {value(x){return super.value(x)+1;}}
            var receiver=new Derived(), nativeSuper=Derived.prototype.value;
            function invokeSuper(x){return receiver.value(x)+1;}
            function driveSuper(){var sum=0;for(var k=0;k<1000;k++)sum+=invokeSuper(k);return sum;}
            driveSuper();
        "#
            ),
            "502500",
            "{tier:?}"
        );
        if tier == Tier::Jit {
            require_hot_native(&mut engine, "invokeSuper", false);
            let method = function(&mut engine, "nativeSuper");
            let chunk = method
                .code
                .get()
                .and_then(Option::as_ref)
                .expect("native super body");
            if cfg!(any(target_arch = "aarch64", target_arch = "x86_64")) {
                assert!(chunk.jit.get().flatten().is_some());
            }
            assert!(chunk
                .jit_ops()
                .iter()
                .any(|op| matches!(op, crate::bytecode::Op::SuperBase)));
        }
    }
}

#[test]
fn hot_super_methods_keep_distinct_homes_receivers_and_live_prototypes() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            evaluate(
                &mut engine,
                r#"
            'use strict';
            class Base {read(){return this.mark+10;}}
            class Derived extends Base {read(){return super.read()+1;}}
            class OtherBase {read(){return this.mark+100;}}
            class Other extends OtherBase {invoke(obj){return obj.read()+super.read();}}
            var target=new Derived(), caller=new Other();target.mark=7;caller.mark=2;
            function drive(){var sum=0;for(var k=0;k<600;k++)sum+=caller.invoke(target);return sum;}
            var before=drive();
            Object.setPrototypeOf(Derived.prototype,{read(){return this.mark+1000;}});
            before+'|'+caller.invoke(target)+'|'+target.mark+'|'+caller.mark;
        "#
            ),
            "72000|1110|7|2",
            "{tier:?}"
        );
    }
}

#[test]
fn hot_private_names_and_typeof_closure_reads_keep_definition_environments() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            evaluate(
                &mut engine,
                r#"
            'use strict';
            class Secret {#value=7;read(x){return this.#value+x;}}
            var secret=new Secret();
            function hotPrivate(x){return secret.read(x)+1;}
            var closed='wrong';
            function make(){var closed=12;return function(){return typeof closed;};}
            var closureRead=make();
            function hotTypeof(){return closureRead()+'!';}
            function drive(){var sum=0,out;for(var k=0;k<600;k++){
                sum+=hotPrivate(1);out=hotTypeof();}return sum+'|'+out;}
            drive();
        "#
            ),
            "5400|number!",
            "{tier:?}"
        );
        if tier == Tier::Jit {
            require_hot_native(&mut engine, "hotPrivate", false);
            require_hot_native(&mut engine, "hotTypeof", false);
        }
    }
}

#[test]
fn frame_independent_effects_still_inline_with_local_and_receiver_remapping() {
    let mut engine = Engine::new();
    engine.set_tier(Tier::Jit);
    engine.set_tier_threshold(0);
    assert_eq!(
        evaluate(
            &mut engine,
            r#"
        'use strict';
        var receiver={offset:3,compute(x){var y=x+this.offset;return {value:y};}};
        function hotIndependent(x){var y=receiver.compute(x);return y.value+1;}
        function drive(){var sum=0;for(var k=0;k<600;k++)sum+=hotIndependent(2);return sum;}
        drive();
    "#
        ),
        "3600"
    );
    require_hot_native(&mut engine, "hotIndependent", true);
}

/// A spliced self-hosted built-in (`crate::self_hosted`) compiles its intrinsics to the same
/// operations as its own body: they resolve in its private environment, never as free names of
/// the caller. The built-ins are strict, so only strict callers splice them (and only at calls
/// that are not proper tail calls).
#[test]
fn hot_self_hosted_methods_keep_their_intrinsics_when_spliced() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            evaluate(
                &mut engine,
                r#"
            'use strict';
            var values=[1,2,3,4];
            function anyLarge(list){var r=list.some(function(x){return x>3;});return r;}
            function firstAbove(list){var r=list.find(function(x){return x>2;});return r;}
            function drive(){var n=0;for(var k=0;k<1000;k++)n+=anyLarge(values)+firstAbove(values);return n;}
            drive()+'|'+anyLarge([])+'|'+(function(){try{anyLarge(null)}catch(e){return e.name}})();
        "#
            ),
            "4000|false|TypeError",
            "{tier:?}"
        );
        if tier == Tier::Jit {
            require_hot_native(&mut engine, "anyLarge", true);
            // Where the JIT spliced the call (asserted above), the body kept its operations.
            let caller = function(&mut engine, "anyLarge");
            if let Some(spliced) = caller.code2.get().and_then(Option::as_ref) {
                assert!(spliced
                    .jit_ops()
                    .iter()
                    .any(|op| matches!(op, crate::bytecode::Op::Abstract(_))));
            }
        }
    }
}

/// A splice compiles the callee's declarations in fresh slots without touching the caller's
/// capture analysis: a callee `let n` spliced before the caller's own captured block `let n`
/// must not consume that binding's homed declaration (even when the splice is abandoned).
#[test]
fn spliced_lexicals_leave_the_callers_homed_block_bindings_alone() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            evaluate(
                &mut engine,
                r#"
            'use strict';
            function Sf(e){return Array.isArray(e)&&e.length===2}
            function Df(t){if(!Sf(t))return!1;let n=t[0];return n===30||n===35}
            var out = [];
            function mp(e,a){a()}
            var homedCaller=(e,t)=>{if(Df(t))e(1);else if(t[0]===6){let n=t[1];
                Df(n)?e(2):mp(e,()=>{e(n)})}else e(0)};
            function rec(a) { out.push(String(a)); }
            for (var i = 0; i < 2000; i++) {
                homedCaller(rec, [35, 'x']);
                homedCaller(rec, [6, [35, 'y']]);
                homedCaller(rec, [6, [7, 'z']]);
                homedCaller(rec, [9]);
            }
            out.length + '|' + out.slice(-6).join(',');
        "#
            ),
            "8000|7,z,0,1,2,7,z,0",
            "{tier:?}"
        );
    }
}
