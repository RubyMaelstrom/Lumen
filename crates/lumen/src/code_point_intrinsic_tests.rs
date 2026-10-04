//! ECMA-262 e28783d5, CodePointAt / String.prototype.codePointAt:
//! receiver conversion precedes index conversion; surrogate positions are code units.
use crate::{bytecode::Tier, Completion, Engine};

fn eval(engine: &mut Engine, source: &str) -> String {
    match engine
        .eval(source, false)
        .expect("character fixture parses")
    {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn character_reads_cover_short_cached_and_smuggled_utf16_positions() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            eval(
                &mut engine,
                r#"
            function point(s,n){return s.codePointAt(n);}
            function unit(s,n){return s.charCodeAt(n);}
            function character(s,n){return s.charAt(n);}
            var s='A\uD83D\uDE00\uD800z\uDC00é',long='x'.repeat(80)+s;
            for(var warm=0;warm<160;warm++){point(s,1);point(long,81);unit(s,2);character(s,3);}
            var points=[65,0x1F600,0xDE00,0xD800,122,0xDC00,233],units=[65,0xD83D,0xDE00,0xD800,122,0xDC00,233],ok=true;
            for(var n=0;n<points.length;n++){
                ok=ok&&point(s,n)===points[n]&&point(long,n+80)===points[n];
                ok=ok&&unit(s,n)===units[n]&&unit(long,n+80)===units[n];
                ok=ok&&character(s,n).charCodeAt(0)===units[n];
            }
            var maximum=String.fromCharCode(0xDBFF,0xDFFF),privateScalar='\u{10F800}';
            ok&&point(maximum,0)===0x10FFFF&&point(maximum,1)===0xDFFF&&point(privateScalar,0)===0x10F800&&point(s,7)===undefined&&character(s,7)===''&&Number.isNaN(unit(s,7))
        "#
            ),
            "true",
            "{tier:?}"
        );
        engine.interp.gc_collect();
        assert_eq!(eval(&mut engine, "point(long,81)"), "128512", "{tier:?}");
    }
}

#[test]
fn code_point_indices_and_method_replacement_keep_coercion_order() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            eval(
                &mut engine,
                r#"
            var original=String.prototype.codePointAt;
            function point(s,n){return s.codePointAt(n);}
            for(var warm=0;warm<160;warm++)point('A😀',1);
            var trace='',receiver={toString(){trace+='r';return 'A😀'}},index={valueOf(){trace+='i';return 1}};
            var value=original.call(receiver,index),firstTrace=trace,failures=0;
            trace='';try{original.call(null,index)}catch(e){if(e instanceof TypeError)failures++;}
            for(var bad of [Symbol(),1n])try{point('A😀',bad)}catch(e){if(e instanceof TypeError)failures++;}
            var ok=point('A😀',NaN)===65&&point('A😀',-0.5)===65&&point('A😀',1.9)===0x1F600&&point('A😀',2.9)===0xDE00;
            for(var out of [-1,3,Infinity,-Infinity,Number.MAX_VALUE])ok=ok&&point('A😀',out)===undefined;
            String.prototype.codePointAt=function(n){return n+900};
            var replaced=point('A😀',2);String.prototype.codePointAt=original;
            [value,firstTrace,trace,failures,ok,replaced,point('A😀',1)].join('|')
        "#
            ),
            "128512|ri||3|true|902|128512",
            "{tier:?}"
        );
    }
}

