//! ECMA-262 ScriptEvaluation / PrepareForOrdinaryCall / BuiltinCallOrConstruct.
use crate::{Engine, Value};

#[test]
fn native_operation_observes_script_caller_across_realms_and_eval() {
    for tier in [
        crate::bytecode::Tier::Interp,
        crate::bytecode::Tier::Bytecode,
        crate::bytecode::Tier::Jit,
    ] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        engine.define_global("caller", 0, |ctx, _, _| Ok(ctx.script_caller_global()));
        let root = engine.global_this();
        let child = engine.ctx().create_embed_realm();
        assert!(engine
            .ctx()
            .member_set(&root, "child", child.clone())
            .is_ok());
        assert!(engine
            .ctx()
            .member_set(&child, "parent", root.clone())
            .is_ok());
        assert!(engine
            .ctx()
            .with_embed_realm(&child, |ctx| {
                ctx.define_embed_global("caller", 0, |ctx, _, _| Ok(ctx.script_caller_global()));
            })
            .is_ok());
        let outcome = engine.eval(r#"
            function assert(x) { if (!x) throw Error('script caller'); }
            assert(child.caller() === globalThis);
            assert(child.caller.call(null) === globalThis);
            assert(Reflect.apply(child.caller, null, []) === globalThis);
            assert(child.caller.bind(null)() === globalThis);
            child.eval('parent.assert(parent.caller() === globalThis); globalThis.f = () => parent.caller()');
            assert(child.f() === child);
            (function () { child.eval('parent.assert(parent.caller() === globalThis)'); })();
            assert(child.caller() === globalThis);
            'ok'
        "#, false).unwrap();
        assert!(
            matches!(outcome, crate::Completion::Value(ref value) if value == "ok"),
            "{tier:?}"
        );
        let module = engine
            .eval(
                "if (child.caller() !== globalThis) throw Error('module caller');",
                true,
            )
            .unwrap();
        assert!(
            matches!(module, crate::Completion::Value(_)),
            "module {tier:?}"
        );
        assert!(matches!(engine.ctx().script_caller_global(), Value::Obj(_)));
    }
}
