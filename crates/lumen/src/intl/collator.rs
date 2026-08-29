//! `Intl.Collator`: Unicode 17 UCA root weights with CLDR 48 locale tailorings.

use super::service::{
    brand_slot, get_option, install_supported_locales, instance_proto, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_builtin, set_data, Gc, Value};

const COLLATIONS: [&str; 12] = [
    "compat", "dict", "emoji", "eor", "phonebk", "phonetic", "pinyin", "searchjl", "stroke",
    "trad", "unihan", "zhuyin",
];

/// AvailableCollations: exactly the collation identifiers for which at least one locale's
/// Collator resolves the requested functionality. The table is already code-unit sorted.
pub(super) fn available_collations() -> &'static [&'static str] {
    &COLLATIONS
}

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "Collator", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
    install_compare_getter(it, &proto);
}

fn install_compare_getter(it: &mut Interp, proto: &Gc) {
    let g = it.make_native("get compare", 0, |i, this, _| {
        let o = brand_slot(i, &this, "__co")?;
        if let Some(f) = o.borrow().props.get("__co_bound").map(|p| p.value()) {
            return Ok(f);
        }
        let f = i.make_native("", 2, |i, that, a| {
            compare(i, &that, &arg(a, 0), &arg(a, 1))
        });
        let bound = crate::intl::numberformat::bind_this(i, Value::Obj(f), this.clone());
        set_builtin(&o, "__co_bound", bound.clone());
        Ok(bound)
    });
    proto.borrow_mut().props.insert(
        "compare",
        crate::value::Property::accessor_prop(Some(Value::Obj(g)), None, false, true),
    );
}

fn construct(i: &mut Interp, _t: Value, a: &[Value]) -> Result<Value, Value> {
    // Legacy service: callable without `new` (returns a fresh instance either way).
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    let usage = get_option(i, &options, "usage", &["sort", "search"], Some("sort"))?.unwrap();
    read_locale_matcher(i, &options)?;
    let collation_opt = get_option(i, &options, "collation", &[], None)?;
    let numeric = {
        let v = ab(i.get_member(&options, "numeric"))?;
        if matches!(v, Value::Undefined) {
            None
        } else {
            Some(i.to_boolean(&v))
        }
    };
    let case_first = get_option(i, &options, "caseFirst", &["upper", "lower", "false"], None)?;
    let sensitivity = get_option(
        i,
        &options,
        "sensitivity",
        &["base", "accent", "case", "variant"],
        Some("variant"),
    )?
    .unwrap();
    let ignore_punct_opt = {
        let v = ab(i.get_member(&options, "ignorePunctuation"))?;
        if matches!(v, Value::Undefined) {
            None
        } else {
            Some(i.to_boolean(&v))
        }
    };
    let resolved = resolve_locale(i, &requested, &["co", "kn", "kf"]);
    // ignorePunctuation defaults per locale: the dictionary-ordered locales (Thai) default to true.
    let ignore_punct =
        ignore_punct_opt.unwrap_or_else(|| resolved.locale.split('-').next() == Some("th"));
    let kw = |k: &str| {
        resolved
            .keywords
            .iter()
            .find(|(kk, _)| kk == k)
            .map(|(_, v)| v.clone())
    };
    let numeric_opt = numeric;
    let case_first_opt = case_first.clone();
    // The option wins over the locale's -u- keyword; a bare -u-kn (empty value) means numeric=true.
    let numeric = numeric_opt.unwrap_or_else(|| match kw("kn") {
        Some(v) => v != "false",
        None => false,
    });
    let case_first = case_first_opt
        .clone()
        .or_else(|| kw("kf"))
        .unwrap_or_else(|| "false".to_string());
    // The `-u-co-` value must be a known collation type (never the reserved standard/search); an
    // unknown one falls back to "default".
    // A collation is *supported* per locale (phonebk for German; eor broadly). The `collation`
    // option overrides the -u-co extension when supported; an unsupported value is ignored.
    let res_lang = resolved
        .locale
        .split('-')
        .next()
        .unwrap_or("en")
        .to_string();
    let supported = |c: &str| supported_collation(&res_lang, c);
    let opt_co = collation_opt
        .filter(|c| COLLATIONS.contains(&c.as_str()) && supported(c))
        .filter(|_| usage == "sort");
    let ext_co = kw("co").filter(|c| COLLATIONS.contains(&c.as_str()) && supported(c));
    let collation = opt_co
        .clone()
        .or_else(|| ext_co.clone())
        .unwrap_or_else(|| "default".to_string());

    // ResolveLocale: reflect the surviving `-u-` keywords in the resolved locale string. A keyword
    // survives when its value came from the locale extension and no differing option overrode it
    // (keys are emitted in alphabetical order: co, kf, kn).
    let mut additions: Vec<(&str, String)> = Vec::new();
    if let Some(co) = ext_co {
        if opt_co.as_ref().is_none_or(|o| *o == co) {
            additions.push(("co", co));
        }
    }
    if let Some(kf) = kw("kf") {
        if ["upper", "lower", "false"].contains(&kf.as_str())
            && case_first_opt.as_deref().is_none_or(|o| o == kf)
        {
            additions.push(("kf", kf));
        }
    }
    if let Some(kn) = kw("kn") {
        let kn_bool = kn != "false";
        if numeric_opt.is_none_or(|o| o == kn_bool) {
            // Canonical form: the `true` value is elided (`-u-kn`), `false` is spelled out.
            additions.push((
                "kn",
                if kn_bool {
                    String::new()
                } else {
                    "false".to_string()
                },
            ));
        }
    }
    let locale = if additions.is_empty() {
        resolved.locale.clone()
    } else {
        let ext: String = additions
            .iter()
            .map(|(k, v)| {
                if v.is_empty() {
                    format!("-{k}")
                } else {
                    format!("-{k}-{v}")
                }
            })
            .collect();
        format!("{}-u{}", resolved.locale, ext)
    };

    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.Collator")? {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__co", Value::Bool(true));
    set_builtin(&obj, "__co_locale", Value::from_string(locale));
    set_builtin(&obj, "__co_usage", Value::from_string(usage));
    set_builtin(&obj, "__co_sensitivity", Value::from_string(sensitivity));
    set_builtin(&obj, "__co_ignorepunct", Value::Bool(ignore_punct));
    set_builtin(&obj, "__co_numeric", Value::Bool(numeric));
    set_builtin(&obj, "__co_collation", Value::from_string(collation));
    set_builtin(&obj, "__co_casefirst", Value::from_string(case_first));
    Ok(Value::Obj(obj))
}

