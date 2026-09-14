//! `Intl.NumberFormat` with ECMA-402 rounding and generated CLDR 48 locale data.

use super::service::{
    brand_slot, get_option, install_supported_locales, instance_proto, read_locale_matcher,
};
use super::{ab, arg, canonicalize_locale_list, coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_builtin, set_data, Gc, Value};

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "NumberFormat", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "format", 1, |i, this, a| {
        // `format` is a bound-ish getter in the spec; here it's a plain method (sufficient for most
        // tests) — but many tests read `.format` then call it, so return a stable per-instance fn.
        format_number(i, &this, &arg(a, 0))
    });
    it.def_method(&proto, "formatToParts", 1, |i, this, a| {
        format_to_parts(i, &this, &arg(a, 0))
    });
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
    it.def_method(&proto, "formatRange", 2, |i, this, a| {
        format_range(i, &this, &arg(a, 0), &arg(a, 1))
    });
    it.def_method(&proto, "formatRangeToParts", 2, |i, this, a| {
        format_range_to_parts(i, &this, &arg(a, 0), &arg(a, 1))
    });
    // A `format` accessor that returns a bound function is what the spec mandates; provide it.
    install_format_getter(it, &proto);
}

/// The two range endpoints as intl mathematical values, rejecting NaN/undefined per
/// FormatNumericRange step 1 (`start`/`end` must not be undefined; NaN throws RangeError).
fn range_endpoints(
    i: &mut Interp,
    this: &Value,
    x: &Value,
    y: &Value,
) -> Result<(Gc, f64, f64), Value> {
    let o = instance(i, this)?;
    if matches!(x, Value::Undefined) || matches!(y, Value::Undefined) {
        return Err(i.make_error("TypeError", "formatRange requires two arguments"));
    }
    let a = to_intl_number(i, x)?;
    let b = to_intl_number(i, y)?;
    if a.is_nan() || b.is_nan() {
        return Err(i.make_error("RangeError", "formatRange arguments must not be NaN"));
    }
    Ok((o, a, b))
}

fn format_range(i: &mut Interp, this: &Value, x: &Value, y: &Value) -> Result<Value, Value> {
    let (o, a, b) = range_endpoints(i, this, x, y)?;
    let sa = assemble_number_exact(i, &o, a, exact_of(x)).text;
    let sb = assemble_number_exact(i, &o, b, exact_of(y)).text;
    let nu = get_str(&o, "__nf_nu");
    // Endpoints that FORMAT identically collapse to a single approximate value.
    if sa == sb {
        return Ok(Value::from_string(format!(
            "{}{}",
            cldr_number_symbols(&o).approximately,
            xlate_digits(&sa, &nu)
        )));
    }
    let (start, sep, end) = range_join(&o, &sa, &sb);
    Ok(Value::from_string(format!(
        "{}{}{}",
        xlate_digits(&start, &nu),
        sep,
        xlate_digits(&end, &nu)
    )))
}

/// The locale's range join: separator plus the ICU affix-collapsing rules (a prefix-sign start
/// keeps only its own affixes; a suffix-currency locale drops the start's suffix).
fn range_join(o: &Gc, sa: &str, sb: &str) -> (String, &'static str, String) {
    let lang = get_str(o, "__nf_locale");
    let lang = lang.split('-').next().unwrap_or("en").to_string();
    let currency = get_str(o, "__nf_style") == "currency";
    if lang == "pt" {
        // Suffix-currency: the start keeps only its digits ("3 - 5 €").
        let start = if currency {
            sa.trim_end_matches(|c: char| !c.is_ascii_digit())
                .to_string()
        } else {
            sa.to_string()
        };
        // A shared sign is shown on the start only.
        let end = if sa.starts_with('+') || sa.starts_with('-') {
            sb.trim_start_matches(['+', '-']).to_string()
        } else {
            sb.to_string()
        };
        return (start, " - ", end);
    }
    if currency {
        if sa.starts_with('+') || sa.starts_with('-') {
            // A signed start absorbs the shared prefix; the end shows bare digits.
            let end = sb
                .trim_start_matches(|c: char| !c.is_ascii_digit())
                .to_string();
            return (sa.to_string(), "\u{2013}", end);
        }
        return (sa.to_string(), " \u{2013} ", sb.to_string());
    }
    (sa.to_string(), "\u{2013}", sb.to_string())
}

fn format_range_to_parts(
    i: &mut Interp,
    this: &Value,
    x: &Value,
    y: &Value,
) -> Result<Value, Value> {
    let (o, a, b) = range_endpoints(i, this, x, y)?;
    let stype = suffix_type_of(&o);
    let symbols = cldr_number_symbols(&o);
    let nu = get_str(&o, "__nf_nu");
    let mut out: Vec<Value> = Vec::new();
    let push_parts = |i: &mut Interp, whole: &str, source: &str, out: &mut Vec<Value>| {
        for (t, mut v) in decompose_parts(whole, &stype, &symbols) {
            let ob = i.new_object();
            set_data(&ob, "type", Value::str(t));
            if matches!(t, "integer" | "fraction" | "exponentInteger") {
                v = xlate_digits(&v, &nu);
            }
            set_data(&ob, "value", Value::from_string(v));
            set_data(&ob, "source", Value::str(source));
            out.push(Value::Obj(ob));
        }
    };
    let sa = assemble_number_exact(i, &o, a, exact_of(x)).text;
    let sb = assemble_number_exact(i, &o, b, exact_of(y)).text;
    if sa == sb {
        let approx = i.new_object();
        set_data(&approx, "type", Value::str("approximatelySign"));
        set_data(
            &approx,
            "value",
            Value::str(cldr_number_symbols(&o).approximately),
        );
        set_data(&approx, "source", Value::str("shared"));
        out.push(Value::Obj(approx));
        push_parts(i, &sa, "shared", &mut out);
        return Ok(i.make_array(out));
    }
    let (start, sep, end) = range_join(&o, &sa, &sb);
    push_parts(i, &start, "startRange", &mut out);
    let lit = i.new_object();
    set_data(&lit, "type", Value::str("literal"));
    set_data(&lit, "value", Value::str(sep));
    set_data(&lit, "source", Value::str("shared"));
    out.push(Value::Obj(lit));
    push_parts(i, &end, "endRange", &mut out);
    Ok(i.make_array(out))
}

fn install_format_getter(it: &mut Interp, proto: &Gc) {
    let g = it.make_native("get format", 0, |i, this, _| {
        let o = instance_unwrap(i, &this)?;
        // Cache a bound function on the instance so repeated reads return the same object.
        if let Some(f) = o.borrow().props.get("__nf_boundformat").map(|p| p.value()) {
            return Ok(f);
        }
        let f = i.make_native("", 1, |i, that, a| format_number(i, &that, &arg(a, 0)));
        // Bind `this` = the (possibly unwrapped) NumberFormat instance.
        let bound = crate::intl::numberformat::bind_this(i, Value::Obj(f), Value::Obj(o.clone()));
        set_builtin(&o, "__nf_boundformat", bound.clone());
        Ok(bound)
    });
    proto.borrow_mut().props.insert(
        "format",
        crate::value::Property::accessor_prop(Some(Value::Obj(g)), None, false, true),
    );
}

/// Bind `this_arg` onto `target` via Function.prototype.bind, then normalise the result to match the
/// spec's `format`/`compare` bound functions: name `""`, length 1, and not a constructor.
pub(crate) fn bind_this(i: &mut Interp, target: Value, this_arg: Value) -> Value {
    if let Ok(bindfn) = i.get_member(&target, "bind") {
        if let Ok(bound) = i.call(bindfn, target.clone(), &[this_arg]) {
            if let Value::Obj(o) = &bound {
                o.borrow_mut().is_constructor = false;
                o.borrow_mut().props.insert(
                    "name",
                    crate::value::Property::data(Value::str(""), false, false, true),
                );
            }
            return bound;
        }
    }
    target
}

pub(super) struct DigitOpts {
    pub(super) min_int: u32,
    pub(super) min_frac: u32,
    pub(super) max_frac: u32,
    pub(super) min_sig: Option<u32>,
    pub(super) max_sig: Option<u32>,
    /// "significant" | "fraction" | "morePrecision" | "lessPrecision".
    pub(super) rounding_type: &'static str,
}

/// The raw significant/fraction-digit options, read in spec order before the rounding options.
pub(super) struct RawDigits {
    min_int: u32,
    mnfd: Option<u32>,
    mxfd: Option<u32>,
    mnsd: Option<u32>,
    mxsd: Option<u32>,
}

