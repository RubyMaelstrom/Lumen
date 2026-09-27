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