fn compare(i: &mut Interp, this: &Value, a: &Value, b: &Value) -> Result<Value, Value> {
    let o = brand_slot(i, this, "__co")?;
    let get = |k: &str| match o.borrow().props.get(k).map(|p| p.value()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => String::new(),
    };
    let getb = |k: &str| {
        matches!(
            o.borrow().props.get(k).map(|p| p.value()),
            Some(Value::Bool(true))
        )
    };
    let sa = ab(i.to_string(a))?.to_string();
    let sb = ab(i.to_string(b))?.to_string();
    let opts = CollateOpts {
        locale: get("__co_locale"),
        collation: get("__co_collation"),
        sensitivity: get("__co_sensitivity"),
        numeric: getb("__co_numeric"),
        ignore_punct: getb("__co_ignorepunct"),
        upper_first: get("__co_casefirst") == "upper",
        // German ä/ö/ü expand to ae/oe/ue under the phonebook collation and in search usage.
        expand_umlaut: {
            let lang = get("__co_locale");
            let lang = lang.split('-').next().unwrap_or("");
            lang == "de" && (get("__co_collation") == "phonebk" || get("__co_usage") == "search")
        },
    };
    let ord = match collate(&sa, &sb, &opts) {
        std::cmp::Ordering::Less => -1.0,
        std::cmp::Ordering::Greater => 1.0,
        std::cmp::Ordering::Equal => 0.0,
    };
    Ok(Value::Num(ord))
}

struct CollateOpts {
    locale: String,
    collation: String,
    sensitivity: String,
    numeric: bool,
    ignore_punct: bool,
    upper_first: bool,
    expand_umlaut: bool,
}

struct CollationElements {
    elements: Vec<crate::unicode_collation::Element>,
    case: Vec<bool>,
}

