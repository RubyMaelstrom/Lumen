//! ECMA-262 snapshot e28783d5, String.prototype.slice/substring/substr
//! (spec.html:36827, 36924, 54429). Substring boundaries remain UTF-16 code-unit boundaries.

use crate::bytecode::Tier;
use crate::value::Value;
use crate::{Completion, Engine};

fn check(source: &str, expected: &str) {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        match engine
            .eval(source, false)
            .expect("substring fixture parses")
        {
            Completion::Value(value) => assert_eq!(value, expected, "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
        engine.interp.gc_collect();
        assert!(matches!(engine.eval("1+2", false), Ok(Completion::Value(value)) if value == "3"));
    }
}

#[test]
fn unicode_substrings_preserve_every_code_unit_boundary() {
    check(
        r#"
        function run(){
            var base='x'.repeat(64)+'é中😀\ud800!\udfff';
            var sources=[base+'z',base+String.fromCodePoint(0x10ffff)+'z'],s;
            function reference(a,b){
                var out='';
                for(var k=a;k<b;k++)out+=String.fromCharCode(s.charCodeAt(k));
                return out;
            }
            for(var source of sources){s=source;
            for(var a=60;a<=s.length;a++)for(var b=a;b<=s.length;b++){
                var expected=reference(a,b);
                if(s.slice(a,b)!==expected||s.substring(a,b)!==expected||
                   s.substring(b,a)!==expected||s.substr(a,b-a)!==expected)return a+':'+b;
            }}
            var whole=s.slice(0), alias=s; s+='!';
            return [whole===alias,whole.length,s.length,
                whole.slice(-1),whole.substring(Infinity),whole.substr(-0.5,1)].join('|');
        }run()
        "#,
        "true|74|75|z||x",
    );
}

#[test]
fn substring_coercions_keep_original_receiver_and_abrupt_order() {
    check(
        r#"
        function run(){
            var source='x'.repeat(64)+'é😀z',log=[];
            var receiver={toString(){log.push('receiver');return source}};
            var start={valueOf(){log.push('start');
                for(var k=0;k<80;k++)('different'+k+'é'.repeat(64)).substring(1,3);
                source+='changed';return 65}};
            var end={valueOf(){log.push('end');return 67}};
            var a=String.prototype.substring.call(receiver,start,end);
            if(a!=='😀')throw 99;
            var b=String.prototype.slice.call({toString(){log.push('r2');return source}},
                {valueOf(){log.push('throw');throw 7}}, {valueOf(){log.push('unreached');return 2}});
            return b;
        }
        var caught;
        try{run()}catch(e){caught=e}
        // A separate trace keeps the thrown fixture's log observable.
        var log=[],s='x'.repeat(64)+'é😀z';
        var result=String.prototype.substring.call({toString(){log.push('receiver');return s}},
            {valueOf(){log.push('start');s+='changed';return 65}},
            {valueOf(){log.push('end');return 67}});
        var bad={valueOf(){log.push('bad');throw 9}};
        try{s.slice(bad,{valueOf(){log.push('unreached');return 2}})}catch(e){log.push(e)}
        [caught,result,log.join(','),s.length].join('|')
        "#,
        "7|😀|receiver,start,end,bad,9|75",
    );
}

#[test]
fn sparse_byte_index_matches_utf16_reconstruction_and_declines_splits() {
    for tail in ["é中😀z", "\u{10f800}x\u{10ffff}", "😀😀é", "éa中b"] {
        let source: crate::lstr::LStr = format!("{}{}", "x".repeat(64), tail).into();
        let units = crate::jstr::units(&source);
        let index = crate::jstr::Utf8Index::new(&source, usize::MAX);
        let mut copied = 0;
        for start in 60..=units.len() {
            for end in start..=units.len() {
                if let Some(bytes) = index.range(&source, start, end) {
                    assert_eq!(bytes, crate::jstr::from_units(&units[start..end]));
                    copied += 1;
                }
            }
        }
        assert!(copied > 0);
    }
    let source: crate::lstr::LStr = format!("{}😀z", "x".repeat(64)).into();
    let index = crate::jstr::Utf8Index::new(&source, usize::MAX);
    assert!(index.range(&source, 64, 65).is_none());
    assert!(index.range(&source, 65, 66).is_none());
    assert_eq!(index.range(&source, 64, 66), Some("😀"));
    let noncanonical: crate::lstr::LStr = format!(
        "{}{}{}",
        "x".repeat(64),
        crate::jstr::smuggle(0xd83d),
        crate::jstr::smuggle(0xde00)
    )
    .into();
    let index = crate::jstr::Utf8Index::new(&noncanonical, usize::MAX);
    assert!(index.range(&noncanonical, 64, 66).is_none());
    let dense: crate::lstr::LStr = "é".repeat(128).into();
    let declined = crate::jstr::Utf8Index::new(&dense, 24);
    assert!(declined.range(&dense, 0, 10).is_none());
    assert_eq!(declined.heap_bytes(), std::mem::size_of_val(&declined));
}

#[test]
fn unicode_slice_index_stays_in_existing_cache_budget_and_pins_identity() {
    let mut engine = Engine::new();
    let original: crate::lstr::LStr = format!("{}é😀z", "x".repeat(128)).into();
    let units = engine.interp.units_full(&original);
    let before = engine.interp.str_units.stats();
    let whole = engine.interp.slice_units(&original, &units, 0, units.len());
    assert!(crate::lstr::LStr::ptr_eq(&original, &whole));
    let after = engine.interp.str_units.stats();
    assert_eq!(before.0, after.0);
    assert!(
        after.1 > before.1,
        "the sparse index is charged to the existing cache"
    );
    let high = engine.interp.slice_units(&original, &units, 129, 130);
    assert_eq!(crate::jstr::units(&high), [0xd83d]);
    for n in 0..80 {
        let source: crate::lstr::LStr = format!("{n}{}é", "x".repeat(128)).into();
        let view = engine.interp.units_full(&source);
        engine.interp.slice_units(&source, &view, 1, view.len());
    }
    let (entries, bytes) = engine.interp.str_units.stats();
    assert!(entries <= 64 && bytes <= 16 << 20);
    let copied = engine
        .interp
        .slice_units(&original, &units, 128, units.len());
    assert!(matches!(Value::Str(copied), Value::Str(s) if s.as_str() == "é😀z"));
}
