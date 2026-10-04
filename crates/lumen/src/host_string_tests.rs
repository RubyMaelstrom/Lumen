//! Strings crossing the host boundary.
//!
//! The engine stores a lone surrogate as a scalar in U+10F800..=U+10FFFF (see `crate::jstr`), and
//! a character in that Plane 16 private-use range as its surrogate pair of such scalars. Rust text
//! holds Unicode scalar values only, so host constructors (`Value::str`, `Value::from_string`) and
//! host source text must store such a character as its pair, and host reads (`coerce_string`,
//! `Value::as_text`) must return the character itself, replacing lone surrogates with U+FFFD as
//! the Web IDL `USVString` conversion does. Expected values were checked against Node v24.

use crate::interpreter::Interp;
use crate::realm_inline_guard_tests::{eval_string, set_global, TIERS};
use crate::value::Value;
use crate::{Completion, Engine};

/// `hostText(n)`: a string a host built from Rust text.
fn host_text(_: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    Ok(match args.first().and_then(Value::as_num_opt).unwrap_or(0.0) as u32 {
        0 => Value::from_string("\u{10FFFD}".to_string()),
        1 => Value::str("\u{10BFF}\u{10FFFD}"),
        2 => Value::from_string("a\u{10F800}b\u{10FFFF}".to_string()),
        3 => Value::from_utf16(&[0xD800, 0x61, 0xDBFF, 0xDFFD, 0xDC00]),
        _ => Value::str("plain ascii"),
    })
}

/// `hostRead(s)`: the host's Rust text for `s`, reported as space-separated scalar values.
fn host_read(it: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    let text = it.coerce_string(args.first().unwrap_or(&Value::Undefined))?;
    if let Some(as_text) = args.first().and_then(Value::as_text) {
        assert_eq!(&*text, &*as_text, "coerce_string and as_text agree on strings");
    }
    let scalars: Vec<String> = text.chars().map(|c| format!("{:x}", c as u32)).collect();
    Ok(Value::from_string(scalars.join(" ")))
}

/// `hostUnits(s)`: the code units `coerce_utf16` reports, rebuilt with `from_utf16`.
fn host_units(it: &mut Interp, _: Value, args: &[Value]) -> Result<Value, Value> {
    let units = it.coerce_utf16(args.first().unwrap_or(&Value::Undefined))?;
    Ok(Value::from_utf16(&units))
}

fn install(it: &mut Interp) {
    let global = it.global.clone();
    it.def_method(&global, "hostText", 1, host_text);
    it.def_method(&global, "hostRead", 1, host_read);
    it.def_method(&global, "hostUnits", 1, host_units);
    set_global(it, "probeReady", Value::Bool(true));
}

const UNITS: &str = r#"function units(s) {
    let out = [];
    for (let i = 0; i < s.length; i++) out.push(s.charCodeAt(i).toString(16));
    return out.join(" ");
}"#;

#[test]
fn host_built_strings_keep_plane_16_private_use_characters() {
    for tier in TIERS {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        let it = &mut *engine.interp;
        install(it);
        // The loop warms the caller so the compiled tiers also run the host calls.
        let source = format!(
            r#"{UNITS}
            (function () {{
                let result;
                for (let round = 0; round < 300; round++) {{
                    const a = hostText(0), b = hostText(1), c = hostText(2), d = hostText(3);
                    result = [
                        a.length, units(a), a === "\u{{10FFFD}}", a.codePointAt(0).toString(16),
                        units(b), b === "\u{{10BFF}}\u{{10FFFD}}",
                        units(c), [...c].length,
                        units(d), d.isWellFormed(),
                        units(hostText(4)),
                    ].join("|");
                }}
                return result;
            }})()"#
        );
        assert_eq!(
            eval_string(it, &source),
            "2|dbff dffd|true|10fffd|d802 dfff dbff dffd|true|\
             61 dbfe dc00 62 dbff dfff|4|d800 61 dbff dffd dc00|false|\
             70 6c 61 69 6e 20 61 73 63 69 69",
            "{tier:?}"
        );
    }
}