fn normalized_input(s: &str, opts: &CollateOpts) -> Vec<u32> {
    let cps = crate::jstr::code_points(s);
    let nfd = crate::unicode_norm_impl::decompose(&cps, false);
    if !opts.expand_umlaut {
        return nfd;
    }
    let mut output = Vec::with_capacity(nfd.len());
    let mut index = 0;
    while index < nfd.len() {
        let code_point = nfd[index];
        output.push(code_point);
        if matches!(code_point, 0x41 | 0x4f | 0x55 | 0x61 | 0x6f | 0x75)
            && nfd.get(index + 1) == Some(&0x308)
        {
            // CLDR de-phonebook: AE/ae << Ä/ä, OE/oe << Ö/ö, UE/ue << Ü/ü. Keep the
            // diaeresis as a secondary element after the primary expansion.
            output.push(if matches!(code_point, 0x41 | 0x4f | 0x55) {
                'E' as u32
            } else {
                'e' as u32
            });
            output.push(0x308);
            index += 2;
            continue;
        }
        index += 1;
    }
    output
}

fn append_numeric_run(
    input: &[u32],
    at: usize,
    output: &mut Vec<crate::unicode_collation::Element>,
) -> Option<usize> {
    if !(0x30..=0x39).contains(input.get(at)?) {
        return None;
    }
    let mut end = at;
    while input.get(end).is_some_and(|cp| (0x30..=0x39).contains(cp)) {
        end += 1;
    }
    let significant = input[at..end]
        .iter()
        .position(|cp| *cp != 0x30)
        .map(|offset| &input[at + offset..end])
        .unwrap_or(&input[end - 1..end]);
    let mut push = |primary: u64| {
        output.push(crate::unicode_collation::Element {
            primary,
            secondary: 0x20,
            tertiary: 2,
            variable: false,
        });
    };
    // A marker at the DUCET digit position, followed by length and digit pseudo-weights, makes
    // arbitrary-size digit runs compare by mathematical value without integer conversion.
    push(0x21e6_0000_0000);
    push(0x1_0000_0000_0000 + significant.len() as u64);
    for digit in significant {
        push(0x2_0000_0000_0000 + u64::from(digit - 0x30));
    }
    Some(end - at)
}

fn tailoring_fold(code_point: u32) -> u32 {
    char::from_u32(code_point)
        .and_then(|character| character.to_lowercase().next())
        .map_or(code_point, |character| character as u32)
}

fn append_tailored_if(
    input: &[u32],
    at: usize,
    source: &[u32],
    anchor: u32,
    primary_rank: u32,
    secondary_rank: u16,
    output: &mut Vec<crate::unicode_collation::Element>,
) -> Option<usize> {
    let candidate = input.get(at..at + source.len())?;
    if !candidate
        .iter()
        .zip(source)
        .all(|(actual, expected)| tailoring_fold(*actual) == *expected)
    {
        return None;
    }
    let uppercase = candidate
        .iter()
        .filter_map(|code_point| char::from_u32(*code_point))
        .find(|character| character.is_alphabetic())
        .is_some_and(char::is_uppercase);
    let anchor = crate::unicode_collation::first_primary(anchor)
        .expect("CLDR tailoring reset anchors are present in DUCET");
    output.push(crate::unicode_collation::Element {
        primary: anchor + u64::from(primary_rank),
        secondary: 0x20 + secondary_rank,
        tertiary: if uppercase { 8 } else { 2 },
        variable: false,
    });
    Some(source.len())
}

