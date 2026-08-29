//! `Intl.Segmenter` using the Unicode 17.0 UAX #29 default grapheme, word, and sentence rules.

use super::service::{
    brand_slot, get_option, install_supported_locales, instance_proto, read_locale_matcher,
    resolve_locale,
};
use super::{
    ab, arg, canonicalize_locale_list, get_options_object as coerce_options, make_service,
};
use crate::interpreter::Interp;
use crate::value::{set_builtin, set_data, Gc, Value};

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "Segmenter", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "segment", 1, |i, this, a| {
        segment(i, &this, &arg(a, 0))
    });
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.Segmenter requires 'new'"));
    }
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;
    let granularity = get_option(
        i,
        &options,
        "granularity",
        &["grapheme", "word", "sentence"],
        Some("grapheme"),
    )?
    .unwrap();
    let resolved = resolve_locale(i, &requested, &[]);
    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.Segmenter")? {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__sg", Value::Bool(true));
    set_builtin(&obj, "__sg_locale", Value::from_string(resolved.locale));
    set_builtin(&obj, "__sg_granularity", Value::from_string(granularity));
    Ok(Value::Obj(obj))
}

#[derive(Clone, Copy)]
struct CodePoint {
    offset: usize,
    value: u32,
}

fn decode_utf16(input: &[u16]) -> Vec<CodePoint> {
    let mut code_points = Vec::with_capacity(input.len());
    let mut offset = 0;
    while offset < input.len() {
        let first = input[offset];
        if (0xD800..=0xDBFF).contains(&first)
            && offset + 1 < input.len()
            && (0xDC00..=0xDFFF).contains(&input[offset + 1])
        {
            code_points.push(CodePoint {
                offset,
                value: 0x10000
                    + (((first as u32 - 0xD800) << 10) | (input[offset + 1] as u32 - 0xDC00)),
            });
            offset += 2;
        } else {
            // ECMAScript strings may contain an unpaired surrogate. It has no UAX #29 property
            // and therefore follows each algorithm's `Other` fallback as one code unit.
            code_points.push(CodePoint {
                offset,
                value: first as u32,
            });
            offset += 1;
        }
    }
    code_points
}

fn in_ranges(r: Option<&'static [(u32, u32)]>, cp: u32) -> bool {
    match r {
        Some(ranges) => ranges
            .binary_search_by(|&(lo, hi)| {
                if cp < lo {
                    std::cmp::Ordering::Greater
                } else if cp > hi {
                    std::cmp::Ordering::Less
                } else {
                    std::cmp::Ordering::Equal
                }
            })
            .is_ok(),
        None => false,
    }
}

fn grapheme_gb9c(
    properties: &[crate::unicode_segment::IndicConjunctBreak],
    boundary: usize,
) -> bool {
    use crate::unicode_segment::IndicConjunctBreak::{Consonant, Extend, Linker};
    if properties[boundary] != Consonant {
        return false;
    }
    let mut index = boundary;
    let mut saw_linker = false;
    while index > 0 {
        index -= 1;
        match properties[index] {
            Extend => {}
            Linker => saw_linker = true,
            Consonant => return saw_linker,
            _ => return false,
        }
    }
    false
}

/// UAX #29 extended grapheme cluster boundaries (GB1–GB999), in UTF-16 code units.
#[allow(clippy::if_same_then_else)]
fn grapheme_boundaries(s: &[u16]) -> Vec<(usize, bool)> {
    use crate::unicode_segment::GraphemeBreak as G;
    let code_points = decode_utf16(s);
    if code_points.is_empty() {
        return Vec::new();
    }
    let properties: Vec<G> = code_points
        .iter()
        .map(|point| crate::unicode_segment::grapheme_break(point.value))
        .collect();
    let conjunct: Vec<_> = code_points
        .iter()
        .map(|point| crate::unicode_segment::indic_conjunct_break(point.value))
        .collect();
    let mut output = vec![(0, false)];
    let mut regional_indicators = usize::from(properties[0] == G::RegionalIndicator);
    for boundary in 1..code_points.len() {
        let a = properties[boundary - 1];
        let b = properties[boundary];
        let no_break = if a == G::Cr && b == G::Lf {
            true // GB3
        } else if matches!(a, G::Control | G::Cr | G::Lf) || matches!(b, G::Control | G::Cr | G::Lf)
        {
            false // GB4 / GB5
        } else if a == G::L && matches!(b, G::L | G::V | G::Lv | G::Lvt) {
            true // GB6
        } else if matches!(a, G::Lv | G::V) && matches!(b, G::V | G::T) {
            true // GB7
        } else if matches!(a, G::Lvt | G::T) && b == G::T {
            true // GB8
        } else if matches!(b, G::Extend | G::Zwj) {
            true // GB9
        } else if b == G::Spacingmark {
            true // GB9a
        } else if a == G::Prepend {
            true // GB9b
        } else if grapheme_gb9c(&conjunct, boundary) {
            true // GB9c
        } else if a == G::Zwj
            && crate::unicode_segment::is_extended_pictographic(code_points[boundary].value)
        {
            // GB11: Extended_Pictographic Extend* ZWJ × Extended_Pictographic.
            let mut index = boundary - 1;
            while index > 0 && properties[index - 1] == G::Extend {
                index -= 1;
            }
            index > 0
                && crate::unicode_segment::is_extended_pictographic(code_points[index - 1].value)
        } else {
            a == G::RegionalIndicator && b == G::RegionalIndicator && regional_indicators % 2 == 1
            // GB12/13
        };
        if !no_break {
            output.push((code_points[boundary].offset, false));
        }
        regional_indicators = if b == G::RegionalIndicator {
            if a == G::RegionalIndicator {
                regional_indicators + 1
            } else {
                1
            }
        } else {
            0
        };
    }
    output
}