#[test]
fn host_reads_return_characters_and_replace_lone_surrogates() {
    for tier in TIERS {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        let it = &mut *engine.interp;
        install(it);
        let source = format!(
            r#"{UNITS}
            (function () {{
                let result;
                for (let round = 0; round < 300; round++) {{
                    result = [
                        hostRead("\u{{10FFFD}}"),
                        hostRead("\u{{10F800}}x\u{{10FFFF}}"),
                        hostRead("\uD800" + "a" + "\uDFFD"),
                        hostRead("\uDBFF" + "\uDFFD"),
                        hostRead(String.fromCharCode(0xDBFE, 0xDC00)),
                        hostRead("é\u{{1F600}}"),
                        hostRead(42),
                        units(hostUnits("\uD800\u{{10FFFD}}\uDC00z")),
                    ].join("|");
                }}
                return result;
            }})()"#
        );
        assert_eq!(
            eval_string(it, &source),
            "10fffd|10f800 78 10ffff|fffd 61 fffd|10fffd|10f800|e9 1f600|34 32|\
             d800 dbff dffd dc00 7a",
            "{tier:?}"
        );
    }
}

#[test]
fn host_values_round_trip_through_rust_text() {
    let mut engine = Engine::new();
    let it = &mut *engine.interp;
    for text in ["\u{10FFFD}", "\u{10BFF}\u{10FFFD}", "a\u{10F800}\u{10FBFF}\u{10FC00}z", "é😀"] {
        let value = Value::str(text);
        assert_eq!(value.as_text().as_deref(), Some(text));
        assert_eq!(it.coerce_string(&value).ok().as_deref(), Some(text));
        assert_eq!(it.coerce_usv_string(&value).ok().as_deref(), Some(text));
        assert_eq!(
            it.coerce_utf16(&value).ok(),
            Some(text.encode_utf16().collect::<Vec<_>>())
        );
        assert_eq!(
            Value::from_string(text.to_string()).as_text().as_deref(),
            Some(text)
        );
        assert_eq!(
            Value::from_utf16(&text.encode_utf16().collect::<Vec<_>>())
                .as_text()
                .as_deref(),
            Some(text)
        );
    }
    let lone = Value::from_utf16(&[0x61, 0xDFFD, 0xD800]);
    assert_eq!(lone.as_text().as_deref(), Some("a\u{FFFD}\u{FFFD}"));
    assert_eq!(it.coerce_utf16(&lone).ok(), Some(vec![0x61, 0xDFFD, 0xD800]));
    assert_eq!(Value::Num(1.0).as_text(), None);
}

/// Script and module source supplied by the host is Rust text: a private-use character written
/// directly in a string literal or template is that character, while eval still parses an
/// engine string (in which a lone surrogate stays lone).
#[test]
fn host_source_text_keeps_plane_16_private_use_characters() {
    for tier in TIERS {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        let script = format!(
            "{UNITS}\nvar literal = '\u{10FFFD}', template = `\u{10F800}`;\n\
             var direct = [literal.length, units(literal), units(template), \
             units(eval(\"'\" + '\\uDBFF' + \"'\")), \
             units(eval(\"'\" + literal + \"'\"))].join('|');\n\
             console.log(literal + ':' + '\\uD800');\ndirect"
        );
        match engine.eval(&script, false) {
            Ok(Completion::Value(value)) => assert_eq!(
                value,
                "2|dbff dffd|dbfe dc00|dbff|dbff dffd",
                "{tier:?} script"
            ),
            _ => panic!("{tier:?}: script failed"),
        }
        assert_eq!(engine.take_console(), ["\u{10FFFD}:\u{FFFD}"], "{tier:?}");
        match engine.eval("literal", false) {
            Ok(Completion::Value(value)) => assert_eq!(value, "\u{10FFFD}", "{tier:?} render"),
            _ => panic!("{tier:?}: render failed"),
        }
        let module = "globalThis.fromModule = '\u{10FFFF}'.length + ':' + '\u{10FFFF}'.charCodeAt(1).toString(16);";
        assert!(matches!(
            engine.eval_module(module, "/plane16.mjs", |_, _| None),
            Ok(Completion::Value(_))
        ));
        match engine.eval("fromModule", false) {
            Ok(Completion::Value(value)) => assert_eq!(value, "2:dfff", "{tier:?} module"),
            _ => panic!("{tier:?}: module result failed"),
        }
        let snapshot = crate::compile_host_snapshot("globalThis.fromSnapshot = '\u{10FFFE}';")
            .expect("snapshot compiles");
        assert!(engine.eval_snapshot(&snapshot, false).is_ok());
        match engine.eval("fromSnapshot.length + ':' + units(fromSnapshot)", false) {
            Ok(Completion::Value(value)) => {
                assert_eq!(value, "2:dbff dffe", "{tier:?} snapshot")
            }
            _ => panic!("{tier:?}: snapshot result failed"),
        }
    }
}
