//! ECMA-262 e28783d5 (local official snapshot, 2026-09-06): GetValue,
//! ToPropertyKey, OrdinaryGet, OrdinaryHasProperty and the `in` operator.
//! The local-source JIT region may skip operand materialization only for a
//! live ordinary own dense data element; every uncertain case replays the
//! original LoadLocal/LoadLocal/op sequence.

use crate::{bytecode::Tier, Completion, Engine};

fn eval(engine: &mut Engine, source: &str) -> String {
    match engine
        .eval(source, false)
        .expect("native-array fixture parses")
    {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}\n{source}"),
    }
}

fn engine(tier: Tier) -> Engine {
    let mut engine = Engine::new();
    engine.set_tier(tier);
    engine.set_tier_threshold(0);
    engine.interp.def_method(
        &engine.interp.global,
        "collectNativeArrayLocalTest",
        0,
        |interp, _, _| {
            interp.gc_collect();
            Ok(crate::value::Value::Undefined)
        },
    );
    engine
}

fn check_all_tiers(source: &str, expected: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = engine(tier);
        assert_eq!(eval(&mut engine, source), expected, "{tier:?}");
    }
}

#[test]
fn local_local_getelem_and_in_compile_and_take_native_own_element_hits() {
    let mut engine = engine(Tier::Jit);
    let before = super::test_local_array_fusion_hits();
    assert_eq!(
        eval(
            &mut engine,
            r#"
            function localGetLoop() {
                // `values` is lexical, so the bytecode keeps the explicit
                // LoadLocal/LoadLocal/GetElem sequence. `index++` makes its
                // numeric home eligible for the DReg-key region path.
                let values = [1, 2, 3, 4];
                let index = 0, total = 0;
                while (index < 4) {
                    total += values[index];
                    index++;
                }
                return total;
            }
            function localInLoop() {
                let values = [1, 2, 3, 4];
                let index = 0, present = 0;
                while (index < 4) {
                    if (index in values) present++;
                    index++;
                }
                return present;
            }
            var result;
            for (var warm = 0; warm < 8; warm++) {
                result = localGetLoop() + ':' + localInLoop();
            }
            result;
        "#
        ),
        "10:4"
    );
    let after = super::test_local_array_fusion_hits();
    assert!(
        after[0] > before[0],
        "no successful 3-op GetElem hit with a numeric DReg key"
    );
    assert!(
        after[2] > before[2],
        "no successful 3-op In hit with a numeric DReg key"
    );
}

#[test]
fn local_array_holes_inherited_getters_gc_and_live_prototype_mutation_fall_back() {
    check_all_tiers(
        r#"
        var backing = [2, 4, 8], getterCalls = 0, getterReceiver = false;
        function readLocalArray() {
            let values = backing;
            let index = 0, total = 0;
            while (index < 3) {
                total += values[index];
                index++;
            }
            return total;
        }
        var before = readLocalArray();
        delete backing[1];
        Object.defineProperty(Array.prototype, '1', {
            configurable: true,
            get() {
                getterCalls++;
                getterReceiver = this === backing;
                collectNativeArrayLocalTest();
                backing[2] = 9;
                return 5;
            }
        });
        var after;
        try {
            after = readLocalArray();
        } finally {
            delete Array.prototype[1];
        }
        before + ':' + after + ':' + getterCalls + ':' + getterReceiver;
    "#,
        "14:16:1:true",
    );
}

#[test]
fn local_array_in_sees_inherited_accessor_without_calling_it() {
    check_all_tiers(
        r#"
        var getterCalls = 0;
        Object.defineProperty(Array.prototype, '1', {
            configurable: true,
            get() { getterCalls++; return 9; }
        });
        function localHasHole() {
            let values = [];
            values[0] = 3;
            values[2] = 7;
            let index = 1, present = 0, attempts = 0;
            while (attempts < 1) {
                if (index in values) present++;
                attempts++;
            }
            return present;
        }
        var present;
        try {
            present = localHasHole();
        } finally {
            delete Array.prototype[1];
        }
        present + ':' + getterCalls;
    "#,
        "1:0",
    );
}