fn word_ignored(property: crate::unicode_segment::WordBreak) -> bool {
    use crate::unicode_segment::WordBreak as W;
    matches!(property, W::Extend | W::Format | W::Zwj)
}

fn word_newline(property: crate::unicode_segment::WordBreak) -> bool {
    use crate::unicode_segment::WordBreak as W;
    matches!(property, W::Cr | W::Lf | W::Newline)
}

fn word_previous(properties: &[crate::unicode_segment::WordBreak], before: usize) -> Option<usize> {
    if before == 0 {
        return None;
    }
    let mut index = before - 1;
    while word_ignored(properties[index]) {
        if index == 0 || word_newline(properties[index - 1]) {
            return Some(index);
        }
        index -= 1;
    }
    Some(index)
}

fn word_next(properties: &[crate::unicode_segment::WordBreak], after: usize) -> Option<usize> {
    let mut index = after + 1;
    while index < properties.len() && word_ignored(properties[index]) {
        index += 1;
    }
    (index < properties.len()).then_some(index)
}

fn word_ah_letter(property: crate::unicode_segment::WordBreak) -> bool {
    use crate::unicode_segment::WordBreak as W;
    matches!(property, W::Aletter | W::HebrewLetter)
}

fn word_mid_letter(property: crate::unicode_segment::WordBreak) -> bool {
    use crate::unicode_segment::WordBreak as W;
    matches!(property, W::Midletter | W::Midnumlet | W::SingleQuote)
}

fn word_mid_number(property: crate::unicode_segment::WordBreak) -> bool {
    use crate::unicode_segment::WordBreak as W;
    matches!(property, W::Midnum | W::Midnumlet | W::SingleQuote)
}

fn word_boundary(
    code_points: &[CodePoint],
    properties: &[crate::unicode_segment::WordBreak],
    boundary: usize,
) -> bool {
    use crate::unicode_segment::WordBreak as W;
    let raw_left = properties[boundary - 1];
    let right = properties[boundary];
    if raw_left == W::Cr && right == W::Lf {
        return false; // WB3
    }
    if word_newline(raw_left) || word_newline(right) {
        return true; // WB3a/WB3b
    }
    if raw_left == W::Zwj
        && crate::unicode_segment::is_extended_pictographic(code_points[boundary].value)
    {
        return false; // WB3c
    }
    if raw_left == W::Wsegspace && right == W::Wsegspace {
        return false; // WB3d
    }
    if word_ignored(right) {
        return false; // WB4
    }
    let Some(left_index) = word_previous(properties, boundary) else {
        return true;
    };
    let left = if word_ignored(properties[left_index]) {
        W::Other
    } else {
        properties[left_index]
    };
    if word_ah_letter(left) && word_ah_letter(right) {
        return false; // WB5
    }
    let next = word_next(properties, boundary).map(|index| properties[index]);
    if word_ah_letter(left) && word_mid_letter(right) && next.is_some_and(word_ah_letter) {
        return false; // WB6
    }
    let previous = word_previous(properties, left_index).map(|index| properties[index]);
    if word_mid_letter(left) && word_ah_letter(right) && previous.is_some_and(word_ah_letter) {
        return false; // WB7
    }
    if left == W::HebrewLetter && right == W::SingleQuote {
        return false; // WB7a
    }
    if left == W::HebrewLetter && right == W::DoubleQuote && next == Some(W::HebrewLetter) {
        return false; // WB7b
    }
    if left == W::DoubleQuote && right == W::HebrewLetter && previous == Some(W::HebrewLetter) {
        return false; // WB7c
    }
    if left == W::Numeric && right == W::Numeric {
        return false; // WB8
    }
    if word_ah_letter(left) && right == W::Numeric {
        return false; // WB9
    }
    if left == W::Numeric && word_ah_letter(right) {
        return false; // WB10
    }
    if word_mid_number(left) && right == W::Numeric && previous == Some(W::Numeric) {
        return false; // WB11
    }
    if left == W::Numeric && word_mid_number(right) && next == Some(W::Numeric) {
        return false; // WB12
    }
    if left == W::Katakana && right == W::Katakana {
        return false; // WB13
    }
    if matches!(
        left,
        W::Aletter | W::HebrewLetter | W::Numeric | W::Katakana | W::Extendnumlet
    ) && right == W::Extendnumlet
    {
        return false; // WB13a
    }
    if left == W::Extendnumlet
        && matches!(
            right,
            W::Aletter | W::HebrewLetter | W::Numeric | W::Katakana
        )
    {
        return false; // WB13b
    }
    if left == W::RegionalIndicator && right == W::RegionalIndicator {
        let mut count = 0;
        let mut cursor = Some(left_index);
        while let Some(index) = cursor {
            if properties[index] != W::RegionalIndicator {
                break;
            }
            count += 1;
            cursor = word_previous(properties, index);
        }
        if count % 2 == 1 {
            return false; // WB15/WB16
        }
    }
    true // WB999
}