fn construct(i: &mut Interp, t: Value, a: &[Value]) -> Result<Value, Value> {
    // Legacy service: callable without `new` (returns a fresh instance either way).
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;
    // numberingSystem is read right after localeMatcher, and must be a valid type identifier.
    let numbering = get_option(i, &options, "numberingSystem", &[], None)?;
    if let Some(ns) = &numbering {
        if !ns
            .split('-')
            .all(|p| p.len() >= 3 && p.len() <= 8 && p.bytes().all(|b| b.is_ascii_alphanumeric()))
        {
            return Err(i.make_error("RangeError", format!("invalid numberingSystem: {ns}")));
        }
    }
    let (resolved_locale, numbering) =
        super::service::resolve_locale_nu(&requested, numbering.as_deref());

    let style = get_option(
        i,
        &options,
        "style",
        &["decimal", "percent", "currency", "unit"],
        Some("decimal"),
    )?
    .unwrap();

    // currency
    let currency = get_option(i, &options, "currency", &[], None)?;
    if style == "currency" && currency.is_none() {
        return Err(i.make_error("TypeError", "currency is required for currency style"));
    }
    if let Some(c) = &currency {
        if !is_well_formed_currency(c) {
            return Err(i.make_error("RangeError", format!("invalid currency: {c}")));
        }
    }
    let currency_display = get_option(
        i,
        &options,
        "currencyDisplay",
        &["code", "symbol", "narrowSymbol", "name"],
        Some("symbol"),
    )?
    .unwrap();
    let currency_sign = get_option(
        i,
        &options,
        "currencySign",
        &["standard", "accounting"],
        Some("standard"),
    )?
    .unwrap();

    // unit
    let unit = get_option(i, &options, "unit", &[], None)?;
    if style == "unit" && unit.is_none() {
        return Err(i.make_error("TypeError", "unit is required for unit style"));
    }
    if let Some(u) = &unit {
        if !is_well_formed_unit(u) {
            return Err(i.make_error("RangeError", format!("invalid unit: {u}")));
        }
    }
    let unit_display = get_option(
        i,
        &options,
        "unitDisplay",
        &["short", "narrow", "long"],
        Some("short"),
    )?
    .unwrap();

    // notation is read before the digit options.
    let notation = get_option(
        i,
        &options,
        "notation",
        &["standard", "scientific", "engineering", "compact"],
        Some("standard"),
    )?
    .unwrap();

    // digit options (minInt, min/maxFrac, min/maxSig), then the rounding options.
    let cur_digits = currency
        .as_deref()
        .map(|c| currency_fraction_digits(&c.to_uppercase()))
        .unwrap_or(2);
    let raw_digits = read_raw_digits(i, &options)?;

    let rounding_increment = {
        let v = ab(i.get_member(&options, "roundingIncrement"))?;
        if matches!(v, Value::Undefined) {
            1u32
        } else {
            let n = ab(i.to_number(&v))?;
            let allowed = [
                1u32, 2, 5, 10, 20, 25, 50, 100, 200, 250, 500, 1000, 2000, 2500, 5000,
            ];
            if n.fract() != 0.0 || !allowed.contains(&(n as u32)) {
                return Err(i.make_error("RangeError", "invalid roundingIncrement"));
            }
            n as u32
        }
    };
    let rounding_mode = get_option(
        i,
        &options,
        "roundingMode",
        &[
            "ceil",
            "floor",
            "expand",
            "trunc",
            "halfCeil",
            "halfFloor",
            "halfExpand",
            "halfTrunc",
            "halfEven",
        ],
        Some("halfExpand"),
    )?
    .unwrap();
    let rounding_priority = get_option(
        i,
        &options,
        "roundingPriority",
        &["auto", "morePrecision", "lessPrecision"],
        Some("auto"),
    )?
    .unwrap();
    let digits = interpret_digits(
        i,
        &raw_digits,
        &style,
        &notation,
        &rounding_priority,
        cur_digits,
        rounding_increment,
    )?;
    // A roundingIncrement other than 1 requires fraction-digit rounding (TypeError otherwise), and
    // then maximumFractionDigits must equal minimumFractionDigits (RangeError otherwise).
    if rounding_increment != 1 {
        if digits.rounding_type != "fraction" {
            return Err(i.make_error(
                "TypeError",
                "roundingIncrement is only supported with fractionDigits rounding",
            ));
        }
        if digits.min_frac != digits.max_frac {
            return Err(i.make_error(
                "RangeError",
                "maximumFractionDigits must equal minimumFractionDigits with roundingIncrement",
            ));
        }
    }
    let trailing_zero = get_option(
        i,
        &options,
        "trailingZeroDisplay",
        &["auto", "stripIfInteger"],
        Some("auto"),
    )?
    .unwrap();

    let compact_display = get_option(
        i,
        &options,
        "compactDisplay",
        &["short", "long"],
        Some("short"),
    )?
    .unwrap();
    let use_grouping = read_use_grouping(i, &options, &notation)?;
    let sign_display = get_option(
        i,
        &options,
        "signDisplay",
        &["auto", "never", "always", "exceptZero", "negative"],
        Some("auto"),
    )?
    .unwrap();

    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.NumberFormat")? {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__nf", Value::Bool(true));
    set_builtin(&obj, "__nf_locale", Value::from_string(resolved_locale));
    set_builtin(&obj, "__nf_nu", Value::from_string(numbering));
    set_builtin(
        &obj,
        "__nf_roundingincrement",
        Value::Num(rounding_increment as f64),
    );
    set_builtin(
        &obj,
        "__nf_roundingpriority",
        Value::from_string(rounding_priority),
    );
    set_builtin(&obj, "__nf_trailingzero", Value::from_string(trailing_zero));
    set_builtin(&obj, "__nf_style", Value::from_string(style));
    if let Some(c) = currency.filter(|_| get_str(&obj, "__nf_style") == "currency") {
        set_builtin(&obj, "__nf_currency", Value::from_string(c.to_uppercase()));
        set_builtin(
            &obj,
            "__nf_currencydisplay",
            Value::from_string(currency_display),
        );
        set_builtin(&obj, "__nf_currencysign", Value::from_string(currency_sign));
    }
    if let Some(u) = unit {
        set_builtin(&obj, "__nf_unit", Value::from_string(u));
        set_builtin(&obj, "__nf_unitdisplay", Value::from_string(unit_display));
    }
    set_builtin(&obj, "__nf_minint", Value::Num(digits.min_int as f64));
    set_builtin(&obj, "__nf_minfrac", Value::Num(digits.min_frac as f64));
    set_builtin(&obj, "__nf_maxfrac", Value::Num(digits.max_frac as f64));
    if let Some(v) = digits.min_sig {
        set_builtin(&obj, "__nf_minsig", Value::Num(v as f64));
    }
    if let Some(v) = digits.max_sig {
        set_builtin(&obj, "__nf_maxsig", Value::Num(v as f64));
    }
    set_builtin(&obj, "__nf_notation", Value::from_string(notation));
    set_builtin(
        &obj,
        "__nf_compactdisplay",
        Value::from_string(compact_display),
    );
    set_builtin(&obj, "__nf_grouping", use_grouping);
    set_builtin(&obj, "__nf_signdisplay", Value::from_string(sign_display));
    set_builtin(&obj, "__nf_roundingmode", Value::from_string(rounding_mode));
    set_builtin(&obj, "__nf_roundingtype", Value::str(digits.rounding_type));
    // Legacy "ChainNumberFormat" (see datetimeformat.rs).
    if !i.constructing {
        if let Some(chained) = crate::intl::legacy_chain(i, &t, "Intl.NumberFormat", &obj) {
            return Ok(chained);
        }
    }
    Ok(Value::Obj(obj))
}

/// The CLDR default fraction-digit count for a currency (ISO 4217 minor units).
fn currency_fraction_digits(code: &str) -> u32 {
    crate::cldr_numbers::currency_digits(code)
}

/// SetNumberFormatDigitOptions, phase 1: read the raw digit options (spec order: minInt, minFrac,
/// maxFrac, minSig, maxSig). The rounding options are read by the caller afterwards; interpretation
/// happens in `interpret_digits` once the rounding priority is known.
pub(super) fn read_raw_digits(i: &mut Interp, options: &Value) -> Result<RawDigits, Value> {
    let min_int = read_range(i, options, "minimumIntegerDigits", 1, 21, 1)?;
    let mnfd = read_range_opt(i, options, "minimumFractionDigits", 0, 100)?;
    let mxfd = read_range_opt(i, options, "maximumFractionDigits", 0, 100)?;
    let mnsd = read_range_opt(i, options, "minimumSignificantDigits", 1, 21)?;
    let mxsd = read_range_opt(i, options, "maximumSignificantDigits", 1, 21)?;
    Ok(RawDigits {
        min_int,
        mnfd,
        mxfd,
        mnsd,
        mxsd,
    })
}

/// SetNumberFormatDigitOptions, phase 2: derive the effective digit bounds and rounding type from the
/// raw options, the style/notation defaults, and the rounding priority.
pub(super) fn interpret_digits(
    i: &mut Interp,
    raw: &RawDigits,
    style: &str,
    notation: &str,
    priority: &str,
    cur_digits: u32,
    rounding_increment: u32,
) -> Result<DigitOpts, Value> {
    // The currency minor-unit defaults only apply in standard notation.
    let (mnfd_default, mut mxfd_default) = if style == "currency" && notation == "standard" {
        (cur_digits, cur_digits)
    } else if style == "percent" {
        (0, 0)
    } else {
        (0, 3)
    };
    if rounding_increment != 1 {
        mxfd_default = mnfd_default;
    }
    let has_sd = raw.mnsd.is_some() || raw.mxsd.is_some();
    let has_fd = raw.mnfd.is_some() || raw.mxfd.is_some();
    let mut need_sd = true;
    let mut need_fd = true;
    if priority == "auto" {
        need_sd = has_sd;
        if need_sd || (!has_fd && notation == "compact") {
            need_fd = false;
        }
    }

    let (mut min_sig, mut max_sig) = (None, None);
    if need_sd {
        if has_sd {
            let mn = raw.mnsd.unwrap_or(1);
            let mx = raw.mxsd.unwrap_or(21);
            if mn > mx {
                return Err(i.make_error(
                    "RangeError",
                    "minimumSignificantDigits > maximumSignificantDigits",
                ));
            }
            min_sig = Some(mn);
            max_sig = Some(mx);
        } else {
            min_sig = Some(1);
            max_sig = Some(21);
        }
    }

    let (mut min_frac, mut max_frac) = (0u32, 0u32);
    if need_fd {
        let (mn, mx) = if has_fd {
            match (raw.mnfd, raw.mxfd) {
                (None, Some(mx)) => (mnfd_default.min(mx), mx),
                (Some(mn), None) => (mn, mxfd_default.max(mn)),
                (Some(mn), Some(mx)) => {
                    if mn > mx {
                        return Err(i.make_error(
                            "RangeError",
                            "minimumFractionDigits > maximumFractionDigits",
                        ));
                    }
                    (mn, mx)
                }
                (None, None) => (mnfd_default, mxfd_default),
            }
        } else {
            (mnfd_default, mxfd_default)
        };
        min_frac = mn;
        max_frac = mx;
    }

    let rounding_type = if !need_sd && !need_fd {
        // Neither range requested (compact, auto): the default rounding is morePrecision over 0
        // fraction and (1..2) significant digits.
        min_frac = 0;
        max_frac = 0;
        min_sig = Some(1);
        max_sig = Some(2);
        "morePrecision"
    } else if priority == "morePrecision" {
        "morePrecision"
    } else if priority == "lessPrecision" {
        "lessPrecision"
    } else if has_sd {
        "significant"
    } else {
        "fraction"
    };

    Ok(DigitOpts {
        min_int: raw.min_int,
        min_frac,
        max_frac,
        min_sig,
        max_sig,
        rounding_type,
    })
}