/// Apply the compact CLDR 48 rules whose relations differ from the root collation for Lumen's
/// advertised locales. Sources are NFD because both UTS #10 and the LDML tailoring syntax operate
/// on canonically decomposed input. Longest contractions are tested before singleton relations.
fn append_cldr_tailoring(
    input: &[u32],
    at: usize,
    opts: &CollateOpts,
    output: &mut Vec<crate::unicode_collation::Element>,
) -> Option<usize> {
    let language = opts.locale.split('-').next().unwrap_or("en");
    let collation = opts.collation.as_str();

    // CLDR's large `<*...` Japanese and Chinese primary chains are generated into a compact
    // code-point/rank map. The reset is `[last regular]`, immediately before implicit Han.
    let han_order = match (language, collation) {
        ("ja", "default") => Some(crate::cldr_collation::HanOrder::Japanese),
        ("zh", "pinyin") => Some(crate::cldr_collation::HanOrder::Pinyin),
        ("zh", "stroke") => Some(crate::cldr_collation::HanOrder::Stroke),
        ("zh", "zhuyin") => Some(crate::cldr_collation::HanOrder::Zhuyin),
        ("zh", "default") if opts.locale.split('-').any(|subtag| subtag == "Hant") => {
            Some(crate::cldr_collation::HanOrder::Stroke)
        }
        ("zh", "default") => Some(crate::cldr_collation::HanOrder::Pinyin),
        _ => None,
    };
    if let Some(rank) =
        han_order.and_then(|order| crate::cldr_collation::primary_rank(order, *input.get(at)?))
    {
        output.push(crate::unicode_collation::Element {
            primary: 0xfa00_0000_0000 + u64::from(rank),
            secondary: 0x20,
            tertiary: 2,
            variable: false,
        });
        return Some(1);
    }

    let mut rule = |source: &[u32], anchor, primary, secondary| {
        append_tailored_if(input, at, source, anchor, primary, secondary, output)
    };

    // es.xml: &N<n-tilde; traditional additionally has C<ch and L<ll.
    if language == "es" {
        if collation == "trad" {
            if let Some(length) = rule(&[0x63, 0x68], 'c' as u32, 1, 0) {
                return Some(length);
            }
            if let Some(length) = rule(&[0x6c, 0x6c], 'l' as u32, 1, 0) {
                return Some(length);
            }
        }
        if let Some(length) = rule(&[0x6e, 0x303], 'n' as u32, 1, 0) {
            return Some(length);
        }
    }

    // ln.xml phonetic digraphs. Equal-primary case variants are handled by the case level.
    if language == "ln" && collation == "phonetic" {
        const DIGRAPHS: &[(&[u32], u32, u32)] = &[
            (&[0x6e, 0x67, 0x62], 0x6e, 3),
            (&[0x67, 0x62], 0x67, 1),
            (&[0x6b, 0x70], 0x6b, 1),
            (&[0x6d, 0x62], 0x6d, 1),
            (&[0x6d, 0x66], 0x6d, 2),
            (&[0x6d, 0x70], 0x6d, 3),
            (&[0x6d, 0x76], 0x6d, 4),
            (&[0x6e, 0x64], 0x6e, 1),
            (&[0x6e, 0x67], 0x6e, 2),
            (&[0x6e, 0x6b], 0x6e, 4),
            (&[0x6e, 0x73], 0x6e, 5),
            (&[0x6e, 0x74], 0x6e, 6),
            (&[0x6e, 0x79], 0x6e, 7),
            (&[0x6e, 0x7a], 0x6e, 8),
            (&[0x73, 0x68], 0x73, 1),
            (&[0x74, 0x73], 0x74, 1),
        ];
        for &(source, anchor, primary) in DIGRAPHS {
            if let Some(length) = rule(source, anchor, primary, 0) {
                return Some(length);
            }
        }
    }

    let rules: &[(&[u32], u32, u32, u16)] = match language {
        // pl.xml: each accented letter is a distinct primary after its base.
        "pl" => &[
            (&[0x61, 0x328], 0x61, 1, 0),
            (&[0x63, 0x301], 0x63, 1, 0),
            (&[0x65, 0x328], 0x65, 1, 0),
            (&[0x142], 0x6c, 1, 0),
            (&[0x6e, 0x301], 0x6e, 1, 0),
            (&[0x6f, 0x301], 0x6f, 1, 0),
            (&[0x73, 0x301], 0x73, 1, 0),
            (&[0x7a, 0x301], 0x7a, 1, 0),
            (&[0x7a, 0x307], 0x7a, 2, 0),
        ],
        // sl.xml: C<caron C<acute, D<stroke, S<caron, Z<caron.
        "sl" => &[
            (&[0x63, 0x30c], 0x63, 1, 0),
            (&[0x63, 0x301], 0x63, 2, 0),
            (&[0x111], 0x64, 1, 0),
            (&[0x73, 0x30c], 0x73, 1, 0),
            (&[0x7a, 0x30c], 0x7a, 1, 0),
        ],
        // sv.xml: the three primary groups after Z, plus its secondary-equivalent letters.
        "sv" => &[
            (&[0x61, 0x30a], 0x7a, 1, 0),
            (&[0x61, 0x308], 0x7a, 2, 0),
            (&[0xe6], 0x7a, 2, 1),
            (&[0x65, 0x328], 0x7a, 2, 2),
            (&[0x6f, 0x308], 0x7a, 3, 0),
            (&[0xf8], 0x7a, 3, 1),
            (&[0x6f, 0x30b], 0x7a, 3, 2),
            (&[0x153], 0x7a, 3, 3),
            (&[0x6f, 0x302], 0x7a, 3, 4),
            (&[0x111], 0x64, 0, 1),
            (&[0xf0], 0x64, 0, 2),
            (&[0xfe], 0x74, 0, 1),
            (&[0x75, 0x308], 0x79, 0, 1),
            (&[0x75, 0x30b], 0x79, 0, 2),
        ],
        // ln.xml standard relations; the phonetic rules above extend these.
        "ln" => &[(&[0x25b], 0x65, 1, 0), (&[0x254], 0x6f, 0, 1)],
        // hi.xml: OM < ANUSVARA << CANDRABINDU < VISARGA.
        "hi" => &[
            (&[0x902], 0x950, 1, 0),
            (&[0x901], 0x950, 1, 1),
            (&[0x903], 0x950, 2, 0),
        ],
        // si.xml: AU < ANUSVARAYA < VISARGAYA and JNYA < TAALUJA NAASIKYAYA.
        "si" => &[
            (&[0xd82], 0xd96, 1, 0),
            (&[0xd83], 0xd96, 2, 0),
            (&[0xda4], 0xda5, 1, 0),
        ],
        _ => &[],
    };
    for &(source, anchor, primary, secondary) in rules {
        if let Some(length) = rule(source, anchor, primary, secondary) {
            return Some(length);
        }
    }
    None
}