fn word_like(
    code_points: &[CodePoint],
    properties: &[crate::unicode_segment::WordBreak],
    alphabetic: Option<&'static [(u32, u32)]>,
    ideographic: Option<&'static [(u32, u32)]>,
) -> bool {
    use crate::unicode_segment::WordBreak as W;
    code_points.iter().zip(properties).any(|(point, property)| {
        matches!(
            property,
            W::Aletter | W::HebrewLetter | W::Numeric | W::Katakana | W::Extendnumlet
        ) || in_ranges(alphabetic, point.value)
            || in_ranges(ideographic, point.value)
    })
}

fn word_boundaries(input: &[u16]) -> Vec<(usize, bool)> {
    let code_points = decode_utf16(input);
    if code_points.is_empty() {
        return Vec::new();
    }
    let properties: Vec<_> = code_points
        .iter()
        .map(|point| crate::unicode_segment::word_break(point.value))
        .collect();
    // Resolve the generated binary-property tables once for the entire string. `lookup` performs
    // loose-name canonicalization, so doing it per code point would allocate on the hot path.
    let alphabetic = crate::unicode_props::lookup("alphabetic", None);
    let ideographic = crate::unicode_props::lookup("ideographic", None);
    let mut starts = vec![0usize];
    for boundary in 1..code_points.len() {
        if word_boundary(&code_points, &properties, boundary) {
            starts.push(boundary);
        }
    }
    starts
        .iter()
        .enumerate()
        .map(|(index, &start)| {
            let end = starts.get(index + 1).copied().unwrap_or(code_points.len());
            (
                code_points[start].offset,
                word_like(
                    &code_points[start..end],
                    &properties[start..end],
                    alphabetic,
                    ideographic,
                ),
            )
        })
        .collect()
}

fn sentence_ignored(property: crate::unicode_segment::SentenceBreak) -> bool {
    use crate::unicode_segment::SentenceBreak as S;
    matches!(property, S::Extend | S::Format)
}

fn sentence_para(property: crate::unicode_segment::SentenceBreak) -> bool {
    use crate::unicode_segment::SentenceBreak as S;
    matches!(property, S::Sep | S::Cr | S::Lf)
}

fn sentence_previous(
    properties: &[crate::unicode_segment::SentenceBreak],
    before: usize,
) -> Option<usize> {
    let mut index = before.checked_sub(1)?;
    while sentence_ignored(properties[index]) {
        if index == 0 || sentence_para(properties[index - 1]) {
            return Some(index);
        }
        index -= 1;
    }
    Some(index)
}

fn sentence_skip_ignored(
    properties: &[crate::unicode_segment::SentenceBreak],
    mut index: usize,
) -> usize {
    while index < properties.len() && sentence_ignored(properties[index]) {
        index += 1;
    }
    index
}