fn read_range(
    i: &mut Interp,
    options: &Value,
    prop: &str,
    lo: u32,
    hi: u32,
    fallback: u32,
) -> Result<u32, Value> {
    Ok(read_range_opt(i, options, prop, lo, hi)?.unwrap_or(fallback))
}
fn read_range_opt(
    i: &mut Interp,
    options: &Value,
    prop: &str,
    lo: u32,
    hi: u32,
) -> Result<Option<u32>, Value> {
    let v = ab(i.get_member(options, prop))?;
    if matches!(v, Value::Undefined) {
        return Ok(None);
    }
    let n = ab(i.to_number(&v))?;
    if n.is_nan() {
        return Err(i.make_error("RangeError", format!("{prop} is NaN")));
    }
    let f = n.floor();
    if f < lo as f64 || f > hi as f64 {
        return Err(i.make_error("RangeError", format!("{prop} out of range")));
    }
    Ok(Some(f as u32))
}

fn read_use_grouping(i: &mut Interp, options: &Value, notation: &str) -> Result<Value, Value> {
    // GetStringOrBooleanOption: undefined → fallback; `true` → "always"; a falsy value → false;
    // otherwise ToString and validate against ["min2","auto","always"] (so the string "true" is a
    // RangeError, not "always"). The fallback is "auto" (or "min2" for compact notation).
    let v = ab(i.get_member(options, "useGrouping"))?;
    let default = Value::str(if notation == "compact" {
        "min2"
    } else {
        "auto"
    });
    if matches!(v, Value::Undefined) {
        return Ok(default);
    }
    if matches!(v, Value::Bool(true)) {
        return Ok(Value::str("always"));
    }
    if !i.to_boolean(&v) {
        return Ok(Value::Bool(false));
    }
    let s = ab(i.to_string(&v))?.to_string();
    // The strings "true"/"false" fall back to the default (they are not valid values, but
    // GetBooleanOrStringNumberFormatOption returns the fallback for them instead of throwing).
    if s == "true" || s == "false" {
        return Ok(default);
    }
    if !["always", "auto", "min2"].contains(&s.as_str()) {
        return Err(i.make_error("RangeError", format!("invalid useGrouping: {s}")));
    }
    Ok(Value::from_string(s))
}

fn is_well_formed_currency(c: &str) -> bool {
    c.len() == 3 && c.bytes().all(|b| b.is_ascii_alphabetic())
}
fn is_well_formed_unit(u: &str) -> bool {
    // A sanctioned single unit, or "X-per-Y" with both X and Y sanctioned (IsWellFormedUnitIdentifier).
    let sanctioned = |s: &str| crate::units::SANCTIONED_UNITS.contains(&s);
    match u.split_once("-per-") {
        Some((a, b)) => sanctioned(a) && sanctioned(b),
        None => sanctioned(u),
    }
}

// ---- formatting ------------------------------------------------------------------------------

fn instance(i: &mut Interp, this: &Value) -> Result<Gc, Value> {
    brand_slot(i, this, "__nf")
}

/// UnwrapNumberFormat: like `instance`, but follows a legacy chained receiver.
fn instance_unwrap(i: &mut Interp, this: &Value) -> Result<Gc, Value> {
    crate::intl::brand_slot_legacy(i, this, "__nf", "Intl.NumberFormat")
}

fn get_str(o: &Gc, k: &str) -> String {
    match o.borrow().props.get(k).map(|p| p.value()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => String::new(),
    }
}

fn cldr_number_locale(o: &Gc) -> &'static str {
    let locale = get_str(o, "__nf_locale");
    let mut parts = locale.split('-');
    let lang = parts.next().unwrap_or("en");
    let mut script = "";
    let mut region = "";
    for part in parts {
        if script.is_empty()
            && part.len() == 4
            && part.as_bytes()[0].is_ascii_uppercase()
            && part.as_bytes()[1..].iter().all(u8::is_ascii_lowercase)
        {
            script = part;
        } else if region.is_empty()
            && ((part.len() == 2 && part.bytes().all(|byte| byte.is_ascii_uppercase()))
                || (part.len() == 3 && part.bytes().all(|byte| byte.is_ascii_digit())))
        {
            region = part;
        }
    }
    crate::cldr_numbers::locale(lang, script, region)
}

fn cldr_number_symbols(o: &Gc) -> crate::cldr_numbers::Symbols {
    crate::cldr_numbers::symbols(cldr_number_locale(o), &get_str(o, "__nf_nu"))
}

fn get_num(o: &Gc, k: &str) -> Option<u32> {
    match o.borrow().props.get(k).map(|p| p.value()) {
        Some(Value::Num(n)) => Some(n as u32),
        _ => None,
    }
}

/// Produce (sign_is_negative, digit-string) for |x| per digit options.
fn format_magnitude(x: f64, o: &Gc) -> String {
    let min_int = get_num(o, "__nf_minint").unwrap_or(1);
    let min_frac = get_num(o, "__nf_minfrac").unwrap_or(0);
    let max_frac = get_num(o, "__nf_maxfrac").unwrap_or(0);
    let min_sig = get_num(o, "__nf_minsig");
    let max_sig = get_num(o, "__nf_maxsig");

    let increment = get_num(o, "__nf_roundingincrement").unwrap_or(1);
    let mode = get_str(o, "__nf_roundingmode");
    let mode = if mode.is_empty() { "halfExpand" } else { &mode };
    let rtype = get_str(o, "__nf_roundingtype");
    format_magnitude_options(
        x, min_int, min_frac, max_frac, min_sig, max_sig, increment, mode, &rtype,
    )
}

/// `FormatNumericToString`'s decimal rounding and minimum-integer padding, shared with
/// `Intl.PluralRules`. Keeping this path common prevents plural selection from drifting from the
/// number that the same digit options format.
#[allow(clippy::too_many_arguments)]
pub(super) fn format_magnitude_options(
    x: f64,
    min_int: u32,
    min_frac: u32,
    max_frac: u32,
    min_sig: Option<u32>,
    max_sig: Option<u32>,
    increment: u32,
    mode: &str,
    rtype: &str,
) -> String {
    let mut s = match rtype {
        "significant" => {
            round_significant_dec(x, max_sig.unwrap_or(21), min_sig.unwrap_or(1), mode)
        }
        "morePrecision" | "lessPrecision" => round_priority(
            x,
            min_sig.unwrap_or(1),
            max_sig.unwrap_or(21),
            min_frac,
            max_frac,
            increment,
            mode,
            rtype == "morePrecision",
        ),
        "fraction" => round_fraction_dec(x, min_frac, max_frac, increment, mode),
        // Fallback for formatters created without a stored rounding type.
        _ if max_sig.is_some() => {
            round_significant_dec(x, max_sig.unwrap(), min_sig.unwrap_or(1), mode)
        }
        _ => round_fraction_dec(x, min_frac, max_frac, increment, mode),
    };
    // Pad integer digits to min_int.
    {
        let (int_part, frac_part) = match s.split_once('.') {
            Some((a, b)) => (a.to_string(), Some(b.to_string())),
            None => (s.clone(), None),
        };
        let int_digits = int_part.trim_start_matches('0');
        let int_len = int_digits.len().max(1);
        let padded_int = if (int_len as u32) < min_int {
            format!("{:0>width$}", int_digits.max(""), width = min_int as usize)
        } else if int_digits.is_empty() {
            "0".to_string()
        } else {
            int_digits.to_string()
        };
        s = match frac_part {
            Some(f) => format!("{padded_int}.{f}"),
            None => padded_int,
        };
    }
    s
}

/// The shortest round-trip decimal digits of `|x|` as (integer_digits, fraction_digits). Rounding is
/// performed on this decimal expansion (not the binary f64) so `1.015` rounds like the source `1.015`.
fn decimal_digits(x: f64) -> (String, String) {
    let s = format!("{}", x.abs());
    match s.split_once('.') {
        Some((a, b)) => (a.to_string(), b.to_string()),
        None => (s, String::new()),
    }
}

/// Round the decimal `int_str.frac_str` to `keep` fraction digits (may be negative, rounding integer
/// places), snapping to a multiple of `inc` at that scale, per the ECMA-402 `mode`. Returns the exact
/// decimal string (unsigned, `keep` fraction digits when `keep > 0`, no min-frac padding).
fn round_decimal(
    int_str: &str,
    frac_str: &str,
    keep: i32,
    inc: u32,
    mode: &str,
    negative: bool,
) -> String {
    let mut digits: Vec<u8> = int_str
        .bytes()
        .chain(frac_str.bytes())
        .map(|b| b - b'0')
        .collect();
    let point = int_str.len() as i32;
    let cut = point + keep; // number of leading digits retained as the coefficient
    if cut <= 0 {
        // Everything is discarded; the only possible non-zero result is a single rounded-up unit.
        let any = digits.iter().any(|&d| d != 0);
        let up = round_up_decision(
            mode,
            negative,
            /*rem*/ 0.0,
            /*frac*/ if any { 0.5001 } else { 0.0 },
            inc,
            0,
        );
        let val = if up { inc as u128 } else { 0 };
        return place_decimal(&val.to_string(), keep);
    }
    let cut = cut as usize;
    if digits.len() < cut {
        digits.resize(cut, 0);
    }
    let retained: String = digits[..cut].iter().map(|d| (d + b'0') as char).collect();
    let discarded = &digits[cut..];
    // The discarded tail as a fraction of one retained unit (short string → exact enough to compare).
    let frac = if discarded.is_empty() {
        0.0
    } else {
        let s: String = discarded.iter().map(|d| (d + b'0') as char).collect();
        format!("0.{s}").parse::<f64>().unwrap_or(0.0)
    };
    let Ok(c) = retained.parse::<u128>() else {
        // Coefficient too large for exact arithmetic; emit unrounded (huge integers seldom round).
        return place_decimal(&retained, keep);
    };
    let rem = (c % inc.max(1) as u128) as f64;
    let quotient = c / inc.max(1) as u128;
    let up = round_up_decision(mode, negative, rem, frac, inc, quotient);
    let base = c - (c % inc.max(1) as u128);
    let result = if up { base + inc as u128 } else { base };
    place_decimal(&result.to_string(), keep)
}

/// Reconstruct a decimal string from an integer coefficient at scale `10^-keep`.
fn place_decimal(coeff: &str, keep: i32) -> String {
    if keep <= 0 {
        return format!("{coeff}{}", "0".repeat((-keep) as usize));
    }
    let keep = keep as usize;
    let padded = if coeff.len() <= keep {
        format!("{:0>width$}", coeff, width = keep + 1)
    } else {
        coeff.to_string()
    };
    let split = padded.len() - keep;
    format!("{}.{}", &padded[..split], &padded[split..])
}

