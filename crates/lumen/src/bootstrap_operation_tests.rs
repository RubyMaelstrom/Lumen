//! Operations a platform bootstrap performs thousands of times per Realm: retained class
//! expressions inside large functions, property removal from large objects, and property
//! descriptor conversion in both directions. Each fast path must keep the generic algorithm's
//! observable behavior on every tier.

use crate::bytecode::Tier;
use crate::{Completion, Engine};

fn all_tiers(source: &str, expected: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        for threshold in [0, 8] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(threshold);
            match engine.eval(source, false).expect("fixture parses") {
                Completion::Value(actual) => {
                    assert_eq!(actual, expected, "{tier:?} threshold {threshold}")
                }
                Completion::Throw { name, message } => {
                    panic!("{tier:?} threshold {threshold}: {name}: {message}")
                }
            }
        }
    }
}

/// A retained class expression reads and writes the compiled function's slot locals through
/// its projected scope (heritage, computed keys, decorators-free class elements), while its
/// methods only reach captured bindings. Unreferenced locals need not be projected.
#[test]
fn retained_class_expressions_see_and_update_the_locals_they_name() {
    all_tiers(
        r#"
        function build(round) {
            "use strict";
            let a0 = 0, a1 = 1, a2 = 2, a3 = 3, a4 = 4, a5 = 5, a6 = 6, a7 = 7, a8 = 8, a9 = 9;
            let b0 = 10, b1 = 11, b2 = 12, b3 = 13, b4 = 14, b5 = 15, b6 = 16, b7 = 17;
            let Base = class { who() { return "base"; } };
            let key = "m" + round;
            let counter = round;
            let shadow = "outer";
            const captured = "captured";
            const C = class extends Base {
                [key + (counter += 1)]() { return captured; }
                static [(counter += 10, "s" + typeof shadow)]() { return 1; }
                [(shadow = "written", "w")]() { return 2; }
            };
            const D = class { [a9 + b7]() { return 3; } };
            let total = 0;
            for (let i = 0; i < 3; i++) {
                const E = class extends (i % 2 ? Base : Object) { [key + i]() { return i; } };
                total += new E()[key + i]();
            }
            return [
                Object.getOwnPropertyNames(C.prototype).join(","),
                Object.getOwnPropertyNames(C).filter(n => n[0] === "s").join(","),
                counter, shadow, new C().who(), new C()[key + (round + 1)](),
                Object.getOwnPropertyNames(D.prototype).join(","), total,
                a0 + a1 + a2 + a3 + a4 + a5 + a6 + a7 + a8 + b0 + b1 + b2 + b3 + b4 + b5 + b6,
            ].join("|");
        }
        let last;
        for (let round = 0; round < 40; round++) last = build(round);
        last
        "#,
        "constructor,m3940,w|sstring|50|written|base|captured|26,constructor|3|127",
    );
}