fn sentence_boundaries(input: &[u16]) -> Vec<(usize, bool)> {
    use crate::unicode_segment::SentenceBreak as S;
    let code_points = decode_utf16(input);
    if code_points.is_empty() {
        return Vec::new();
    }
    let properties: Vec<_> = code_points
        .iter()
        .map(|point| crate::unicode_segment::sentence_break(point.value))
        .collect();
    let mut starts = vec![0usize];
    let mut index = 0;
    while index < properties.len() {
        let property = properties[index];
        if property == S::Cr && properties.get(index + 1) == Some(&S::Lf) {
            if index + 2 < code_points.len() {
                starts.push(index + 2); // SB3/SB4
            }
            index += 2;
            continue;
        }
        if sentence_para(property) {
            if index + 1 < code_points.len() {
                starts.push(index + 1); // SB4
            }
            index += 1;
            continue;
        }
        if !matches!(property, S::Aterm | S::Sterm) {
            index += 1;
            continue;
        }

        let next = sentence_skip_ignored(&properties, index + 1);
        let previous = sentence_previous(&properties, index);
        if property == S::Aterm && properties.get(next) == Some(&S::Numeric) {
            index += 1; // SB6
            continue;
        }
        if property == S::Aterm
            && previous.is_some_and(|p| matches!(properties[p], S::Upper | S::Lower))
            && properties.get(next) == Some(&S::Upper)
        {
            index += 1; // SB7
            continue;
        }

        let mut tail = next;
        while properties.get(tail) == Some(&S::Close) {
            tail = sentence_skip_ignored(&properties, tail + 1);
        }
        while properties.get(tail) == Some(&S::Sp) {
            tail = sentence_skip_ignored(&properties, tail + 1);
        }

        let mut suppress = false;
        if property == S::Aterm {
            let mut lookahead = tail;
            while let Some(candidate) = properties.get(lookahead) {
                if *candidate == S::Lower {
                    suppress = true; // SB8
                    break;
                }
                if matches!(
                    candidate,
                    S::Oletter | S::Upper | S::Cr | S::Lf | S::Sep | S::Aterm | S::Sterm
                ) {
                    break;
                }
                lookahead = sentence_skip_ignored(&properties, lookahead + 1);
            }
        }
        if matches!(
            properties.get(tail),
            Some(S::Scontinue | S::Aterm | S::Sterm)
        ) {
            suppress = true; // SB8a
        }
        if suppress {
            index += 1;
            continue;
        }

        // SB9–SB11 include Close*, Sp*, and one optional paragraph separator in this sentence.
        let mut end = tail;
        if properties.get(end) == Some(&S::Cr) && properties.get(end + 1) == Some(&S::Lf) {
            end += 2;
        } else if properties.get(end).is_some_and(|p| sentence_para(*p)) {
            end += 1;
        }
        if end < code_points.len() {
            starts.push(end);
        }
        index += 1;
    }
    starts.sort_unstable();
    starts.dedup();
    starts
        .into_iter()
        .map(|start| (code_points[start].offset, false))
        .collect()
}

fn boundaries(s: &[u16], granularity: &str) -> Vec<(usize, bool)> {
    match granularity {
        "grapheme" => grapheme_boundaries(s),
        "word" => word_boundaries(s),
        _ => sentence_boundaries(s),
    }
}

fn segment(i: &mut Interp, this: &Value, input: &Value) -> Result<Value, Value> {
    let o = brand_slot(i, this, "__sg")?;
    let granularity = match o.borrow().props.get("__sg_granularity").map(|p| p.value()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "grapheme".to_string(),
    };
    let s = ab(i.to_string(input))?.to_string();
    // jstr units: lone surrogates must round-trip (each is its own single-unit segment).
    let units: Vec<u16> = crate::jstr::units(&s);
    let bnds = boundaries(&units, &granularity);

    // Build an array-like "Segments" object that is iterable and has a `containing` method. We
    // pre-materialise the segment records.
    let segments = i.new_object();
    let mut records: Vec<Value> = Vec::new();
    for (k, &(start, wordlike)) in bnds.iter().enumerate() {
        let end = bnds.get(k + 1).map(|&(e, _)| e).unwrap_or(units.len());
        let seg: String = crate::jstr::from_units(&units[start..end]);
        let rec = i.new_object();
        set_data(&rec, "segment", Value::from_string(seg));
        set_data(&rec, "index", Value::Num(start as f64));
        set_data(&rec, "input", Value::from_string(s.clone()));
        if granularity == "word" {
            set_data(&rec, "isWordLike", Value::Bool(wordlike));
        }
        records.push(Value::Obj(rec));
    }
    // Make `segments` iterable by giving it a @@iterator returning an array iterator over records.
    let arr = i.make_array(records);
    set_builtin(&segments, "__seg_records", arr.clone());
    if let Some(sym) = i.iterator_sym.clone() {
        let f = i.make_native("[Symbol.iterator]", 0, |i, this, _| {
            let recs = ab(i.get_member(&this, "__seg_records"))?;
            let itf = ab(i.get_member(&recs, "values"))?;
            ab(i.call(itf, recs, &[]))
        });
        segments.borrow_mut().props.insert(
            crate::interpreter::Interp::sym_key(&sym),
            crate::value::Property::builtin(Value::Obj(f)),
        );
    }
    it_containing(i, &segments);
    Ok(Value::Obj(segments))
}