#[test]
#[cfg(all(
    target_arch = "aarch64",
    any(target_os = "linux", target_os = "macos", target_os = "windows")
))]
fn numeric_code_point_calls_enter_the_guarded_native_intrinsic() {
    let mut engine = Engine::new();
    engine.set_tier(Tier::Jit);
    engine.set_tier_threshold(0);
    eval(
        &mut engine,
        r#"
        function point(s,n){return s.codePointAt(n);}
        for(var warm=0;warm<160;warm++)point('é😀',1);
    "#,
    );
    // CALL_IC_EPOCH is process-wide: a concurrently running test that recompiles a chunk
    // invalidates every cached call site, and the next few calls here legitimately revalidate
    // through the generic path. Measure a run during which the epoch stayed unchanged.
    let (mut hits, mut epoch_before, mut epoch_after) = (0, 0, 1);
    for _attempt in 0..20 {
        let before = crate::bytecode::TEST_JIT_CODE_POINT_INTRINSICS.with(std::cell::Cell::get);
        epoch_before = crate::bytecode::CALL_IC_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(
            eval(
                &mut engine,
                r#"
            var s='é😀',wide='x'.repeat(80)+s,ok=true;
            for(var n=0;n<40;n++)ok=ok&&point(s,1)===0x1F600&&point(wide,81)===0x1F600&&point('plain',2)===97;
            ok
        "#
            ),
            "true"
        );
        hits = crate::bytecode::TEST_JIT_CODE_POINT_INTRINSICS.with(std::cell::Cell::get) - before;
        epoch_after = crate::bytecode::CALL_IC_EPOCH.load(std::sync::atomic::Ordering::Relaxed);
        if epoch_before == epoch_after {
            break;
        }
    }
    assert!(hits >= 120,
        "warm String/Number calls bypass generic native argument decoding: {hits} hits, epoch {epoch_before}->{epoch_after}");
    assert!(engine.interp.fn_frames.is_empty());
}

/// charCodeAt on non-ASCII receivers and with any Number index, String.fromCharCode(Number) and
/// isNaN/Number.isNaN(Number) take the JIT's operand-only paths; other operands, replaced
/// functions and fresh (last-owner) receivers keep the full call. Expected values are Node's.
#[test]
fn unit_reads_from_char_code_and_is_nan_match_the_full_calls() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            eval(
                &mut engine,
                r#"
            function unit(s,n){return s.charCodeAt(n);}
            function fcc(n){return String.fromCharCode(n);}
            function nan1(x){return isNaN(x);}
            function nan2(x){return Number.isNaN(x);}
            function nan3(o,x){return o.isNaN(x);}
            var long='é'.repeat(40)+'abc', short='hé';
            for (var w=0; w<300; w++){ unit(long, w%45); unit(short, w&1); fcc(w); nan1(w); nan2(w);
                nan3(globalThis, w); nan3(Number, w); }
            var r=[];
            r.push(unit(long,0), unit(long,40), unit(long,42), unit(long,43), unit(long,-1),
                unit(long,1.9), unit(long,NaN), unit(long,1e300), unit(short,1), unit(short,5),
                unit(long,-0.5));
            r.push(fcc(65).charCodeAt(0), fcc(-1).charCodeAt(0), fcc(65536+66).charCodeAt(0),
                fcc(NaN).charCodeAt(0), fcc(Infinity).charCodeAt(0), fcc(97.9),
                fcc(0xD800).charCodeAt(0), fcc(233), fcc(-0).length);
            r.push(nan1(NaN), nan1(1), nan1('abc'), nan1('5'), nan2(NaN), nan2('abc'),
                nan3(globalThis, NaN), nan3({isNaN: isNaN}, 3), nan3(Number, NaN), nan3(Number, 'x'),
                nan1(undefined), nan1(-Infinity));
            r.push(unit('é'.repeat(70)+String(w), 69), unit(String(w)+'é', 3));
            var saved = String.fromCharCode; String.fromCharCode = function(){return 'patched'};
            r.push(fcc(65)); String.fromCharCode = saved; r.push(fcc(66));
            var savedNaN = isNaN; isNaN = function(){return 'p2'}; r.push(nan1(1));
            isNaN = savedNaN; r.push(nan1(NaN));
            r.join(',');
        "#
            ),
            "233,97,99,NaN,NaN,233,233,NaN,233,NaN,233,65,65535,66,0,0,a,55296,\u{e9},1,\
             true,false,true,false,true,false,true,false,true,false,true,false,233,233,patched,B,p2,true",
            "{tier:?}"
        );
    }
}