fn elements(s: &str, opts: &CollateOpts) -> CollationElements {
    let input = normalized_input(s, opts);
    let case = input
        .iter()
        .filter_map(|code_point| char::from_u32(*code_point))
        .filter(|character| character.is_uppercase() || character.is_lowercase())
        .map(char::is_uppercase)
        .collect();
    let mut elements = Vec::with_capacity(input.len());
    let mut at = 0;
    while at < input.len() {
        if opts.numeric {
            if let Some(length) = append_numeric_run(&input, at, &mut elements) {
                at += length;
                continue;
            }
        }
        if let Some(length) = append_cldr_tailoring(&input, at, opts, &mut elements) {
            at += length;
            continue;
        }
        #[cfg(test)]
        if opts.locale == "und" {
            at += crate::unicode_collation::append_ducet_mapping(&input, at, &mut elements);
            continue;
        }
        at += crate::unicode_collation::append_mapping(&input, at, &mut elements);
    }
    CollationElements { elements, case }
}

/// UTS #10 comparison: NFD, longest-match DUCET collation-element production, then successive
/// non-zero primary, secondary, and tertiary weights according to ECMA-402 sensitivity.
fn collate(a: &str, b: &str, opts: &CollateOpts) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let ea = elements(a, opts);
    let eb = elements(b, opts);
    let active =
        |element: &&crate::unicode_collation::Element| !(opts.ignore_punct && element.variable);
    let prim = ea
        .elements
        .iter()
        .filter(&active)
        .map(|element| element.primary)
        .filter(|weight| *weight != 0)
        .cmp(
            eb.elements
                .iter()
                .filter(&active)
                .map(|element| element.primary)
                .filter(|weight| *weight != 0),
        );
    if prim != Ordering::Equal {
        return prim;
    }
    let sens = opts.sensitivity.as_str();
    if sens == "accent" || sens == "variant" {
        let sec = ea
            .elements
            .iter()
            .filter(&active)
            .map(|element| element.secondary)
            .filter(|weight| *weight != 0)
            .cmp(
                eb.elements
                    .iter()
                    .filter(&active)
                    .map(|element| element.secondary)
                    .filter(|weight| *weight != 0),
            );
        if sec != Ordering::Equal {
            return sec;
        }
    }
    if sens == "case" || (sens == "variant" && opts.upper_first) {
        let case_weight = |upper: &bool| *upper != opts.upper_first;
        let case = ea
            .case
            .iter()
            .map(case_weight)
            .cmp(eb.case.iter().map(case_weight));
        if case != Ordering::Equal {
            return case;
        }
    }
    if sens == "variant" {
        let ter = ea
            .elements
            .iter()
            .filter(&active)
            .map(|element| element.tertiary)
            .filter(|weight| *weight != 0)
            .cmp(
                eb.elements
                    .iter()
                    .filter(&active)
                    .map(|element| element.tertiary)
                    .filter(|weight| *weight != 0),
            );
        if ter != Ordering::Equal {
            return ter;
        }
    }
    Ordering::Equal
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = brand_slot(i, &this, "__co")?;
    let get = |k: &str| {
        o.borrow()
            .props
            .get(k)
            .map(|p| p.value())
            .unwrap_or(Value::Undefined)
    };
    let res = i.new_object();
    set_data(&res, "locale", get("__co_locale"));
    set_data(&res, "usage", get("__co_usage"));
    set_data(&res, "sensitivity", get("__co_sensitivity"));
    set_data(&res, "ignorePunctuation", get("__co_ignorepunct"));
    set_data(&res, "collation", get("__co_collation"));
    set_data(&res, "numeric", get("__co_numeric"));
    set_data(&res, "caseFirst", get("__co_casefirst"));
    Ok(Value::Obj(res))
}

