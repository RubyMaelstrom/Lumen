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
