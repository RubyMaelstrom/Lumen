//! `Intl.PluralRules` following ECMA-402 ResolvePlural with CLDR 48 cardinal, ordinal, and range
//! rules for every locale Lumen advertises.

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
    let (ctor, proto) = make_service(it, ns, "PluralRules", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "select", 1, |i, this, args| {
        select(i, &this, &arg(args, 0))
    });
    it.def_method(&proto, "selectRange", 2, |i, this, args| {
        select_range(i, &this, &arg(args, 0), &arg(args, 1))
    });
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
}

fn construct(i: &mut Interp, _this: Value, args: &[Value]) -> Result<Value, Value> {
    if !i.constructing {
        return Err(i.make_error("TypeError", "Intl.PluralRules requires 'new'"));
    }
    let requested = canonicalize_locale_list(i, &arg(args, 0))?;
    let options = coerce_options(i, &arg(args, 1))?;
    read_locale_matcher(i, &options)?;
    let kind = get_option(
        i,
        &options,
        "type",
        &["cardinal", "ordinal"],
        Some("cardinal"),
    )?
    .unwrap();
    let notation = get_option(
        i,
        &options,
        "notation",
        &["standard", "scientific", "engineering", "compact"],
        Some("standard"),
    )?
    .unwrap();
    // ECMA-402 reads compactDisplay even when notation is not compact; it is only omitted from
    // resolvedOptions in that case.
    let compact_display = get_option(
        i,
        &options,
        "compactDisplay",
        &["short", "long"],
        Some("short"),
    )?
    .unwrap();

    let raw_digits = super::numberformat::read_raw_digits(i, &options)?;
    let rounding_increment = {
        let value = ab(i.get_member(&options, "roundingIncrement"))?;
        if matches!(value, Value::Undefined) {
            1
        } else {
            let number = ab(i.to_number(&value))?;
            const ALLOWED: &[u32] = &[
                1, 2, 5, 10, 20, 25, 50, 100, 200, 250, 500, 1000, 2000, 2500, 5000,
            ];
            if !number.is_finite() || number.fract() != 0.0 || !ALLOWED.contains(&(number as u32)) {
                return Err(i.make_error("RangeError", "invalid roundingIncrement"));
            }
            number as u32
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
    let digits = super::numberformat::interpret_digits(
        i,
        &raw_digits,
        "decimal",
        &notation,
        &rounding_priority,
        0,
        rounding_increment,
    )?;
    if rounding_increment != 1 {
        if digits.rounding_type != "fraction" {
            return Err(i.make_error(
                "TypeError",
                "roundingIncrement requires fraction-digits rounding",
            ));
        }
        if digits.min_frac != digits.max_frac {
            return Err(i.make_error(
                "RangeError",
                "maximumFractionDigits must equal minimumFractionDigits with roundingIncrement",
            ));
        }
    }
    let trailing_zero_display = get_option(
        i,
        &options,
        "trailingZeroDisplay",
        &["auto", "stripIfInteger"],
        Some("auto"),
    )?
    .unwrap();
    let resolved = resolve_locale(i, &requested, &[]);

    let object = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.PluralRules")? {
        object.borrow_mut().proto = Some(proto);
    }
    set_builtin(&object, "__pr", Value::Bool(true));
    set_builtin(&object, "__pr_locale", Value::from_string(resolved.locale));
    set_builtin(&object, "__pr_type", Value::from_string(kind));
    set_builtin(&object, "__pr_notation", Value::from_string(notation));
    set_builtin(
        &object,
        "__pr_compactdisplay",
        Value::from_string(compact_display),
    );
    set_builtin(&object, "__pr_minint", Value::Num(digits.min_int as f64));
    set_builtin(&object, "__pr_minfrac", Value::Num(digits.min_frac as f64));
    set_builtin(&object, "__pr_maxfrac", Value::Num(digits.max_frac as f64));
    if let Some(value) = digits.min_sig {
        set_builtin(&object, "__pr_minsig", Value::Num(value as f64));
    }
    if let Some(value) = digits.max_sig {
        set_builtin(&object, "__pr_maxsig", Value::Num(value as f64));
    }
    set_builtin(
        &object,
        "__pr_roundingincrement",
        Value::Num(rounding_increment as f64),
    );
    set_builtin(
        &object,
        "__pr_roundingmode",
        Value::from_string(rounding_mode),
    );
    set_builtin(
        &object,
        "__pr_roundingpriority",
        Value::from_string(rounding_priority),
    );
    set_builtin(
        &object,
        "__pr_roundingtype",
        Value::str(digits.rounding_type),
    );
    set_builtin(
        &object,
        "__pr_trailingzero",
        Value::from_string(trailing_zero_display),
    );
    Ok(Value::Obj(object))
}

fn string_slot(object: &Gc, key: &str) -> String {
    match object
        .borrow()
        .props
        .get(key)
        .map(|property| property.value())
    {
        Some(Value::Str(value)) => value.to_string(),
        _ => String::new(),
    }
}

fn number_slot(object: &Gc, key: &str) -> Option<u32> {
    match object
        .borrow()
        .props
        .get(key)
        .map(|property| property.value())
    {
        Some(Value::Num(value)) => Some(value as u32),
        _ => None,
    }
}

fn compact_exponent(decimal: &str) -> u32 {
    let decimal = decimal.strip_prefix(['+', '-']).unwrap_or(decimal);
    let integer = decimal
        .split_once('.')
        .map_or(decimal, |(integer, _)| integer);
    let significant = integer.trim_start_matches('0');
    if significant.len() < 4 {
        0
    } else {
        ((significant.len() - 1) / 3 * 3) as u32
    }
}

/// ECMA-402 ResolvePlural: round first through FormatNumericToString, then apply the locale's
/// cardinal/ordinal rule to that decimal string. This ordering makes visible trailing zeros and
/// digit options participate in CLDR's v/f/t operands.
fn resolve_plural(
    i: &mut Interp,
    object: &Gc,
    value: &Value,
) -> Result<(&'static str, String), Value> {
    let exact = super::numberformat::exact_of(value);
    let number = super::numberformat::to_intl_number(i, value)?;
    if number.is_nan() {
        return Ok(("other", "NaN".to_string()));
    }
    if number.is_infinite() && exact.is_none() {
        return Ok((
            "other",
            if number.is_sign_negative() {
                "-Infinity"
            } else {
                "Infinity"
            }
            .to_string(),
        ));
    }

    let min_int = number_slot(object, "__pr_minint").unwrap_or(1);
    let min_frac = number_slot(object, "__pr_minfrac").unwrap_or(0);
    let max_frac = number_slot(object, "__pr_maxfrac").unwrap_or(3);
    let min_sig = number_slot(object, "__pr_minsig");
    let max_sig = number_slot(object, "__pr_maxsig");
    let increment = number_slot(object, "__pr_roundingincrement").unwrap_or(1);
    let mode = string_slot(object, "__pr_roundingmode");
    let rounding_type = string_slot(object, "__pr_roundingtype");
    let notation = string_slot(object, "__pr_notation");
    let decimal = exact
        .as_ref()
        .and_then(|exact| {
            // FormatNumericToString itself is notation-independent. Passing standard here allows
            // exact BigInt/decimal digits while the already-resolved compact digit defaults still
            // control rounding.
            super::numberformat::exact_magnitude_options(
                exact,
                min_int,
                min_frac,
                max_frac,
                min_sig,
                max_sig,
                increment,
                &mode,
                &rounding_type,
                "standard",
            )
        })
        .unwrap_or_else(|| {
            super::numberformat::format_magnitude_options(
                number,
                min_int,
                min_frac,
                max_frac,
                min_sig,
                max_sig,
                increment,
                &mode,
                &rounding_type,
            )
        });
    let exponent = if notation == "compact" {
        compact_exponent(&decimal)
    } else {
        0
    };
    let locale = string_slot(object, "__pr_locale");
    let language = locale.split('-').next().unwrap_or("en");
    let category = if string_slot(object, "__pr_type") == "ordinal" {
        crate::cldr_plurals::select_ordinal(language, &decimal, exponent)
    } else {
        crate::cldr_plurals::select_cardinal(language, &decimal, exponent)
    };
    Ok((category, decimal))
}

fn select(i: &mut Interp, this: &Value, value: &Value) -> Result<Value, Value> {
    let object = brand_slot(i, this, "__pr")?;
    let (category, _) = resolve_plural(i, &object, value)?;
    Ok(Value::str(category))
}

fn select_range(i: &mut Interp, this: &Value, start: &Value, end: &Value) -> Result<Value, Value> {
    let object = brand_slot(i, this, "__pr")?;
    if matches!(start, Value::Undefined) || matches!(end, Value::Undefined) {
        return Err(i.make_error("TypeError", "selectRange requires two values"));
    }
    let (start_category, start_decimal) = resolve_plural(i, &object, start)?;
    let (end_category, end_decimal) = resolve_plural(i, &object, end)?;
    if start_decimal == "NaN" || end_decimal == "NaN" {
        return Err(i.make_error("RangeError", "selectRange arguments must not be NaN"));
    }
    // ResolvePluralRange returns the first category immediately when both formatted strings are
    // equal; only distinct formatted endpoints enter the CLDR plural-range table.
    if start_decimal == end_decimal {
        return Ok(Value::str(start_category));
    }
    let locale = string_slot(&object, "__pr_locale");
    let language = locale.split('-').next().unwrap_or("en");
    Ok(Value::str(crate::cldr_plurals::select_range(
        language,
        start_category,
        end_category,
    )))
}

fn resolved_options(i: &mut Interp, this: Value, _args: &[Value]) -> Result<Value, Value> {
    let object = brand_slot(i, &this, "__pr")?;
    let result = i.new_object();
    let get = |key: &str| {
        object
            .borrow()
            .props
            .get(key)
            .map(|property| property.value())
            .unwrap_or(Value::Undefined)
    };
    set_data(&result, "locale", get("__pr_locale"));
    set_data(&result, "type", get("__pr_type"));
    set_data(&result, "notation", get("__pr_notation"));
    if matches!(get("__pr_notation"), Value::Str(value) if &*value == "compact") {
        set_data(&result, "compactDisplay", get("__pr_compactdisplay"));
    }
    set_data(&result, "minimumIntegerDigits", get("__pr_minint"));
    set_data(&result, "minimumFractionDigits", get("__pr_minfrac"));
    set_data(&result, "maximumFractionDigits", get("__pr_maxfrac"));
    if !matches!(get("__pr_minsig"), Value::Undefined) {
        set_data(&result, "minimumSignificantDigits", get("__pr_minsig"));
        set_data(&result, "maximumSignificantDigits", get("__pr_maxsig"));
    }
    let locale = string_slot(&object, "__pr_locale");
    let language = locale.split('-').next().unwrap_or("en");
    let kind = string_slot(&object, "__pr_type");
    let categories = crate::cldr_plurals::categories(language, &kind)
        .iter()
        .map(|category| Value::str(*category))
        .collect();
    set_data(&result, "pluralCategories", i.make_array(categories));
    set_data(&result, "roundingIncrement", get("__pr_roundingincrement"));
    set_data(&result, "roundingMode", get("__pr_roundingmode"));
    set_data(&result, "roundingPriority", get("__pr_roundingpriority"));
    set_data(&result, "trailingZeroDisplay", get("__pr_trailingzero"));
    Ok(Value::Obj(result))
}