#[test]
fn local_proxy_reads_and_has_preserve_conversion_trap_order() {
    check_all_tiers(
        r#"
        var events = '';
        function proxyGetLoop() {
            let values = new Proxy([7], {
                get(target, key, receiver) {
                    events += 'get:' + key + ';';
                    return Reflect.get(target, key, receiver);
                }
            });
            let key = { [Symbol.toPrimitive](hint) {
                events += 'key:' + hint + ';';
                return 0;
            }};
            let i = 0, total = 0;
            while (i < 2) {
                total += values[key];
                i++;
            }
            return total;
        }
        let getValue = proxyGetLoop(), getEvents = events;
        events = '';
        function proxyHasLoop() {
            let values = new Proxy({}, {
                has(target, key) { events += 'has:' + key + ';'; return true; }
            });
            let key = { toString() { events += 'string;'; return 'x'; } };
            let i = 0, count = 0;
            while (i < 2) {
                if (key in values) count++;
                i++;
            }
            return count;
        }
        let hasValue = proxyHasLoop();
        getValue + ':' + getEvents + '|' + hasValue + ':' + events;
    "#,
        "14:key:string;get:0;key:string;get:0;|2:string;has:x;string;has:x;",
    );
}

#[test]
fn local_index_guards_preserve_negative_zero_fraction_nan_large_keys_and_symbols() {
    check_all_tiers(
        r#"
        let values = [];
        values[0] = 'zero';
        values['1.5'] = 'fraction';
        values.NaN = 'nan';
        values.Infinity = 'infinity';
        values['4294967295'] = 'large';
        values.x = 'coerced';
        let symbolKey = Symbol('element');
        values[symbolKey] = 'symbol';
        let conversions = 0;
        let objectKey = { [Symbol.toPrimitive](hint) {
            if (hint !== 'string') throw 'wrong hint';
            conversions++;
            return 'x';
        }};
        function convertedRead(rawKey) {
            let array = values, key = rawKey, n = 0, result;
            while (n < 1) {
                result = array[key];
                n++;
            }
            return result;
        }
        convertedRead(-0) + '|' + convertedRead(1.5) + '|' +
        convertedRead(NaN) + '|' + convertedRead(Infinity) + '|' +
        convertedRead(4294967295) + '|' + convertedRead(objectKey) + '|' +
        convertedRead(symbolKey) + ':' + conversions;
    "#,
        "zero|fraction|nan|infinity|large|coerced|symbol:1",
    );
}

#[test]
fn local_get_and_in_slow_paths_preserve_null_order_throw_identity_and_tdz() {
    check_all_tiers(
        r#"
        var events = '', sentinel = { id: 'sentinel' }, coercions = 0;
        let key = { [Symbol.toPrimitive]() { coercions++; events += 'key;'; return 'x'; } };
        function nullRead() {
            let receiver = null, localKey = key, n = 0;
            while (n < 1) { receiver[localKey]; n++; }
        }
        function nullIn() {
            let receiver = null, localKey = key, n = 0;
            while (n < 1) { localKey in receiver; n++; }
        }
        var nullReadName = '', nullInName = '';
        try { nullRead(); } catch (error) { nullReadName = error.name; }
        try { nullIn(); } catch (error) { nullInName = error.name; }
        var nullOrder = events + ':' + coercions;

        function throwingRead() {
            let receiver = [1];
            let localKey = { [Symbol.toPrimitive]() { throw sentinel; } };
            let n = 0;
            while (n < 1) { receiver[localKey]; n++; }
        }
        var sameThrown = false;
        try { throwingRead(); } catch (error) { sameThrown = error === sentinel; }

        function tdzRead() {
            let receiver = [1], total = 0;
            for (let n = 0; n < 1; n++) total += receiver[lateKey];
            let lateKey = 0;
            return total;
        }
        var tdzName = '';
        try { tdzRead(); } catch (error) { tdzName = error.name; }
        nullReadName + ':' + nullInName + ':' + nullOrder + ':' +
        sameThrown + ':' + tdzName;
    "#,
        "TypeError:TypeError::0:true:ReferenceError",
    );
}