fn it_containing(i: &mut Interp, segments: &Gc) {
    let f = i.make_native("containing", 1, |i, this, a| {
        let number = ab(i.to_number(&arg(a, 0)))?;
        let idx = if number.is_nan() { 0.0 } else { number.trunc() };
        if idx < 0.0 || idx == f64::INFINITY {
            return Ok(Value::Undefined);
        }
        let recs = ab(i.get_member(&this, "__seg_records"))?;
        let len = ab(i.get_member(&recs, "length"))?;
        let len = ab(i.to_number(&len))? as usize;
        // Segment starts are strictly increasing. Find the first start greater than `idx`, then
        // inspect its predecessor: O(log segments) instead of the old full-record scan.
        let mut low = 0usize;
        let mut high = len;
        while low < high {
            let middle = low + (high - low) / 2;
            let record = ab(i.get_member(&recs, &middle.to_string()))?;
            let start = ab(i.get_member(&record, "index"))?;
            let start = ab(i.to_number(&start))?;
            if start <= idx {
                low = middle + 1;
            } else {
                high = middle;
            }
        }
        if low == 0 {
            return Ok(Value::Undefined);
        }
        let record = ab(i.get_member(&recs, &(low - 1).to_string()))?;
        let segment = ab(i.get_member(&record, "segment"))?;
        let start = ab(i.get_member(&record, "index"))?;
        let start = ab(i.to_number(&start))?;
        let length = match &segment {
            Value::Str(value) => crate::jstr::unit_len(value) as f64,
            _ => 0.0,
        };
        if idx < start + length {
            return Ok(record);
        }
        Ok(Value::Undefined)
    });
    set_builtin(segments, "containing", Value::Obj(f));
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__sg")?;
    let get = |k: &str| {
        o.borrow()
            .props
            .get(k)
            .map(|p| p.value())
            .unwrap_or(Value::Undefined)
    };
    let res = i.new_object();
    set_data(&res, "locale", get("__sg_locale"));
    set_data(&res, "granularity", get("__sg_granularity"));
    Ok(Value::Obj(res))
}

#[cfg(test)]
mod tests {
    use super::boundaries;

    const GRAPHEME_TESTS: &str = include_str!("../../tests/unicode-17.0.0/GraphemeBreakTest.txt");
    const WORD_TESTS: &str = include_str!("../../tests/unicode-17.0.0/WordBreakTest.txt");
    const SENTENCE_TESTS: &str = include_str!("../../tests/unicode-17.0.0/SentenceBreakTest.txt");

    fn parse_case(line: &str) -> Option<(Vec<u16>, Vec<usize>)> {
        let body = line.split('#').next()?.trim();
        if body.is_empty() {
            return None;
        }
        let mut input = Vec::new();
        let mut expected = Vec::new();
        for token in body.split_ascii_whitespace() {
            match token {
                "÷" => expected.push(input.len()),
                "×" => {}
                code_point => {
                    let value = u32::from_str_radix(code_point, 16).ok()?;
                    let character = char::from_u32(value)?;
                    let mut units = [0; 2];
                    input.extend_from_slice(character.encode_utf16(&mut units));
                }
            }
        }
        if expected.last() == Some(&input.len()) {
            expected.pop();
        }
        Some((input, expected))
    }

    fn assert_break_test(data: &str, granularity: &str) {
        for (line_number, line) in data.lines().enumerate() {
            let Some((input, expected)) = parse_case(line) else {
                continue;
            };
            let actual: Vec<usize> = boundaries(&input, granularity)
                .into_iter()
                .map(|(offset, _)| offset)
                .collect();
            assert_eq!(
                actual,
                expected,
                "Unicode 17.0 {granularity} break test line {}: {line}",
                line_number + 1
            );
        }
    }

    #[test]
    fn unicode_17_grapheme_break_conformance() {
        assert_break_test(GRAPHEME_TESTS, "grapheme");
    }

    #[test]
    fn unicode_17_word_break_conformance() {
        assert_break_test(WORD_TESTS, "word");
    }

    #[test]
    fn unicode_17_sentence_break_conformance() {
        assert_break_test(SENTENCE_TESTS, "sentence");
    }
}