/// Map/Set/WeakMap/WeakSet methods and Math's numeric functions called on moved operands
/// without the native-call boundary, and Reflect.apply with such a target, keep the full
/// algorithms: CoerceKey, brand checks, weak-key validation, ToNumber's order for non-Number
/// arguments, and CreateListFromArrayLike's Gets for holes, getters and array-likes. Expected
/// values are Node's.
#[test]
fn operand_only_natives_and_reflect_apply_match_the_full_calls() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        assert_eq!(
            eval(
                &mut engine,
                r#"
var log = [];
function rec(x) { log.push(String(x)); }
function err(f) { try { return String(f()); } catch (e) { return e.constructor.name; } }
var keys = [{}, {}, 'k', -0, NaN, 1];
var m = new Map(), s = new Set(), wm = new WeakMap(), ws = new WeakSet();
function mg(x, k) { return x.get(k); }
function mh(x, k) { return x.has(k); }
function ms(x, k, v) { return x.set(k, v); }
function md(x, k) { return x.delete(k); }
function sa(x, k) { return x.add(k); }
function ab(x) { return Math.abs(x); }
function fl(x) { return Math.floor(x); }
function mx(a, b) { return Math.max(a, b); }
function ra(f, t, l) { return Reflect.apply(f, t, l); }
for (var i = 0; i < 400; i++) {
  var k = keys[i % keys.length];
  ms(m, k, i); mg(m, k); mh(m, k); sa(s, k); mh(s, k);
  if (typeof k === 'object') { ms(wm, k, i); mg(wm, k); mh(wm, k); sa(ws, k); mh(ws, k); }
  ab(i - 200); fl(i / 3); mx(i, 200 - i); ra(WeakMap.prototype.get, wm, [keys[0]]);
}
rec(mg(m, 0)); rec(mg(m, -0)); rec(mg(m, NaN)); rec(mg(m, 'k')); rec(mg(m, keys[1])); rec(mg(m, 'nope'));
rec(mh(m, +0)); rec(md(m, -0)); rec(mh(m, 0)); rec(ms(m, 'x', 1) === m); rec(m.size);
rec(mh(s, NaN)); rec(sa(s, 7) === s); rec(md(s, 7)); rec(s.size);
rec(mg(wm, keys[0])); rec(mg(wm, 'prim')); rec(mh(wm, 3)); rec(err(function () { return ms(wm, 'prim', 1); }));
rec(md(wm, keys[1])); rec(mh(wm, keys[1])); rec(mh(ws, keys[0])); rec(err(function () { return sa(ws, 5); }));
var sym = Symbol('w'); ms(wm, sym, 'sv'); rec(mg(wm, sym));
rec(err(function () { return mg({ get: Map.prototype.get }, 1); }));
rec(err(function () { return mg(s, 1); }));
rec(err(function () { return mh(m.has ? { has: Set.prototype.has } : 0, 1); }));
rec(err(function () { return mg(wm.get ? { get: WeakMap.prototype.get } : 0, keys[0]); }));
rec(err(function () { return mg(new WeakSet(), keys[0]); }));
var order = []; var o1 = { valueOf: function () { order.push('a'); return -3; } }, o2 = { valueOf: function () { order.push('b'); return 4; } };
rec(ab(o1)); rec(fl('2.7')); rec(mx(o1, o2)); rec(order.join('')); rec(ab(-0) === 0 && 1 / ab(-0)); rec(fl(-0.5)); rec(mx(NaN, 1)); rec(mx(-0, 0));
rec(ra(WeakMap.prototype.get, wm, [keys[0]])); rec(ra(WeakMap.prototype.get, wm, [,])); rec(ra(Map.prototype.get, m, ['k']));
var getterHits = 0; var arr = [0]; Object.defineProperty(arr, 0, { get: function () { getterHits++; return 'k'; } });
rec(ra(Map.prototype.get, m, arr)); rec(getterHits);
rec(ra(Map.prototype.get, m, { length: 1, 0: 'k' })); rec(err(function () { return ra(Map.prototype.get, s, ['k']); }));
rec(err(function () { return ra(5, m, []); })); rec(ra(function (a, b) { return a + b + this.z; }, { z: 1 }, [2, 3]));
rec(ra(Math.max, null, [3, 9, 4])); rec(ra(Math.max, null, [3, o2])); rec(order.join(''));
Array.prototype[0] = 'k'; rec(ra(Map.prototype.get, m, [,])); delete Array.prototype[0];
log.join(',');
        "#
            ),
            "399,399,394,398,397,undefined,true,true,false,true,6,true,true,true,6,396,undefined,false,TypeError,true,false,true,TypeError,sv,TypeError,TypeError,TypeError,TypeError,TypeError,3,2,4,aab,Infinity,-1,NaN,0,396,undefined,398,398,1,398,TypeError,TypeError,6,9,4,aabb,398",
            "{tier:?}"
        );
    }
}