/// Whether a locale's Collator supports a collation type (mirrors the CLDR availability that
/// Intl.supportedValuesOf("collation") reflects). "eor"/"emoji" are available everywhere.
fn supported_collation(lang: &str, c: &str) -> bool {
    match c {
        "eor" | "emoji" => true,
        "phonebk" => lang == "de",
        "compat" => lang == "ar",
        "trad" => lang == "es",
        "dict" => lang == "si",
        "phonetic" => lang == "ln",
        "searchjl" => lang == "ko",
        "pinyin" | "stroke" | "zhuyin" | "unihan" => lang == "zh",
        _ => false,
    }
}

/// The non-default entries in this locale's `%Intl.Collator%.[[SortLocaleData]].[[co]]`, sorted as
/// required by ECMA-402 CollationsOfLocale.
pub(super) fn supported_collations(lang: &str) -> Vec<&'static str> {
    let mut collations = COLLATIONS
        .iter()
        .copied()
        .filter(|collation| supported_collation(lang, collation))
        .collect::<Vec<_>>();
    collations.sort_unstable();
    collations
}

#[cfg(test)]
mod tests {
    use super::*;

    const UCA_NON_IGNORABLE: &str =
        include_str!("../../tests/unicode-17.0.0/CollationTest_NON_IGNORABLE_SHORT.txt");
    const CLDR_NON_IGNORABLE: &str =
        include_str!("../../tests/cldr-48/CollationTest_CLDR_NON_IGNORABLE_SHORT.txt");

    fn from_code_points(line: &str) -> String {
        let code_points = line
            .split_whitespace()
            .map(|value| u32::from_str_radix(value, 16).unwrap())
            .collect::<Vec<_>>();
        crate::jstr::from_code_points(&code_points)
    }

    #[test]
    fn unicode_17_uca_non_ignorable_conformance() {
        let options = CollateOpts {
            locale: "und".to_string(),
            collation: "default".to_string(),
            sensitivity: "variant".to_string(),
            numeric: false,
            ignore_punct: false,
            upper_first: false,
            expand_umlaut: false,
        };
        let mut previous: Option<(usize, String)> = None;
        let mut checked = 0;
        for (index, raw) in UCA_NON_IGNORABLE.lines().enumerate() {
            let line = raw.split('#').next().unwrap().trim();
            if line.is_empty() {
                continue;
            }
            let current = from_code_points(line);
            if let Some((previous_index, previous_value)) = &previous {
                assert!(
                    collate(previous_value, &current, &options) != std::cmp::Ordering::Greater,
                    "UCA order regression between corpus lines {} and {}",
                    previous_index + 1,
                    index + 1
                );
            }
            previous = Some((index, current));
            checked += 1;
        }
        assert!(checked > 10_000, "official short UCA corpus was truncated");
    }

    #[test]
    fn cldr_48_root_non_ignorable_conformance() {
        let options = CollateOpts {
            locale: "en".to_string(),
            collation: "default".to_string(),
            sensitivity: "variant".to_string(),
            numeric: false,
            ignore_punct: false,
            upper_first: false,
            expand_umlaut: false,
        };
        let mut previous: Option<(usize, String)> = None;
        let mut checked = 0;
        for (index, raw) in CLDR_NON_IGNORABLE.lines().enumerate() {
            let line = raw.split('#').next().unwrap().trim();
            if line.is_empty() {
                continue;
            }
            let current = from_code_points(line);
            if let Some((previous_index, previous_value)) = &previous {
                assert!(
                    collate(previous_value, &current, &options) != std::cmp::Ordering::Greater,
                    "CLDR root order regression between corpus lines {} and {}",
                    previous_index + 1,
                    index + 1
                );
            }
            previous = Some((index, current));
            checked += 1;
        }
        assert!(checked > 10_000, "official short CLDR corpus was truncated");
    }
}