/// Whether to round the coefficient up by one increment, given the discarded position `rem + frac`
/// (in increment units, `rem` an integer remainder and `frac` in [0,1)) under ECMA-402 `mode`.
fn round_up_decision(
    mode: &str,
    negative: bool,
    rem: f64,
    frac: f64,
    inc: u32,
    quotient: u128,
) -> bool {
    let position = rem + frac;
    let half = inc as f64 / 2.0;
    let some = position > 0.0;
    match mode {
        "trunc" => false,
        "expand" => some,
        "ceil" => some && !negative,
        "floor" => some && negative,
        "halfTrunc" => position > half,
        "halfExpand" => position >= half,
        "halfCeil" => position > half || (position == half && !negative),
        "halfFloor" => position > half || (position == half && negative),
        "halfEven" => {
            // Tie → round toward the multiple whose quotient is even (parity of the retained value).
            position > half || (position == half && quotient % 2 == 1)
        }
        _ => position >= half, // halfExpand default
    }
}

/// Round `x` to at most `max_frac` fraction digits, snapping to `increment`, per `mode`; pad to
/// `min_frac`.
fn round_fraction_dec(x: f64, min_frac: u32, max_frac: u32, increment: u32, mode: &str) -> String {
    let (int_str, frac_str) = decimal_digits(x);
    let negative = x.is_sign_negative();
    let mut s = round_decimal(
        &int_str,
        &frac_str,
        max_frac as i32,
        increment.max(1),
        mode,
        negative,
    );
    // Trim trailing zeros beyond min_frac (only meaningful without an increment > 1).
    if increment <= 1 && max_frac > min_frac && s.contains('.') {
        while s.ends_with('0') {
            let frac_len = s.split('.').nth(1).map(|f| f.len()).unwrap_or(0);
            if frac_len as u32 <= min_frac {
                break;
            }
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

/// The base-10 exponent of the most-significant digit of `x` (0 for the ones place). Zero maps to 0,
/// matching ToRawPrecision's handling.
fn msd_exponent(x: f64) -> i32 {
    if x == 0.0 {
        return 0;
    }
    let (int_str, frac_str) = decimal_digits(x);
    let int_trim = int_str.trim_start_matches('0');
    if !int_trim.is_empty() {
        int_trim.len() as i32 - 1
    } else {
        -(1 + frac_str.bytes().take_while(|&b| b == b'0').count() as i32)
    }
}

/// PartitionNumberPattern's morePrecision/lessPrecision disambiguation: round `x` both ways and pick
/// the significant-digit or fraction-digit result. The rounding magnitude uses the *maximum*
/// settings (ToRawPrecision: `e - maxSig + 1`; ToRawFixed: `-maxFrac`).
#[allow(clippy::too_many_arguments)]
fn round_priority(
    x: f64,
    min_sig: u32,
    max_sig: u32,
    min_frac: u32,
    max_frac: u32,
    increment: u32,
    mode: &str,
    more: bool,
) -> String {
    let s_result = round_significant_dec(x, max_sig, min_sig, mode);
    let f_result = round_fraction_dec(x, min_frac, max_frac, increment.max(1), mode);
    let s_mag = msd_exponent(x) - max_sig as i32 + 1;
    let f_mag = -(max_frac as i32);
    let fixed_is_more_precise = f_mag < s_mag;
    if (more && fixed_is_more_precise) || (!more && !fixed_is_more_precise) {
        f_result
    } else {
        s_result
    }
}

/// Round `x` to `max_sig` significant digits (per `mode`), keeping at least `min_sig`.
fn round_significant_dec(x: f64, max_sig: u32, min_sig: u32, mode: &str) -> String {
    if x == 0.0 {
        if min_sig > 1 {
            return format!("0.{}", "0".repeat((min_sig - 1) as usize));
        }
        return "0".to_string();
    }
    let (int_str, frac_str) = decimal_digits(x);
    let negative = x.is_sign_negative();
    // Position of the most-significant digit (0 = ones place, positive = 10^k, negative = 10^-k).
    let int_trim = int_str.trim_start_matches('0');
    let msd: i32 = if !int_trim.is_empty() {
        int_trim.len() as i32 - 1
    } else {
        // Leading zeros in the fraction push the first significant digit right.
        -(1 + frac_str.bytes().take_while(|&b| b == b'0').count() as i32)
    };
    let keep = max_sig as i32 - 1 - msd; // fraction digits to retain
    let mut s = round_decimal(&int_str, &frac_str, keep, 1, mode, negative);
    // Trim trailing fractional zeros down to min_sig significant digits.
    if s.contains('.') {
        let significant = |t: &str| {
            t.chars()
                .filter(|c| c.is_ascii_digit())
                .skip_while(|c| *c == '0')
                .count()
        };
        while s.ends_with('0') && significant(&s) as u32 > min_sig {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
    }
    s
}

fn group_integer(
    int_part: &str,
    grouping: &Value,
    sep: &str,
    sizes: (usize, usize),
    minimum_grouping: usize,
) -> String {
    let enabled = !matches!(grouping, Value::Bool(false));
    let min2 = matches!(grouping, Value::Str(s) if &**s == "min2")
        || (minimum_grouping >= 2 && matches!(grouping, Value::Str(s) if &**s == "auto"));
    let (primary, secondary) = sizes;
    if !enabled || int_part.len() <= primary || (min2 && int_part.len() <= primary + 1) {
        return int_part.to_string();
    }
    // Insert separators right-to-left: the first (rightmost) group is `primary` digits, the rest are
    // `secondary` digits (Indian-style 3;2 when they differ).
    let digits: Vec<char> = int_part.chars().collect();
    let n = digits.len();
    let mut out = String::new();
    for (idx, c) in digits.iter().enumerate() {
        let from_right = n - idx;
        let boundary =
            idx > 0 && from_right >= primary && (from_right - primary).is_multiple_of(secondary);
        if boundary {
            out.push_str(sep);
        }
        out.push(*c);
    }
    out
}

/// An exact decimal value (sign + integer/fraction digit strings) that must not round through
/// f64: a BigInt, or a plain decimal string input (ToIntlMathematicalValue keeps exact digits).
#[derive(Clone)]
pub(crate) struct ExactDec {
    pub int: String,
    pub frac: String,
    pub negative: bool,
}

impl ExactDec {
    /// Parse a simple decimal literal (`[+-]?digits[.digits]`, surrounding whitespace allowed);
    /// anything fancier (exponents, hex, Infinity) falls back to the f64 path.
    fn parse(s: &str) -> Option<ExactDec> {
        let t = s.trim();
        let negative = t.starts_with('-');
        let t = t.strip_prefix(['-', '+']).unwrap_or(t);
        let (int, frac) = match t.split_once('.') {
            Some((a, b)) => (a, b),
            None => (t, ""),
        };
        if int.is_empty() && frac.is_empty() {
            return None;
        }
        if !int.bytes().all(|b| b.is_ascii_digit()) || !frac.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let mut i = int.trim_start_matches('0').to_string();
        if i.is_empty() {
            i.push('0');
        }
        Some(ExactDec {
            int: i,
            frac: frac.to_string(),
            negative,
        })
    }
}

/// The exact value a format argument carries, when it has one.
pub(crate) fn exact_of(x: &Value) -> Option<ExactDec> {
    match x {
        Value::BigInt(b) => ExactDec::parse(&b.to_string_radix(10)),
        Value::Str(s) => ExactDec::parse(s),
        _ => None,
    }
}

fn exact_msd(ed: &ExactDec) -> i32 {
    let integer = ed.int.trim_start_matches('0');
    if !integer.is_empty() {
        integer.len().saturating_sub(1).min(i32::MAX as usize) as i32
    } else if let Some(first) = ed.frac.bytes().position(|byte| byte != b'0') {
        -(first.min(i32::MAX as usize - 1) as i32) - 1
    } else {
        0
    }
}

fn exact_zero(ed: &ExactDec) -> bool {
    ed.int.bytes().all(|byte| byte == b'0') && ed.frac.bytes().all(|byte| byte == b'0')
}

fn exact_f64(ed: &ExactDec) -> f64 {
    let sign = if ed.negative { "-" } else { "" };
    let text = if ed.frac.is_empty() {
        format!("{sign}{}", ed.int)
    } else {
        format!("{sign}{}.{}", ed.int, ed.frac)
    };
    text.parse().unwrap_or(if ed.negative {
        f64::NEG_INFINITY
    } else {
        f64::INFINITY
    })
}

/// Multiply an exact decimal by 10^places by moving its radix point without touching its digits.
fn shift_exact(ed: &ExactDec, places: i32) -> ExactDec {
    let mut digits = format!("{}{}", ed.int, ed.frac);
    let point = ed.int.len() as i64 + places as i64;
    let (int, frac) = if point <= 0 {
        (
            "0".to_string(),
            format!("{}{}", "0".repeat((-point) as usize), digits),
        )
    } else if point as usize >= digits.len() {
        digits.push_str(&"0".repeat(point as usize - digits.len()));
        (digits, String::new())
    } else {
        let frac = digits.split_off(point as usize);
        (digits, frac)
    };
    let int = int.trim_start_matches('0');
    ExactDec {
        int: if int.is_empty() { "0" } else { int }.to_string(),
        frac,
        negative: ed.negative,
    }
}

/// Format an exact magnitude after any percent/notation scaling has moved its radix point.
fn exact_magnitude(ed: &ExactDec, o: &Gc) -> Option<String> {
    exact_magnitude_options(
        ed,
        get_num(o, "__nf_minint").unwrap_or(1),
        get_num(o, "__nf_minfrac").unwrap_or(0),
        get_num(o, "__nf_maxfrac").unwrap_or(3),
        get_num(o, "__nf_minsig"),
        get_num(o, "__nf_maxsig"),
        get_num(o, "__nf_roundingincrement").unwrap_or(1),
        &get_str(o, "__nf_roundingmode"),
        &get_str(o, "__nf_roundingtype"),
        "standard",
    )
}

/// Exact-decimal counterpart of [`format_magnitude_options`]. It covers the standard rounding
/// shapes without converting BigInt or decimal-string inputs through `f64`.
#[allow(clippy::too_many_arguments)]
pub(super) fn exact_magnitude_options(
    ed: &ExactDec,
    min_int: u32,
    min_frac: u32,
    max_frac: u32,
    min_sig: Option<u32>,
    max_sig: Option<u32>,
    increment: u32,
    mode: &str,
    rtype: &str,
    notation: &str,
) -> Option<String> {
    if notation != "standard" {
        return None;
    }
    if matches!(rtype, "morePrecision" | "lessPrecision") {
        let significant = exact_magnitude_options(
            ed,
            min_int,
            0,
            0,
            Some(min_sig.unwrap_or(1)),
            Some(max_sig.unwrap_or(21)),
            1,
            mode,
            "significant",
            "standard",
        )?;
        let fraction = exact_magnitude_options(
            ed, min_int, min_frac, max_frac, None, None, increment, mode, "fraction", "standard",
        )?;
        let significant_magnitude = exact_msd(ed) - max_sig.unwrap_or(21) as i32 + 1;
        let fraction_magnitude = -(max_frac as i32);
        let fixed_is_more_precise = fraction_magnitude < significant_magnitude;
        return Some(
            if (rtype == "morePrecision" && fixed_is_more_precise)
                || (rtype == "lessPrecision" && !fixed_is_more_precise)
            {
                fraction
            } else {
                significant
            },
        );
    }

    let significant = rtype == "significant" || min_sig.is_some() || max_sig.is_some();
    let mut rounded = if significant && exact_zero(ed) {
        let minimum = min_sig.unwrap_or(1).max(1);
        if minimum == 1 {
            "0".to_string()
        } else {
            format!("0.{}", "0".repeat((minimum - 1) as usize))
        }
    } else if significant {
        let maximum = max_sig.unwrap_or(21).max(1);
        let keep = maximum as i32 - 1 - exact_msd(ed);
        round_decimal(&ed.int, &ed.frac, keep, 1, mode, ed.negative)
    } else {
        round_decimal(
            &ed.int,
            &ed.frac,
            max_frac as i32,
            increment.max(1),
            mode,
            ed.negative,
        )
    };

    let minimum = if significant {
        min_sig.unwrap_or(1).max(1)
    } else {
        min_frac
    } as usize;
    if significant && !exact_zero(ed) {
        let significant_digits = |text: &str| {
            text.chars()
                .filter(char::is_ascii_digit)
                .skip_while(|character| *character == '0')
                .count()
                .max(1)
        };
        while rounded.ends_with('0')
            && rounded.contains('.')
            && significant_digits(&rounded) > minimum
        {
            rounded.pop();
        }
    } else {
        while rounded.ends_with('0')
            && rounded.contains('.')
            && rounded.split_once('.').unwrap().1.len() > minimum
        {
            rounded.pop();
        }
    }
    if rounded.ends_with('.') {
        rounded.pop();
    }

    let (mut integer, mut fraction) = rounded
        .split_once('.')
        .map(|(integer, fraction)| (integer.to_string(), fraction.to_string()))
        .unwrap_or_else(|| (rounded, String::new()));
    if significant {
        let shown = if exact_zero(ed) {
            1 + fraction.len()
        } else {
            integer
                .chars()
                .chain(fraction.chars())
                .skip_while(|character| *character == '0')
                .count()
                .max(1)
        };
        if shown < minimum {
            fraction.push_str(&"0".repeat(minimum - shown));
        }
    } else if fraction.len() < minimum {
        fraction.push_str(&"0".repeat(minimum - fraction.len()));
    }
    if integer.len() < min_int as usize {
        integer.insert_str(0, &"0".repeat(min_int as usize - integer.len()));
    }
    Some(if fraction.is_empty() {
        integer
    } else {
        format!("{integer}.{fraction}")
    })
}

struct FormattedNumber {
    text: String,
    // Retain the selected pattern so formatToParts uses the same rounded
    // quantity as format, including placeholder-free unit forms.
    unit_pattern: Option<String>,
}

/// `exact` carries a BigInt or decimal-string input whose digits must not round through f64.
fn assemble_number_exact(
    i: &mut Interp,
    o: &Gc,
    x: f64,
    exact: Option<ExactDec>,
) -> FormattedNumber {
    let style = get_str(o, "__nf_style");
    let cldr_locale = cldr_number_locale(o);
    let number_system = get_str(o, "__nf_nu");
    let symbols = crate::cldr_numbers::symbols(cldr_locale, &number_system);
    let mut value = x;
    let mut exact = exact;
    if style == "percent" {
        value *= 100.0;
        exact = exact.as_ref().map(|decimal| shift_exact(decimal, 2));
    }
    // Negative zero counts as negative for sign display (so -0 formats as "-0").
    let negative = exact
        .as_ref()
        .map(|decimal| decimal.negative)
        .unwrap_or_else(|| value.is_sign_negative() && !value.is_nan());
    // scientific / engineering notation: mantissa in [1,10) or [1,1000), plus an exponent.
    let notation = get_str(o, "__nf_notation");
    let mut exponent: Option<i32> = None;
    if (notation == "scientific" || notation == "engineering")
        && exact.as_ref().is_some_and(|decimal| !exact_zero(decimal))
    {
        let mut e = exact_msd(exact.as_ref().unwrap());
        if notation == "engineering" {
            e -= e.rem_euclid(3);
        }
        exact = exact.as_ref().map(|decimal| shift_exact(decimal, -e));
        value = exact.as_ref().map(exact_f64).unwrap_or(value);
        exponent = Some(e);
    } else if (notation == "scientific" || notation == "engineering")
        && value != 0.0
        && value.is_finite()
    {
        let mut e = value.abs().log10().floor() as i32;
        if notation == "engineering" {
            e -= e.rem_euclid(3);
        }
        value /= 10f64.powi(e);
        // Guard against log10 rounding pushing the mantissa to 10.
        if value.abs()
            >= if notation == "engineering" {
                1000.0
            } else {
                10.0
            }
        {
            value /= 10.0;
            e += 1;
        }
        exponent = Some(e);
    }
    // ComputeExponentForMagnitude selects the CLDR compact pattern for the input magnitude; the
    // pattern's zero count determines the scaling exponent.
    let compact_input = value;
    let compact_exact_input = exact.clone();
    let mut compact: Option<(u8, crate::cldr_numbers::Compact)> = None;
    if notation == "compact" && (value.is_finite() || exact.is_some()) {
        let magnitude = exact
            .as_ref()
            .map(|decimal| exact_msd(decimal).max(0).min(u8::MAX as i32) as u8)
            .unwrap_or_else(|| {
                if value.abs() >= 1.0 {
                    value.abs().log10().floor().clamp(0.0, u8::MAX as f64) as u8
                } else {
                    0
                }
            });
        let display = get_str(o, "__nf_compactdisplay");
        if let Some(pattern) = crate::cldr_numbers::compact(
            cldr_locale,
            &number_system,
            &display,
            magnitude,
            "other",
            false,
        ) {
            if pattern.exponent != 0 {
                value /= 10f64.powi(pattern.exponent);
                exact = exact
                    .as_ref()
                    .map(|decimal| shift_exact(decimal, -pattern.exponent));
                if let Some(decimal) = &exact {
                    value = exact_f64(decimal);
                }
            }
            compact = Some((magnitude, pattern));
        }
    }
    let (mut int_part, mut frac_part) = if value.is_nan() && exact.is_none() {
        (symbols.nan.to_string(), None)
    } else if value.is_infinite() && exact.is_none() {
        (symbols.infinity.to_string(), None)
    } else {
        // Compact notation rounds with the default "morePrecision" of 2 significant / 0 fraction
        // digits: keep max(0, 2 - integerDigits) fraction digits (unless digit options were given).
        let has_sig = o.borrow().props.contains("__nf_minsig");
        let mag = if let Some(magnitude) = exact.as_ref().and_then(|ed| exact_magnitude(ed, o)) {
            magnitude
        } else if compact.is_some() && !has_sig {
            // roundingPriority "morePrecision" over (max 0 fraction) and (max 2 significant): pick
            // whichever shows more fraction digits (ties keep the integer/fraction result).
            let s_frac = round_fraction_dec(value.abs(), 0, 0, 1, "halfExpand");
            let s_sig = round_significant_dec(value.abs(), 2, 1, "halfExpand");
            let fd = |s: &str| s.split('.').nth(1).map(|f| f.len()).unwrap_or(0);
            if fd(&s_sig) > fd(&s_frac) {
                s_sig
            } else {
                s_frac
            }
        } else {
            // Pass the signed value so the sign-sensitive rounding modes (ceil/floor and their
            // half-variants) see the true sign; the returned magnitude is unsigned.
            format_magnitude(value, o)
        };
        match mag.split_once('.') {
            Some((a, b)) => (a.to_string(), Some(b.to_string())),
            None => (mag.clone(), None),
        }
    };
    // ComputeExponent repeats after rounding when a compact result crosses into the next
    // magnitude (for example 999,500 → 1M rather than 1000K).
    if let Some((magnitude, seed)) = compact {
        let rounded = match &frac_part {
            Some(fraction) => format!("{int_part}.{fraction}"),
            None => int_part.clone(),
        };
        let threshold_power = magnitude as i32 - seed.exponent + 1;
        let crossed = rounded
            .parse::<f64>()
            .is_ok_and(|number| number >= 10f64.powi(threshold_power));
        if crossed {
            let next_magnitude = magnitude.saturating_add(1);
            if let Some(next) = crate::cldr_numbers::compact(
                cldr_locale,
                &number_system,
                &get_str(o, "__nf_compactdisplay"),
                next_magnitude,
                "other",
                false,
            ) {
                exact = compact_exact_input
                    .as_ref()
                    .map(|decimal| shift_exact(decimal, -next.exponent));
                value = exact
                    .as_ref()
                    .map(exact_f64)
                    .unwrap_or_else(|| compact_input / 10f64.powi(next.exponent));
                let remade = if let Some(magnitude) = exact
                    .as_ref()
                    .and_then(|decimal| exact_magnitude(decimal, o))
                {
                    magnitude
                } else if o.borrow().props.contains("__nf_minsig") {
                    format_magnitude(value, o)
                } else {
                    let fraction = round_fraction_dec(value.abs(), 0, 0, 1, "halfExpand");
                    let significant = round_significant_dec(value.abs(), 2, 1, "halfExpand");
                    let digits =
                        |number: &str| number.split('.').nth(1).map(str::len).unwrap_or_default();
                    if digits(&significant) > digits(&fraction) {
                        significant
                    } else {
                        fraction
                    }
                };
                match remade.split_once('.') {
                    Some((integer, fraction)) => {
                        int_part = integer.to_string();
                        frac_part = Some(fraction.to_string());
                    }
                    None => {
                        int_part = remade;
                        frac_part = None;
                    }
                }
                compact = Some((next_magnitude, next));
            }
        }
    }
    // A value that rounds to zero (e.g. -0.0001 with the default 3 fraction digits) is "zero" for
    // the purpose of the `exceptZero`/`negative` sign rules, even though its sign bit is negative.
    let rounded_zero = value.is_finite()
        && int_part.chars().all(|c| c == '0')
        && frac_part.as_deref().unwrap_or("").chars().all(|c| c == '0');
    let grouping = o
        .borrow()
        .props
        .get("__nf_grouping")
        .map(|p| p.value())
        .unwrap_or(Value::str("auto"));
    // Grouping is suppressed in scientific/engineering notation.
    let grouped = if exponent.is_some() || (!value.is_finite() && exact.is_none()) {
        int_part.clone()
    } else {
        group_integer(
            &int_part,
            &grouping,
            symbols.group,
            (symbols.primary_group, symbols.secondary_group),
            symbols.minimum_grouping,
        )
    };
    let mut num = match frac_part {
        Some(ref f) => format!("{grouped}{}{f}", symbols.decimal),
        None => grouped,
    };
    if let Some(e) = exponent {
        let exponent_digits = if e < 0 {
            format!("{}{}", symbols.minus, e.unsigned_abs())
        } else {
            e.to_string()
        };
        num = format!("{num}{}{exponent_digits}", symbols.exponential);
        let scientific = crate::cldr_numbers::pattern(cldr_locale, &number_system, "scientific");
        num = format!(
            "{}{num}{}",
            scientific.positive_prefix, scientific.positive_suffix
        );
    }
    if let Some((magnitude, seed)) = compact {
        let rounded = match &frac_part {
            Some(fraction) => format!("{int_part}.{fraction}"),
            None => int_part.clone(),
        };
        let compact_exponent = seed.exponent.max(0) as u32;
        let category = crate::cldr_plurals::select_cardinal(o_lang(o), &rounded, compact_exponent);
        let exact_one = rounded
            .parse::<f64>()
            .is_ok_and(|rounded_value| rounded_value == 1.0);
        let selected = crate::cldr_numbers::compact(
            cldr_locale,
            &number_system,
            &get_str(o, "__nf_compactdisplay"),
            magnitude,
            category,
            exact_one,
        )
        .unwrap_or(seed);
        num = if selected.has_number {
            format!("{}{num}{}", selected.prefix, selected.suffix)
        } else {
            format!("{}{}", selected.prefix, selected.suffix)
        };
    }

    // Sign display. `auto`/`always` key off the sign bit (so -0 and values rounding to zero still
    // show "-0"); `exceptZero`/`negative` suppress the sign when the displayed value is zero or NaN.
    let sign_display = get_str(o, "__nf_signdisplay");
    let zeroish = rounded_zero || value.is_nan();
    let sign = match sign_display.as_str() {
        "never" => 0,
        "always" => {
            if negative {
                -1
            } else {
                1
            }
        }
        "exceptZero" => {
            if zeroish {
                0
            } else if negative {
                -1
            } else {
                1
            }
        }
        "negative" => {
            if negative && !zeroish {
                -1
            } else {
                0
            }
        }
        _ => {
            if negative {
                -1
            } else {
                0
            }
        }
    };

    let rounded = match &frac_part {
        Some(fraction) => format!("{int_part}.{fraction}"),
        None => int_part.clone(),
    };
    let compact_exponent = compact
        .map(|(_, pattern)| pattern.exponent.max(0) as u32)
        .unwrap_or(0);
    let quantity_exponent = exponent.unwrap_or(compact_exponent as i32);
    let mut unit_pattern = None;

    // GetNumberFormatPattern selects the localized sign/style affixes around the notation
    // subpattern. Currency names use CLDR's plural currency unit pattern instead of a symbol slot.
    match style.as_str() {
        "percent" => {
            num = apply_number_pattern(
                crate::cldr_numbers::pattern(cldr_locale, &number_system, "percent"),
                sign,
                &num,
                &symbols,
                "",
            );
        }
        "currency" => {
            let code = get_str(o, "__nf_currency");
            let display = get_str(o, "__nf_currencydisplay");
            let category =
                quantity_plural_category(o_lang(o), &rounded, quantity_exponent, compact_exponent);
            let currency = crate::cldr_numbers::currency(cldr_locale, &code, category);
            if display == "name" {
                let signed = apply_number_pattern(
                    crate::cldr_numbers::pattern(cldr_locale, &number_system, "decimal"),
                    sign,
                    &num,
                    &symbols,
                    "",
                );
                let name = currency.map(|value| value.name).unwrap_or(&code);
                num = crate::cldr_numbers::currency_unit(cldr_locale, category)
                    .replace("{0}", &signed)
                    .replace("{1}", name);
            } else {
                let text = match display.as_str() {
                    "code" => code.as_str(),
                    "narrowSymbol" => currency.map(|value| value.narrow).unwrap_or(&code),
                    _ => currency.map(|value| value.symbol).unwrap_or(&code),
                };
                let accounting = get_str(o, "__nf_currencysign") == "accounting";
                let base_kind = if accounting { "accounting" } else { "currency" };
                let base_pattern =
                    crate::cldr_numbers::pattern(cldr_locale, &number_system, base_kind);
                // UTS #35's alphaNextToNumber variant applies only when the edge of the
                // substituted currency string adjacent to the number is alphabetic. A symbol
                // such as `US$` contains letters but ends in `$`, so a prefix pattern must not
                // insert the alphabetic spacing used by a currency code such as `USD`.
                let alpha = if base_pattern.positive_prefix.contains("{currency}") {
                    text.chars().next_back().is_some_and(char::is_alphabetic)
                } else if base_pattern.positive_suffix.contains("{currency}") {
                    text.chars().next().is_some_and(char::is_alphabetic)
                } else {
                    false
                };
                let kind = match (accounting, alpha) {
                    (false, false) => "currency",
                    (false, true) => "currencyAlpha",
                    (true, false) => "accounting",
                    (true, true) => "accountingAlpha",
                };
                num = apply_number_pattern(
                    crate::cldr_numbers::pattern(cldr_locale, &number_system, kind),
                    sign,
                    &num,
                    &symbols,
                    text,
                );
            }
        }
        "unit" => {
            let signed = apply_number_pattern(
                crate::cldr_numbers::pattern(cldr_locale, &number_system, "decimal"),
                sign,
                &num,
                &symbols,
                "",
            );
            let category =
                quantity_plural_category(o_lang(o), &rounded, quantity_exponent, compact_exponent);
            unit_pattern = unit_pattern_for_category(o, category);
            num = match unit_pattern.as_deref() {
                Some(p) => p.replace("{0}", &signed),
                None => {
                    let unit = get_str(o, "__nf_unit");
                    let disp = get_str(o, "__nf_unitdisplay");
                    unit_wrap(&signed, &unit, &disp, category != "one")
                }
            };
        }
        _ => {
            num = apply_number_pattern(
                crate::cldr_numbers::pattern(cldr_locale, &number_system, "decimal"),
                sign,
                &num,
                &symbols,
                "",
            );
        }
    }
    let _ = i;
    FormattedNumber {
        text: num,
        unit_pattern,
    }
}

/// ECMA-402 #sec-partitionnumberpattern (snapshot b1c961988b9a) uses the
/// rounded result for locale-dependent affixes. UTS #35 Part 3 #Operands
/// (snapshot 1c6bc010ee9a) counts digits of the represented quantity, so
/// 1E4 has i=10000 and 1.20050c3 has i=1200, v=2, f=50. Move the radix
/// point exactly, retaining visible zeros and BigInt/decimal-string digits.
/// This is distinct from choosing the compact power word itself, which
/// follows UTS #35 #Compact_Number_Formats using the scaled mantissa.
fn quantity_plural_category(
    lang: &str,
    rounded: &str,
    exponent: i32,
    compact_exponent: u32,
) -> &'static str {
    let Some(decimal) = ExactDec::parse(rounded) else {
        return "other"; // NaN and infinities do not have plural operands.
    };
    let quantity = shift_exact(&decimal, exponent);
    let digits = if quantity.frac.is_empty() {
        quantity.int
    } else {
        format!("{}.{}", quantity.int, quantity.frac)
    };
    crate::cldr_plurals::select_cardinal(lang, &digits, compact_exponent)
}

fn apply_number_pattern(
    pattern: crate::cldr_numbers::Pattern,
    sign: i8,
    number: &str,
    symbols: &crate::cldr_numbers::Symbols,
    currency: &str,
) -> String {
    let (mut prefix, suffix) = if sign < 0 {
        (pattern.negative_prefix.to_string(), pattern.negative_suffix)
    } else {
        (pattern.positive_prefix.to_string(), pattern.positive_suffix)
    };
    if sign > 0 {
        // CLDR derives the plus pattern from the positive pattern. Keep leading bidi controls ahead
        // of the localized plus sign so Arabic and other RTL affixes remain well-ordered.
        let byte = prefix
            .char_indices()
            .find(|(_, character)| !matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}'))
            .map(|(index, _)| index)
            .unwrap_or(prefix.len());
        prefix.insert_str(byte, "{plusSign}");
    }
    format!(
        "{}{number}{}",
        render_number_affix(&prefix, symbols, currency),
        render_number_affix(suffix, symbols, currency)
    )
}

fn render_number_affix(
    affix: &str,
    symbols: &crate::cldr_numbers::Symbols,
    currency: &str,
) -> String {
    affix
        .replace("{currency}", currency)
        .replace("{percentSign}", symbols.percent)
        .replace("{minusSign}", symbols.minus)
        .replace("{plusSign}", symbols.plus)
}

fn unit_short_name(u: &str) -> Option<&'static str> {
    Some(match u {
        "kilometer-per-hour" => "km/h",
        "meter" => "m",
        "kilometer" => "km",
        "centimeter" => "cm",
        "percent" => "%",
        "liter" => "L",
        "kilobyte" => "kB",
        "megabyte" => "MB",
        "celsius" => "°C",
        "fahrenheit" => "°F",
        _ => return None,
    })
}

fn unit_wrap(num: &str, unit: &str, display: &str, plural: bool) -> String {
    if display == "long" {
        return format!("{num} {}", unit_long_en(unit, plural));
    }
    let name = unit_short_name(unit).unwrap_or(unit);
    // The narrow style attaches the unit with no space ("987km/h"); short keeps a space.
    if display == "narrow" {
        format!("{num}{name}")
    } else {
        format!("{num} {name}")
    }
}

/// The English long display name of a sanctioned unit (regular +s plural; "X-per-Y" pluralizes the
/// numerator and keeps a singular denominator).
fn unit_long_en(unit: &str, plural: bool) -> String {
    if let Some((a, b)) = unit.split_once("-per-") {
        return format!("{} per {}", unit_long_en(a, plural), unit_long_en(b, false));
    }
    let name = unit.replace('-', " ");
    if plural {
        format!("{name}s")
    } else {
        name
    }
}

/// Replace the ASCII digits of a formatted number with the glyphs of numbering system `nu` (a no-op
/// for `latn` or an unknown system).
pub(crate) fn xlate_digits(s: &str, nu: &str) -> String {
    if nu == "latn" {
        return s.to_string();
    }
    match crate::numbering::NUMBERING.iter().find(|(id, _)| *id == nu) {
        Some((_, glyphs)) => s
            .chars()
            .map(|c| {
                if c.is_ascii_digit() {
                    glyphs[c as usize - '0' as usize]
                } else {
                    c
                }
            })
            .collect(),
        None => s.to_string(),
    }
}

fn format_number(i: &mut Interp, this: &Value, x: &Value) -> Result<Value, Value> {
    let o = instance(i, this)?;
    let n = to_intl_number(i, x)?;
    let s = assemble_number_exact(i, &o, n, exact_of(x)).text;
    Ok(Value::from_string(xlate_digits(
        &s,
        &get_str(&o, "__nf_nu"),
    )))
}

/// The trailing-affix classification for this formatter's parts (compact suffix vs plain).
fn suffix_type_of(o: &Gc) -> String {
    if get_str(o, "__nf_style") == "unit" {
        "unit".to_string()
    } else if get_str(o, "__nf_style") == "currency" {
        "currency".to_string()
    } else if get_str(o, "__nf_notation") == "compact" {
        "compact".to_string()
    } else {
        "literal".to_string()
    }
}

pub(super) fn to_intl_number(i: &mut Interp, x: &Value) -> Result<f64, Value> {
    // ToIntlMathematicalValue — we approximate with ToNumber (BigInt handled as its value).
    match x {
        Value::BigInt(b) => Ok(b.to_f64()),
        _ => ab(i.to_number(x)),
    }
}

fn format_to_parts(i: &mut Interp, this: &Value, x: &Value) -> Result<Value, Value> {
    let o = instance(i, this)?;
    let n = to_intl_number(i, x)?;
    let FormattedNumber {
        text: whole,
        unit_pattern,
    } = assemble_number_exact(i, &o, n, exact_of(x));
    let nu = get_str(&o, "__nf_nu");
    let symbols = cldr_number_symbols(&o);
    // Unit style: rebuild from the CLDR pattern so a unit prefix/suffix (e.g. ko "시속 {0}킬로미터")
    // is tagged as unit/literal around the number's own parts.
    let parts = if get_str(&o, "__nf_style") == "unit" {
        if let Some(pat) = unit_pattern {
            if !pat.contains("{0}") {
                vec![("unit", whole)]
            } else {
                let (pre, post) = pat.split_once("{0}").unwrap();
                let num_only = whole
                    .strip_prefix(pre)
                    .and_then(|s| s.strip_suffix(post))
                    .unwrap_or(&whole);
                let mut p = unit_affix_parts(pre, true);
                p.extend(decompose_parts(num_only, "literal", &symbols));
                p.extend(unit_affix_parts(post, false));
                p
            }
        } else {
            decompose_parts(&whole, &suffix_type_of(&o), &symbols)
        }
    } else {
        decompose_parts(&whole, &suffix_type_of(&o), &symbols)
    };
    let arr: Vec<Value> = parts
        .into_iter()
        .map(|(t, v)| {
            let ob = i.new_object();
            set_data(&ob, "type", Value::str(t));
            // Localize the digits of numeric parts to the numbering system.
            let v = if matches!(t, "integer" | "fraction" | "exponentInteger") {
                xlate_digits(&v, &nu)
            } else {
                v
            };
            set_data(&ob, "value", Value::from_string(v));
            Value::Obj(ob)
        })
        .collect();
    Ok(i.make_array(arr))
}

/// Break an assembled number string into typed parts (minusSign/plusSign, currency/percentSign/
/// literal affixes, grouped integer, decimal, fraction, and the exponent group). `suffix_type`
/// classifies the trailing affix ("compact" for compact notation, else percent/literal by content).
/// The formatter locale's primary language subtag.
fn o_lang(o: &Gc) -> &'static str {
    cldr_number_locale(o).split('-').next().unwrap_or("en")
}

/// The CLDR unit-display pattern ("{0} km/h") for the already-selected plural
/// category; zh is split by script and en-IN has region-specific patterns.
fn unit_pattern_for_category(o: &Gc, category: &str) -> Option<String> {
    let unit = get_str(o, "__nf_unit");
    let disp = get_str(o, "__nf_unitdisplay");
    let style = if disp.is_empty() {
        "short"
    } else {
        disp.as_str()
    };
    let locale = cldr_number_locale(o);
    let cldr_loc = if locale == "zh" { "zh-Hans" } else { locale };
    let lang = o_lang(o);
    crate::cldr_units::unit_pattern(cldr_loc, &unit, style, category)
        .or_else(|| crate::cldr_units::unit_pattern(cldr_loc, &unit, style, "other"))
        .or_else(|| crate::cldr_units::unit_pattern(lang, &unit, style, "other"))
        .map(|s| s.to_string())
}

/// Tokenize a unit pattern's pre/post affix into (unit, literal) parts: the unit name is the
/// non-space run, adjoining spaces are literals (a `pre` affix ends with the separator, a `post`
/// affix begins with it).
fn unit_affix_parts(text: &str, is_pre: bool) -> Vec<(&'static str, String)> {
    if text.is_empty() {
        return Vec::new();
    }
    let sp = |c: char| c == ' ' || c == '\u{a0}';
    let mut v = Vec::new();
    if is_pre {
        let unit = text.trim_end_matches(sp);
        if !unit.is_empty() {
            v.push(("unit", unit.to_string()));
        }
        let tail = &text[unit.len()..];
        if !tail.is_empty() {
            v.push(("literal", tail.to_string()));
        }
    } else {
        let unit = text.trim_start_matches(sp);
        let lead = &text[..text.len() - unit.len()];
        if !lead.is_empty() {
            v.push(("literal", lead.to_string()));
        }
        if !unit.is_empty() {
            v.push(("unit", unit.to_string()));
        }
    }
    v
}

fn decompose_parts(
    s: &str,
    suffix_type: &str,
    symbols: &crate::cldr_numbers::Symbols,
) -> Vec<(&'static str, String)> {
    let mut parts: Vec<(&'static str, String)> = Vec::new();
    let numeric_start = s
        .char_indices()
        .find(|(index, character)| {
            character.is_ascii_digit()
                || s[*index..].starts_with(symbols.infinity)
                || s[*index..].starts_with(symbols.nan)
        })
        .map(|(index, _)| index);
    let Some(mut idx) = numeric_start else {
        // CLDR has placeholder-free compact/unit forms such as French `mille` and Arabic `متر`.
        push_affix_parts(&mut parts, s, suffix_type, symbols);
        return parts;
    };
    push_affix_parts(&mut parts, &s[..idx], suffix_type, symbols);

    // Non-finite body: emit a single infinity/nan part, then fall through to any trailing affix.
    if s[idx..].starts_with(symbols.infinity) {
        push_part(&mut parts, "infinity", symbols.infinity);
        idx += symbols.infinity.len();
    } else if s[idx..].starts_with(symbols.nan) {
        push_part(&mut parts, "nan", symbols.nan);
        idx += symbols.nan.len();
    }

    // Integer digits and grouping separators. Generated CLDR symbols are strings rather than
    // assumed one-byte punctuation, so token matching stays correct for every numbering system.
    let mut integer = String::new();
    while idx < s.len() {
        let rest = &s[idx..];
        if let Some(character) = rest.chars().next().filter(char::is_ascii_digit) {
            integer.push(character);
            idx += character.len_utf8();
        } else if rest.starts_with(symbols.group) {
            if !integer.is_empty() {
                push_part(&mut parts, "integer", &integer);
                integer.clear();
            }
            push_part(&mut parts, "group", symbols.group);
            idx += symbols.group.len();
        } else {
            break;
        }
    }
    if !integer.is_empty() {
        push_part(&mut parts, "integer", &integer);
    }
    // Decimal + fraction.
    if idx < s.len() && s[idx..].starts_with(symbols.decimal) {
        push_part(&mut parts, "decimal", symbols.decimal);
        idx += symbols.decimal.len();
        let start = idx;
        while idx < s.len() {
            let Some(character) = s[idx..].chars().next() else {
                break;
            };
            if !character.is_ascii_digit() {
                break;
            }
            idx += character.len_utf8();
        }
        push_part(&mut parts, "fraction", &s[start..idx]);
    }

    // Scientific/engineering exponent, followed by any style affix (currency/unit/percent).
    if idx < s.len() && s[idx..].starts_with(symbols.exponential) {
        push_part(&mut parts, "exponentSeparator", symbols.exponential);
        idx += symbols.exponential.len();
        if s[idx..].starts_with(symbols.minus) {
            push_symbol_parts(&mut parts, symbols.minus, "exponentMinusSign");
            idx += symbols.minus.len();
        } else if s[idx..].starts_with(symbols.plus) {
            push_symbol_parts(&mut parts, symbols.plus, "exponentPlusSign");
            idx += symbols.plus.len();
        }
        let start = idx;
        while idx < s.len() {
            let Some(character) = s[idx..].chars().next() else {
                break;
            };
            if !character.is_ascii_digit() {
                break;
            }
            idx += character.len_utf8();
        }
        push_part(&mut parts, "exponentInteger", &s[start..idx]);
    }
    push_affix_parts(&mut parts, &s[idx..], suffix_type, symbols);
    parts
}

fn is_affix_literal(character: char) -> bool {
    character.is_whitespace()
        || matches!(character, '(' | ')' | '\u{061c}' | '\u{200e}' | '\u{200f}')
}

fn push_part(parts: &mut Vec<(&'static str, String)>, kind: &'static str, value: &str) {
    if value.is_empty() {
        return;
    }
    if let Some((last_kind, last_value)) = parts.last_mut() {
        if *last_kind == kind && kind == "literal" {
            last_value.push_str(value);
            return;
        }
    }
    parts.push((kind, value.to_string()));
}

/// Split directional controls out of a localized sign/percent symbol. ECMA-402 emits the visible
/// field as the semantic part while the controls remain literal parts preserving bidi ordering.
fn push_symbol_parts(parts: &mut Vec<(&'static str, String)>, symbol: &str, kind: &'static str) {
    let mut run = String::new();
    let mut run_literal = None;
    for character in symbol.chars() {
        let literal = matches!(character, '\u{061c}' | '\u{200e}' | '\u{200f}');
        if run_literal.is_some_and(|previous| previous != literal) {
            push_part(
                parts,
                if run_literal.unwrap() {
                    "literal"
                } else {
                    kind
                },
                &run,
            );
            run.clear();
        }
        run_literal = Some(literal);
        run.push(character);
    }
    if !run.is_empty() {
        push_part(
            parts,
            if run_literal.unwrap() {
                "literal"
            } else {
                kind
            },
            &run,
        );
    }
}

/// Tokenize a prefix/suffix from a CLDR pattern. Spaces, parentheses and bidi controls are
/// literals; the remaining field is currency/unit/compact data. Localized signs and percent signs
/// are recognized even when their CLDR symbol contains directional controls.
fn push_affix_parts(
    parts: &mut Vec<(&'static str, String)>,
    mut text: &str,
    semantic: &str,
    symbols: &crate::cldr_numbers::Symbols,
) {
    while !text.is_empty() {
        if text.starts_with(symbols.minus) {
            push_symbol_parts(parts, symbols.minus, "minusSign");
            text = &text[symbols.minus.len()..];
            continue;
        }
        if text.starts_with(symbols.plus) {
            push_symbol_parts(parts, symbols.plus, "plusSign");
            text = &text[symbols.plus.len()..];
            continue;
        }
        if text.starts_with(symbols.percent) {
            push_symbol_parts(parts, symbols.percent, "percentSign");
            text = &text[symbols.percent.len()..];
            continue;
        }
        let first = text.chars().next().unwrap();
        if is_affix_literal(first) {
            let end = text
                .char_indices()
                .find(|(_, character)| !is_affix_literal(*character))
                .map(|(index, _)| index)
                .unwrap_or(text.len());
            push_part(parts, "literal", &text[..end]);
            text = &text[end..];
            continue;
        }
        let end = text
            .char_indices()
            .skip(1)
            .find(|(index, character)| {
                is_affix_literal(*character)
                    || text[*index..].starts_with(symbols.minus)
                    || text[*index..].starts_with(symbols.plus)
                    || text[*index..].starts_with(symbols.percent)
            })
            .map(|(index, _)| index)
            .unwrap_or(text.len());
        let kind = match semantic {
            "currency" => "currency",
            "unit" => "unit",
            "compact" => "compact",
            _ => "literal",
        };
        push_part(parts, kind, &text[..end]);
        text = &text[end..];
    }
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = instance_unwrap(i, &this)?;
    let res = i.new_object();
    let put = |i: &mut Interp, res: &Gc, k: &str, slot: &str| {
        if let Some(v) = o.borrow().props.get(slot).map(|p| p.value()) {
            set_data(res, k, v);
        }
        let _ = i;
    };
    put(i, &res, "locale", "__nf_locale");
    put(i, &res, "numberingSystem", "__nf_nu");
    put(i, &res, "style", "__nf_style");
    put(i, &res, "currency", "__nf_currency");
    put(i, &res, "currencyDisplay", "__nf_currencydisplay");
    put(i, &res, "currencySign", "__nf_currencysign");
    put(i, &res, "unit", "__nf_unit");
    put(i, &res, "unitDisplay", "__nf_unitdisplay");
    put(i, &res, "minimumIntegerDigits", "__nf_minint");
    {
        // Spec key order: fraction digits, then significant digits. Under a *Precision rounding
        // type (e.g. the compact-notation default) BOTH pairs are present.
        let rtype = get_str(&o, "__nf_roundingtype");
        let has_sig = o.borrow().props.contains("__nf_minsig");
        if !has_sig || rtype.contains("Precision") {
            put(i, &res, "minimumFractionDigits", "__nf_minfrac");
            put(i, &res, "maximumFractionDigits", "__nf_maxfrac");
        }
        if has_sig {
            put(i, &res, "minimumSignificantDigits", "__nf_minsig");
            put(i, &res, "maximumSignificantDigits", "__nf_maxsig");
        }
    }
    // useGrouping/notation/compactDisplay/signDisplay precede the rounding options in key order.
    set_data(
        &res,
        "useGrouping",
        o.borrow()
            .props
            .get("__nf_grouping")
            .map(|p| p.value())
            .unwrap_or(Value::str("auto")),
    );
    put(i, &res, "notation", "__nf_notation");
    // compactDisplay only appears when notation is compact.
    if matches!(o.borrow().props.get("__nf_notation").map(|p| p.value()), Some(Value::Str(s)) if &*s == "compact")
    {
        put(i, &res, "compactDisplay", "__nf_compactdisplay");
    }
    put(i, &res, "signDisplay", "__nf_signdisplay");
    put(i, &res, "roundingIncrement", "__nf_roundingincrement");
    put(i, &res, "roundingMode", "__nf_roundingmode");
    put(i, &res, "roundingPriority", "__nf_roundingpriority");
    put(i, &res, "trailingZeroDisplay", "__nf_trailingzero");
    Ok(Value::Obj(res))
}

#[cfg(test)]
mod tests {
    use crate::{bytecode::Tier, Completion, Engine};

    fn check_all_tiers(source: &str) {
        for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
            let mut engine = Engine::new();
            engine.set_tier(tier);
            engine.set_tier_threshold(0);
            match engine.eval(source, false).expect("valid test script") {
                Completion::Value(value) => assert_eq!(value, "ok"),
                Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
            }
        }
    }

    #[test]
    fn numberformat_plural_quantities_preserve_notation_exponents() {
        // ECMA-402 PartitionNumberPattern and UTS #35 Part 3, Operands:
        // the quantity 1E4 has i=10000, not i=1. Currency/unit names use
        // this quantity; the separate compact power word uses its mantissa.
        check_all_tiers(
            r#"
            function check(actual, expected) {
                if (actual !== expected) throw Error(JSON.stringify({actual, expected}));
            }
            for (const [notation, value, expected] of [
                ['scientific', 10000, '1E4 bits'],
                ['scientific', -10000, '-1E4 bits'],
                ['scientific', .1, '1E-1 bits'],
                ['scientific', 1, '1E0 bit'],
                ['engineering', 1000, '1E3 bits'],
                ['engineering', .1, '100E-3 bits'],
                ['compact', 1000, '1K bits'],
                ['compact', 1000000, '1M bits']
            ]) {
                const format = new Intl.NumberFormat('en', {
                    notation, style: 'unit', unit: 'bit', unitDisplay: 'long'
                });
                check(format.format(value), expected);
                check(format.formatToParts(value).map(p => p.value).join(''), expected);
                check(format.formatToParts(value).filter(p => p.type === 'unit').length, 1);
            }
            for (const notation of ['scientific', 'engineering', 'compact']) {
                const format = new Intl.NumberFormat('en', {
                    notation, style: 'currency', currency: 'USD', currencyDisplay: 'name',
                    minimumFractionDigits: 0, maximumFractionDigits: 0
                });
                const prefix = notation === 'compact' ? '1K' : '1E3';
                check(format.format(1000), prefix + ' US dollars');
            }
            check(new Intl.NumberFormat('ru', {notation: 'compact', compactDisplay: 'long'}).format(2000), '2 тысячи');
            'ok';
        "#,
        );
    }

    #[test]
    fn numberformat_unit_plurals_share_rounded_digits_with_parts() {
        // ECMA-402 #sec-partitionnumberpattern; UTS #35 #Unit_Elements
        // and #Plural_Operand_Meanings: visible zeros and exact integer
        // digits participate in plural selection after rounding.
        check_all_tiers(
            r#"
            function check(actual, expected) {
                if (actual !== expected) throw Error(JSON.stringify({actual, expected}));
            }
            for (const [value, options, expected] of [
                [1.2, {maximumFractionDigits: 0}, '1 bit'],
                [.99, {maximumFractionDigits: 0}, '1 bit'],
                [1.99, {maximumFractionDigits: 0}, '2 bits'],
                [1, {minimumFractionDigits: 1}, '1.0 bits'],
                [-1, {}, '-1 bit'],
                [NaN, {}, 'NaN bits'],
                [Infinity, {}, '∞ bits']
            ]) {
                const format = new Intl.NumberFormat('en', {
                    style: 'unit', unit: 'bit', unitDisplay: 'long', ...options
                });
                check(format.format(value), expected);
                const parts = format.formatToParts(value);
                check(parts.map(p => p.value).join(''), expected);
                check(parts.filter(p => p.type === 'unit').length, 1);
            }
            const ru = new Intl.NumberFormat('ru', {style: 'unit', unit: 'meter', unitDisplay: 'long', useGrouping: false});
            const large = 100000000000000000000000000000000000000000000000000000000000000021n;
            check(ru.format(large), large.toString() + ' метр');
            check(ru.formatToParts(large).map(p => p.value).join(''), ru.format(large));
            const ar = new Intl.NumberFormat('ar', {style: 'unit', unit: 'meter', unitDisplay: 'long', maximumFractionDigits: 0});
            check(ar.format(.99), 'متر');
            check(JSON.stringify(ar.formatToParts(.99)), '[{"type":"unit","value":"متر"}]');
            const sl = new Intl.NumberFormat('sl', {style: 'unit', unit: 'meter', unitDisplay: 'long', minimumFractionDigits: 2});
            check(sl.format(1), '1,00 metri');
            check(sl.formatToParts(1).map(p => p.value).join(''), '1,00 metri');
            'ok';
        "#,
        );
    }
}
