//! ECMA-402 `Intl.DateTimeFormat`, backed by generated CLDR locale patterns and calendar names.

use super::service::{
    brand_slot, get_option, install_supported_locales, instance_proto, read_locale_matcher,
    resolve_locale,
};
use super::{ab, arg, canonicalize_locale_list, coerce_options, make_service};
use crate::interpreter::Interp;
use crate::value::{set_builtin, set_data, Gc, Value};
use std::rc::Rc;

/// Days since the Unix epoch for a proleptic-Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// The epoch-milliseconds a Temporal value formats at (ISO calendar, UTC).
fn temporal_to_ms(t: &crate::temporal::Temporal) -> f64 {
    use crate::temporal::Temporal as T;
    let date_ms = |d: &crate::temporal::IsoDate| {
        days_from_civil(d.year, d.month as i64, d.day as i64) as f64 * 86_400_000.0
    };
    let time_ms = |t: &crate::temporal::IsoTime| {
        (t.hour as f64) * 3_600_000.0
            + (t.minute as f64) * 60_000.0
            + (t.second as f64) * 1000.0
            + (t.ms as f64)
    };
    match t {
        T::Date(d) | T::YearMonth(d) | T::MonthDay(d) => date_ms(d),
        T::DateTime(d, tm) => date_ms(d) + time_ms(tm),
        T::Time(tm) => time_ms(tm),
        T::Instant(ns) => (*ns / 1_000_000) as f64,
        // A ZonedDateTime formats at its *local* wall-clock time (epoch + offset), since our
        // DateTimeFormat renders components in UTC.
        T::Zoned {
            epoch_ns,
            offset_ns,
            ..
        } => ((*epoch_ns + *offset_ns as i128) / 1_000_000) as f64,
        T::Duration(_) => 0.0,
    }
}

pub fn install(it: &mut Interp, ns: &Gc) {
    let (ctor, proto) = make_service(it, ns, "DateTimeFormat", 0, construct);
    install_supported_locales(it, &ctor);
    it.def_method(&proto, "formatToParts", 1, |i, this, a| {
        let o = brand_slot(i, &this, "__dtf")?;
        let (ms, kind) = dtf_ms_kind(i, &o, &arg(a, 0))?;
        let nu = dtf_nu(&o);
        let parts = build_parts(&o, ms, kind);
        let arr: Vec<Value> = parts
            .into_iter()
            .map(|(t, v)| {
                let ob = i.new_object();
                set_data(&ob, "type", Value::str(t));
                // Localize digits to the numbering system (name/literal parts have no ASCII digits).
                set_data(
                    &ob,
                    "value",
                    Value::from_string(crate::intl::numberformat::xlate_digits(&v, &nu)),
                );
                Value::Obj(ob)
            })
            .collect();
        Ok(i.make_array(arr))
    });
    it.def_method(&proto, "resolvedOptions", 0, resolved_options);
    it.def_method(&proto, "formatRange", 2, |i, this, a| {
        let o = brand_slot(i, &this, "__dtf")?;
        let (s, e, kind) = range_dates(i, &o, &arg(a, 0), &arg(a, 1))?;
        let out: String = build_range_parts(&o, s, e, kind)
            .into_iter()
            .map(|(_, value, _)| value)
            .collect();
        Ok(Value::from_string(crate::intl::numberformat::xlate_digits(
            &out,
            &dtf_nu(&o),
        )))
    });
    it.def_method(&proto, "formatRangeToParts", 2, |i, this, a| {
        let o = brand_slot(i, &this, "__dtf")?;
        let (s, e, kind) = range_dates(i, &o, &arg(a, 0), &arg(a, 1))?;
        let nu = dtf_nu(&o);
        let mut arr: Vec<Value> = Vec::new();
        let emit = |i: &mut Interp, arr: &mut Vec<Value>, ty: &str, val: &str, src: &str| {
            let ob = i.new_object();
            set_data(&ob, "type", Value::str(ty));
            set_data(
                &ob,
                "value",
                Value::from_string(crate::intl::numberformat::xlate_digits(val, &nu)),
            );
            set_data(&ob, "source", Value::str(src));
            arr.push(Value::Obj(ob));
        };
        for (kind, value, source) in build_range_parts(&o, s, e, kind) {
            emit(i, &mut arr, kind, &value, source);
        }
        Ok(i.make_array(arr))
    });
    install_format_getter(it, &proto);
}

/// Resolve both range endpoints (numbers or Temporal objects) to epoch-ms + kind. The two endpoints
/// must be the same kind; NaN or start > end throws.
fn range_dates(i: &mut Interp, o: &Gc, a: &Value, b: &Value) -> Result<(f64, f64, u8), Value> {
    if matches!(a, Value::Undefined) || matches!(b, Value::Undefined) {
        return Err(i.make_error("TypeError", "formatRange requires two dates"));
    }
    // ToDateTimeFormattable each endpoint IN ORDER: a Temporal value keeps its type, a non-Temporal
    // is ToNumber-coerced now (its valueOf side-effects must run before the kind-mismatch check).
    let ta = range_type_tag(i, a);
    let na = if ta == 0 {
        Some(ab(i.to_number(a))?)
    } else {
        None
    };
    let tb = range_type_tag(i, b);
    let nb = if tb == 0 {
        Some(ab(i.to_number(b))?)
    } else {
        None
    };
    // Two Temporal endpoints must share a calendar (RangeError otherwise).
    let cal_of = |i: &Interp, v: &Value| -> Option<String> {
        let ptr = Rc::as_ptr(v.as_obj()?) as usize;
        if i.temporal.contains_key(&ptr) {
            Some(
                i.temporal_cal
                    .get(&ptr)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "iso8601".to_string()),
            )
        } else {
            None
        }
    };
    if let (Some(ca), Some(cb)) = (cal_of(i, a), cal_of(i, b)) {
        if ca != cb {
            return Err(i.make_error(
                "RangeError",
                "formatRange endpoints have different calendars",
            ));
        }
    }
    // The two endpoints must be the same type (checked AFTER coercion, before TimeClip).
    if ta != tb {
        return Err(i.make_error("TypeError", "formatRange endpoints must be the same type"));
    }
    // TimeClip a coerced number; a Temporal reads its own fields (no re-coercion).
    let resolve = |i: &mut Interp, v: &Value, n: Option<f64>| -> Result<(f64, u8), Value> {
        match n {
            Some(x) => {
                if !x.is_finite() || x.abs() > 8.64e15 {
                    return Err(i.make_error("RangeError", "Invalid time value"));
                }
                Ok((x.trunc() + 0.0, 0))
            }
            None => dtf_ms_kind(i, o, v),
        }
    };
    let (s, _) = resolve(i, a, na)?;
    let (e, ks) = resolve(i, b, nb)?;
    Ok((s, e, ks))
}

/// Map a temporal era CODE to its CLDR era index for name lookup (single-era calendars are "0";
/// two-era calendars split by sign; japanese eras have no simple index so fall back to the code).
fn era_cldr_index(cal: &str, code: &str) -> &'static str {
    match code {
        "ce" | "ad" => "1",
        "bce" | "bc" => "0",
        "ah" => "0",
        "bh" => "1",
        "roc" | "minguo" => "1",
        "broc" => "0",
        "am" if cal == "coptic" || cal == "ethiopic" => "1",
        _ => "0",
    }
}

/// A type discriminant for formatRange: 0 = non-Temporal (number/Date), else the Temporal variant.
fn range_type_tag(i: &Interp, v: &Value) -> u8 {
    use crate::temporal::Temporal as T;
    if let Some(o) = v.as_obj() {
        if let Some(t) = i.temporal.get(&(Rc::as_ptr(o) as usize)) {
            return match t {
                T::Date(_) => 1,
                T::Time(_) => 2,
                T::DateTime(..) => 3,
                T::YearMonth(_) => 4,
                T::MonthDay(_) => 5,
                T::Instant(_) => 6,
                T::Zoned { .. } => 7,
                T::Duration(_) => 8,
            };
        }
    }
    0
}

fn install_format_getter(it: &mut Interp, proto: &Gc) {
    let g = it.make_native("get format", 0, |i, this, _| {
        let o = crate::intl::brand_slot_legacy(i, &this, "__dtf", "Intl.DateTimeFormat")?;
        if let Some(f) = o.borrow().props.get("__dtf_bound").map(|p| p.value()) {
            return Ok(f);
        }
        let f = i.make_native("", 1, |i, that, a| {
            Ok(Value::from_string(do_format(i, &that, &arg(a, 0))?))
        });
        let bound = crate::intl::numberformat::bind_this(i, Value::Obj(f), Value::Obj(o.clone()));
        set_builtin(&o, "__dtf_bound", bound.clone());
        Ok(bound)
    });
    proto.borrow_mut().props.insert(
        "format",
        crate::value::Property::accessor_prop(Some(Value::Obj(g)), None, false, true),
    );
}

/// A Unicode key type identifier: one or more "-"-joined 3..8 alnum subtags.
fn valid_type_id(s: &str) -> bool {
    !s.is_empty()
        && s.split('-')
            .all(|p| p.len() >= 3 && p.len() <= 8 && p.bytes().all(|b| b.is_ascii_alphanumeric()))
}

/// Return the run width of the first requested field in a UTS #35 date pattern. Quoted ASCII
/// letters are literals, and doubled apostrophes do not change quote state.
fn dtf_pattern_width(pattern: &str, fields: &str) -> Option<usize> {
    let mut chars = pattern.chars().peekable();
    let mut quoted = false;
    while let Some(character) = chars.next() {
        if character == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
            } else {
                quoted = !quoted;
            }
            continue;
        }
        if quoted || !fields.contains(character) {
            continue;
        }
        let mut width = 1;
        while chars.peek() == Some(&character) {
            chars.next();
            width += 1;
        }
        return Some(width);
    }
    None
}

/// Populate the internal component values used to compute a style formatter's fields. ECMA-402
/// styles are locale patterns, not a fixed English component preset, so widths must come from the
/// selected CLDR pattern (for example Japanese full dates use numeric `M`, not named `MMMM`).
fn install_style_components(obj: &Gc, pattern: &str) {
    let put_width = |slot: &str, fields: &str, value: fn(usize) -> &'static str| {
        if let Some(width) = dtf_pattern_width(pattern, fields) {
            set_builtin(obj, slot, Value::str(value(width)));
        }
    };
    put_width("__dtfx_weekday", "Eec", |width| match width {
        4 => "long",
        5 => "narrow",
        _ => "short",
    });
    put_width("__dtfx_era", "G", |width| match width {
        4 => "long",
        5 => "narrow",
        _ => "short",
    });
    put_width("__dtfx_year", "yYuUr", |width| {
        if width == 2 {
            "2-digit"
        } else {
            "numeric"
        }
    });
    put_width("__dtfx_month", "ML", |width| match width {
        2 => "2-digit",
        3 => "short",
        4 => "long",
        5 => "narrow",
        _ => "numeric",
    });
    put_width("__dtfx_day", "d", |width| {
        if width == 2 {
            "2-digit"
        } else {
            "numeric"
        }
    });
    put_width("__dtfx_hour", "hHKk", |width| {
        if width == 2 {
            "2-digit"
        } else {
            "numeric"
        }
    });
    put_width("__dtfx_minute", "m", |width| {
        if width == 2 {
            "2-digit"
        } else {
            "numeric"
        }
    });
    put_width("__dtfx_second", "s", |width| {
        if width == 2 {
            "2-digit"
        } else {
            "numeric"
        }
    });
    if let Some(width) = dtf_pattern_width(pattern, "zZvVOXx") {
        set_builtin(
            obj,
            "__dtfx_tzname",
            Value::str(if width >= 4 { "long" } else { "short" }),
        );
    }
}

/// Update the month slot when best-fit selection changes between a named and numeric CLDR pattern.
/// This avoids attaching a numeric locale suffix twice and makes `resolvedOptions()` describe the
/// selected format (Japanese `month: "short"`, for example, resolves to numeric here).
fn install_resolved_components(obj: &Gc, pattern: &str) {
    if obj.borrow().props.contains("__dtf_month") {
        if let Some(width) = dtf_pattern_width(pattern, "ML") {
            set_builtin(
                obj,
                "__dtf_month",
                Value::str(match width {
                    2 => "2-digit",
                    3 => "short",
                    4 => "long",
                    5 => "narrow",
                    _ => "numeric",
                }),
            );
        }
    }
}

fn construct(i: &mut Interp, t: Value, a: &[Value]) -> Result<Value, Value> {
    // Legacy service: callable without `new` (returns a fresh instance either way).
    let requested = canonicalize_locale_list(i, &arg(a, 0))?;
    let options = coerce_options(i, &arg(a, 1))?;
    read_locale_matcher(i, &options)?;

    // Spec option read order: calendar, numberingSystem, hour12, hourCycle, timeZone, then the
    // component options, then formatMatcher, dateStyle, timeStyle.
    let calendar = get_option(i, &options, "calendar", &[], None)?.map(|c| {
        // A deprecated calendar id canonicalizes (e.g. ethiopic-amete-alem -> ethioaa).
        let lc = c.to_lowercase();
        crate::intl::tags::canonical_ca(&lc).unwrap_or(lc)
    });
    if let Some(c) = &calendar {
        if !valid_type_id(c) {
            return Err(i.make_error("RangeError", format!("invalid calendar: {c}")));
        }
    }
    // "islamic"/"islamic-rgsa" are not directly available; fall back to a concrete Islamic calendar.
    // Deprecated islamic ids fall back to islamic-civil; any calendar not in AvailableCalendars
    // (a not-yet-supported one like "bangla") falls back to gregory.
    const AVAILABLE_CALENDARS: [&str; 17] = [
        "buddhist",
        "chinese",
        "coptic",
        "dangi",
        "ethioaa",
        "ethiopic",
        "gregory",
        "hebrew",
        "indian",
        "islamic-civil",
        "islamic-tbla",
        "islamic-umalqura",
        "iso8601",
        "japanese",
        "persian",
        "roc",
        "islamicc",
    ];
    // Resolve a calendar VALUE: islamic/islamic-rgsa → islamic-civil, an available id is kept, any
    // other (unsupported) value is None so it is ignored (the -u-ca extension, then gregory, apply).
    let resolve_cal = |c: &str| -> Option<String> {
        if c == "islamic" || c == "islamic-rgsa" {
            Some("islamic-civil".to_string())
        } else if AVAILABLE_CALENDARS.contains(&c) {
            Some(c.to_string())
        } else {
            None
        }
    };
    let calendar = calendar.and_then(|c| resolve_cal(&c));
    let numbering = get_option(i, &options, "numberingSystem", &[], None)?;
    if let Some(ns) = &numbering {
        if !valid_type_id(ns) {
            return Err(i.make_error("RangeError", format!("invalid numberingSystem: {ns}")));
        }
    }
    let hour12 = {
        let v = ab(i.get_member(&options, "hour12"))?;
        if matches!(v, Value::Undefined) {
            None
        } else {
            Some(i.to_boolean(&v))
        }
    };
    let hour_cycle = get_option(
        i,
        &options,
        "hourCycle",
        &["h11", "h12", "h23", "h24"],
        None,
    )?;
    let time_zone = {
        let v = ab(i.get_member(&options, "timeZone"))?;
        if matches!(v, Value::Undefined) {
            "UTC".to_string()
        } else {
            let raw = ab(i.to_string(&v))?.to_string();
            match canonicalize_time_zone(&raw) {
                Some(tz) => tz,
                None => return Err(i.make_error("RangeError", format!("invalid time zone: {raw}"))),
            }
        }
    };

    let weekday = get_option(i, &options, "weekday", &["narrow", "short", "long"], None)?;
    let era = get_option(i, &options, "era", &["narrow", "short", "long"], None)?;
    let year = get_option(i, &options, "year", &["2-digit", "numeric"], None)?;
    let month = get_option(
        i,
        &options,
        "month",
        &["2-digit", "numeric", "narrow", "short", "long"],
        None,
    )?;
    let day = get_option(i, &options, "day", &["2-digit", "numeric"], None)?;
    let day_period = get_option(i, &options, "dayPeriod", &["narrow", "short", "long"], None)?;
    let hour = get_option(i, &options, "hour", &["2-digit", "numeric"], None)?;
    let minute = get_option(i, &options, "minute", &["2-digit", "numeric"], None)?;
    let second = get_option(i, &options, "second", &["2-digit", "numeric"], None)?;
    let frac_sec = {
        let v = ab(i.get_member(&options, "fractionalSecondDigits"))?;
        if matches!(v, Value::Undefined) {
            None
        } else {
            // GetNumberOption(1, 3): reject NaN/out-of-range, otherwise floor into [1,3].
            let n = ab(i.to_number(&v))?;
            if n.is_nan() || !(1.0..=3.0).contains(&n) {
                return Err(i.make_error("RangeError", "fractionalSecondDigits out of range"));
            }
            Some(n.floor() as u32)
        }
    };
    let tz_name = get_option(
        i,
        &options,
        "timeZoneName",
        &[
            "short",
            "long",
            "shortOffset",
            "longOffset",
            "shortGeneric",
            "longGeneric",
        ],
        None,
    )?;
    let _format_matcher = get_option(
        i,
        &options,
        "formatMatcher",
        &["basic", "best fit"],
        Some("best fit"),
    )?;
    let date_style = get_option(
        i,
        &options,
        "dateStyle",
        &["full", "long", "medium", "short"],
        None,
    )?;
    let time_style = get_option(
        i,
        &options,
        "timeStyle",
        &["full", "long", "medium", "short"],
        None,
    )?;

    // A dateStyle/timeStyle may not be combined with explicit component options.
    let has_components = weekday.is_some()
        || era.is_some()
        || year.is_some()
        || month.is_some()
        || day.is_some()
        || day_period.is_some()
        || hour.is_some()
        || minute.is_some()
        || second.is_some()
        || frac_sec.is_some()
        || tz_name.is_some();
    if (date_style.is_some() || time_style.is_some()) && has_components {
        return Err(i.make_error(
            "TypeError",
            "dateStyle/timeStyle cannot be combined with explicit date-time components",
        ));
    }

    // era and timeZoneName do not count toward ToDateTimeOptions' needDefaults check (only the date
    // and time components do), so a formatter with only `era` still gets the default date components.
    let has_explicit = weekday.is_some()
        || year.is_some()
        || month.is_some()
        || day.is_some()
        || hour.is_some()
        || minute.is_some()
        || second.is_some()
        || day_period.is_some()
        || frac_sec.is_some();
    let resolved = resolve_locale(i, &requested, &["ca", "nu", "hc"]);
    // The `-u-hc-` locale-extension hour cycle (the `hourCycle` option, read later, overrides it).
    let hc_ext = resolved
        .keywords
        .iter()
        .find(|(k, _)| k == "hc")
        .map(|(_, v)| v.clone());
    // ResolveLocale for the `nu` key gives both the numbering system and the resolved locale string
    // (reflecting a surviving `-u-nu-` extension).
    let (resolved_locale, nu_final) =
        super::service::resolve_locale_nu(&requested, numbering.as_deref());

    let obj = i.new_object();
    if let Some(proto) = instance_proto(i, "Intl.DateTimeFormat")? {
        obj.borrow_mut().proto = Some(proto);
    }
    set_builtin(&obj, "__dtf", Value::Bool(true));
    let locale_lang = resolved_locale.split('-').next().unwrap_or("").to_string();
    // ResolveLocale for the `ca` key: the locale's -u-ca- extension value (if supported) is used and
    // reflected in the resolved locale, unless a supported calendar option overrides it with a
    // different value (then the reflection is dropped).
    let ca_ext = resolved
        .keywords
        .iter()
        .find(|(k, _)| k == "ca")
        .and_then(|(_, v)| {
            let lc = v.to_lowercase();
            resolve_cal(&crate::intl::tags::canonical_ca(&lc).unwrap_or(lc))
        });
    let mut ca_addition = ca_ext.clone();
    if let Some(opt) = &calendar {
        if ca_ext.as_ref() != Some(opt) {
            ca_addition = None;
        }
    }
    let eff_cal = calendar
        .clone()
        .or(ca_ext)
        .unwrap_or_else(|| "gregory".to_string());
    // ResolveLocale for the `hc` key: reflect a valid -u-hc- unless the hourCycle/hour12 options
    // override it with a different value.
    let hc_valid = hc_ext
        .clone()
        .filter(|v| matches!(v.as_str(), "h11" | "h12" | "h23" | "h24"));
    let hc_addition = match (&hc_valid, &hour_cycle, hour12) {
        (Some(e), Some(opt), _) if opt != e => None,
        (Some(_), _, Some(_)) => None,
        (Some(e), _, _) => Some(e.clone()),
        _ => None,
    };
    // Rebuild the -u- extension from the surviving ca/hc/nu additions (keys sorted alphabetically).
    let base = resolved_locale
        .split("-u-")
        .next()
        .unwrap_or(&resolved_locale)
        .to_string();
    let nu_add = resolved_locale
        .strip_prefix(&format!("{base}-u-nu-"))
        .map(|s| s.to_string());
    let mut ext = String::new();
    if let Some(ca) = &ca_addition {
        ext.push_str(&format!("-ca-{ca}"));
    }
    if let Some(hc) = &hc_addition {
        ext.push_str(&format!("-hc-{hc}"));
    }
    if let Some(nu) = &nu_add {
        ext.push_str(&format!("-nu-{nu}"));
    }
    let resolved_locale = if ext.is_empty() {
        base
    } else {
        format!("{base}-u{ext}")
    };
    set_builtin(&obj, "__dtf_locale", Value::from_string(resolved_locale));
    set_builtin(&obj, "__dtf_ca", Value::from_string(eff_cal));
    set_builtin(&obj, "__dtf_nu", Value::from_string(nu_final));
    set_builtin(&obj, "__dtf_tz", Value::from_string(time_zone));
    let put = |obj: &Gc, k: &str, v: &Option<String>| {
        if let Some(v) = v {
            set_builtin(obj, k, Value::from_string(v.clone()));
        }
    };
    put(&obj, "__dtf_weekday", &weekday);
    put(&obj, "__dtf_era", &era);
    put(&obj, "__dtf_year", &year);
    put(&obj, "__dtf_month", &month);
    put(&obj, "__dtf_day", &day);
    put(&obj, "__dtf_dayperiod", &day_period);
    put(&obj, "__dtf_hour", &hour);
    put(&obj, "__dtf_minute", &minute);
    put(&obj, "__dtf_second", &second);
    if let Some(f) = frac_sec {
        set_builtin(&obj, "__dtf_fracsec", Value::Num(f as f64));
    }
    put(&obj, "__dtf_tzname", &tz_name);
    put(&obj, "__dtf_datestyle", &date_style);
    put(&obj, "__dtf_timestyle", &time_style);
    // ECMA-402 DateTime Style Formats selects locale-specific CLDR patterns. Derive the working
    // component set from those actual patterns rather than applying an English-shaped preset.
    let cldr_locale = dtf_cldr_locale(&obj);
    if let Some(ds) = &date_style {
        install_style_components(
            &obj,
            crate::cldr_datetime_patterns::date_style(cldr_locale, ds),
        );
    }
    if let Some(ts) = &time_style {
        install_style_components(
            &obj,
            crate::cldr_datetime_patterns::time_style(cldr_locale, ts),
        );
    }
    // The hour cycle is always resolved (a Temporal PlainTime/Instant/PlainDateTime toLocaleString
    // shows a defaulted hour that must honor hour12/hourCycle), but only *reported* in
    // resolvedOptions when the formatter's own pattern shows an hour (explicit hour or a timeStyle).
    let shows_hour = hour.is_some() || time_style.is_some();
    {
        // hour12 overrides hourCycle: true → the locale's 12-hour cycle (h11 for ja, else h12);
        // false → h23. Absent both, fall back to the requested hourCycle or h23.
        let lang = locale_lang.as_str();
        let locale_hour_cycle = crate::cldr_datetime_patterns::default_hour_cycle(cldr_locale);
        let hc = if let Some(h12) = hour12 {
            if h12 {
                if lang == "ja" { "h11" } else { "h12" }.to_string()
            } else {
                "h23".to_string()
            }
        } else {
            // The `hourCycle` option wins, then the locale's `-u-hc-` extension, then the locale
            // default (en is 12-hour; most others are h23).
            hour_cycle
                .clone()
                .or_else(|| hc_ext.clone())
                .unwrap_or_else(|| locale_hour_cycle.to_string())
        };
        let h12 = matches!(hc.as_str(), "h11" | "h12");
        set_builtin(&obj, "__dtf_hourcycle", Value::from_string(hc));
        set_builtin(&obj, "__dtf_hour12", Value::Bool(h12));
        set_builtin(&obj, "__dtf_hourshown", Value::Bool(shows_hour));
    }
    // Default components when nothing was requested: year/month/day numeric. Flagged so a Temporal
    // receiver's compatibility check ignores them (only *explicit* options can conflict).
    if !has_explicit && date_style.is_none() && time_style.is_none() {
        set_builtin(&obj, "__dtf_year", Value::str("numeric"));
        set_builtin(&obj, "__dtf_month", Value::str("numeric"));
        set_builtin(&obj, "__dtf_day", Value::str("numeric"));
        set_builtin(&obj, "__dtf_defaults", Value::Bool(true));
    }
    if date_style.is_none() && time_style.is_none() {
        let mut requested = Vec::new();
        for (slot, kind) in [
            ("__dtf_weekday", "weekday"),
            ("__dtf_era", "era"),
            ("__dtf_year", "year"),
            ("__dtf_month", "month"),
            ("__dtf_day", "day"),
            ("__dtf_dayperiod", "dayPeriod"),
            ("__dtf_hour", "hour"),
            ("__dtf_minute", "minute"),
            ("__dtf_second", "second"),
            ("__dtf_tzname", "timeZoneName"),
        ] {
            if obj.borrow().props.contains(slot) {
                requested.push((kind, String::new()));
            }
        }
        if let Some(pattern) = resolved_dtf_pattern(&obj, &requested) {
            install_resolved_components(&obj, &pattern);
        }
    }
    // Legacy "ChainDateTimeFormat": called without `new` on an object inheriting from
    // %DateTimeFormat.prototype%, the fresh instance is stored ON that object under the realm's
    // [[FallbackSymbol]] and the object itself is returned.
    if !i.constructing {
        if let Some(chained) = crate::intl::legacy_chain(i, &t, "Intl.DateTimeFormat", &obj) {
            return Ok(chained);
        }
    }
    Ok(Value::Obj(obj))
}

/// Break epoch-ms into UTC (Y, M, D, h, m, s, weekday).
fn ymd(ms: f64) -> (i64, u32, u32, u32, u32, u32, u32) {
    let ms_i = ms.floor() as i64;
    let mut days = ms_i.div_euclid(86_400_000);
    let mut rem = ms_i.rem_euclid(86_400_000);
    let h = (rem / 3_600_000) as u32;
    rem %= 3_600_000;
    let mi = (rem / 60_000) as u32;
    rem %= 60_000;
    let sec = (rem / 1000) as u32;
    let weekday = ((days % 7 + 4) % 7 + 7) as u32 % 7; // 0=Sun; 1970-01-01 was Thursday(4)
                                                       // civil from days
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let year = if m <= 2 { y + 1 } else { y };
    let _ = &mut days;
    (year, m, d, h, mi, sec, weekday)
}

const WD_LONG: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];
const WD_SHORT: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

/// Resolve the format argument to epoch-milliseconds. A Temporal receiver uses its ISO fields and
/// must be compatible with the formatter's requested components (else TypeError/RangeError).
/// Resolve to (epoch-ms, Temporal-kind); kind gates the components a Temporal receiver may show
/// (1=Date, 2=Time, 3=DateTime, 4=YearMonth, 5=MonthDay, 0=any/number/instant/zoned).
fn dtf_ms_kind(i: &mut Interp, o: &Gc, date: &Value) -> Result<(f64, u8), Value> {
    use crate::temporal::Temporal as T;
    if matches!(date, Value::Undefined) {
        return Ok((0.0, 0));
    }
    if let Some(dobj) = date.as_obj() {
        let ptr = Rc::as_ptr(dobj) as usize;
        if let Some(t) = i.temporal.get(&ptr).cloned() {
            // format()/formatRange() do not accept Temporal.ZonedDateTime directly (the caller can't
            // know which time zone to use); Temporal.ZonedDateTime.prototype.toLocaleString handles it.
            if matches!(t, T::Zoned { .. }) {
                return Err(i.make_error(
                    "TypeError",
                    "Intl.DateTimeFormat does not support Temporal.ZonedDateTime; use zonedDateTime.toLocaleString()",
                ));
            }
            temporal_compat_check(i, o, &t)?;
            // Calendar mismatch: a non-ISO Temporal calendar must equal the formatter's calendar.
            let tcal = i
                .temporal_cal
                .get(&ptr)
                .map(|c| c.to_string())
                .unwrap_or_else(|| "iso8601".to_string());
            let dcal = match o.borrow().props.get("__dtf_ca").map(|p| p.value()) {
                Some(Value::Str(s)) => s.to_string(),
                _ => "iso8601".to_string(),
            };
            let kind = match t {
                T::Date(_) => 1,
                T::Time(_) => 2,
                T::DateTime(..) => 3,
                T::YearMonth(_) => 4,
                T::MonthDay(_) => 5,
                T::Instant(_) => 6, // absolute (tz-shifted like kind 0) but defaults to date+time
                _ => 0,
            };
            // YearMonth/MonthDay require an EXACT calendar match (no ISO exception, since a bare
            // month-day/year-month is calendar-specific); other types accept an ISO instance.
            let exact = matches!(kind, 4 | 5);
            if tcal != dcal && (exact || tcal != "iso8601") {
                return Err(
                    i.make_error("RangeError", format!("calendar mismatch: {tcal} vs {dcal}"))
                );
            }
            return Ok((temporal_to_ms(&t), kind));
        }
    }
    let n = ab(i.to_number(date))?;
    // TimeClip: the representable range is ±8.64e15 ms; anything outside (or non-finite) is invalid.
    if !n.is_finite() || n.abs() > 8.64e15 {
        return Err(i.make_error("RangeError", "Invalid time value"));
    }
    // TimeClip truncates toward zero (so -0.9 -> +0, not floor's -1).
    Ok((n.trunc() + 0.0, 0))
}

/// A Temporal receiver must overlap the formatter's requested fields, else TypeError. The requested
/// set includes both explicit component options and any dateStyle/timeStyle expansion, so e.g. a
/// lone `timeStyle` (time fields only) has no overlap with a `PlainDate` and throws, while a
/// `dateStyle`+`timeStyle` formatter overlaps every receiver.
fn temporal_compat_check(
    i: &mut Interp,
    o: &Gc,
    t: &crate::temporal::Temporal,
) -> Result<(), Value> {
    use crate::temporal::Temporal as T;
    // Field set of the receiver (era is auxiliary to year and never stands alone, so it is omitted).
    let recv: &[&str] = match t {
        T::Date(_) => &["weekday", "year", "month", "day"],
        T::YearMonth(_) => &["year", "month"],
        T::MonthDay(_) => &["month", "day"],
        T::Time(_) => &["hour", "minute", "second", "fracsec", "dayperiod"],
        _ => return Ok(()), // DateTime / Instant / Zoned overlap everything
    };
    // A fully-defaulted formatter adapts its fields to the receiver, so it always overlaps.
    if o.borrow().props.contains("__dtf_defaults") {
        return Ok(());
    }
    // A field is requested if set explicitly (`__dtf_`) or via a dateStyle/timeStyle (`__dtfx_`).
    let present = |field: &str| {
        let b = o.borrow();
        b.props.contains(&format!("__dtf_{field}")) || b.props.contains(&format!("__dtfx_{field}"))
    };
    const ALL: &[&str] = &[
        "weekday",
        "year",
        "month",
        "day",
        "hour",
        "minute",
        "second",
        "fracsec",
        "dayperiod",
    ];
    let requested: Vec<&str> = ALL.iter().copied().filter(|f| present(f)).collect();
    if !requested.is_empty() && !requested.iter().any(|f| recv.contains(f)) {
        return Err(i.make_error(
            "TypeError",
            "no overlap between the formatter and the Temporal value",
        ));
    }
    Ok(())
}

fn do_format(i: &mut Interp, this: &Value, date: &Value) -> Result<String, Value> {
    let o = brand_slot(i, this, "__dtf")?;
    let (ms, kind) = dtf_ms_kind(i, &o, date)?;
    Ok(do_format_ms(&o, ms, kind))
}

/// The formatter's numbering system id (`latn` unless set).
fn dtf_nu(o: &Gc) -> String {
    match o.borrow().props.get("__dtf_nu").map(|p| p.value()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "latn".to_string(),
    }
}

fn dtf_cldr_locale(o: &Gc) -> &'static str {
    let locale = match o
        .borrow()
        .props
        .get("__dtf_locale")
        .map(|property| property.value())
    {
        Some(Value::Str(locale)) => locale.to_string(),
        _ => "en".to_string(),
    };
    let mut parts = locale.split('-');
    let language = parts.next().unwrap_or("en");
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
    crate::cldr_datetime_patterns::locale(language, script, region)
}
/// Format an already-resolved epoch-ms + Temporal kind to the joined string.
fn do_format_ms(o: &Gc, ms: f64, kind: u8) -> String {
    let s: String = build_parts(o, ms, kind)
        .into_iter()
        .map(|(_, v)| v)
        .collect();
    crate::intl::numberformat::xlate_digits(&s, &dtf_nu(o))
}

/// Build localized typed `(type, value)` parts for the given epoch milliseconds. `kind` gates which
/// components a Temporal receiver may show (see [`dtf_ms_kind`]).
fn dtf_slot(o: &Gc, slot: &str) -> Option<String> {
    match o.borrow().props.get(slot).map(|property| property.value()) {
        Some(Value::Str(value)) => Some(value.to_string()),
        _ => None,
    }
}

fn skeleton_width(value: &str, numeric: char, named: char) -> String {
    match value {
        "2-digit" => numeric.to_string().repeat(2),
        "short" => named.to_string().repeat(3),
        "long" => named.to_string().repeat(4),
        "narrow" => named.to_string().repeat(5),
        _ => numeric.to_string(),
    }
}

fn skeleton_signature(skeleton: &str) -> String {
    let mut output = String::new();
    for character in skeleton.chars() {
        let field = match character {
            'L' => 'M',
            'e' | 'c' => 'E',
            'K' => 'h',
            'k' => 'H',
            other => other,
        };
        if !output.contains(field) {
            output.push(field);
        }
    }
    output
}

fn skeleton_distance(left: &str, right: &str) -> usize {
    let widths = |skeleton: &str| {
        let mut result = [0_u8; 128];
        let mut chars = skeleton.chars().peekable();
        while let Some(character) = chars.next() {
            if !character.is_ascii() {
                continue;
            }
            let mut width = 1_u8;
            while chars.peek() == Some(&character) {
                chars.next();
                width = width.saturating_add(1);
            }
            result[character as usize] = width;
        }
        result
    };
    let left = widths(left);
    let right = widths(right);
    left.iter()
        .zip(right.iter())
        .map(|(a, b)| a.abs_diff(*b) as usize)
        .sum()
}

fn closest_available_pattern(locale: &str, skeleton: &str) -> Option<&'static str> {
    if let Some(pattern) = crate::cldr_datetime_patterns::available_exact(locale, skeleton) {
        return Some(pattern);
    }
    let signature = skeleton_signature(skeleton);
    crate::cldr_datetime_patterns::available(locale)
        .iter()
        .filter(|row| skeleton_signature(row.0) == signature)
        .min_by_key(|row| skeleton_distance(row.0, skeleton))
        .map(|row| row.1)
}

/// Apply the formatter's resolved hour cycle to a CLDR time-style pattern. UTS #35 pattern
/// matching replaces the hour field type and removes the day-period field for a 24-hour request.
fn time_style_pattern(o: &Gc, locale: &str, style: &str) -> String {
    let pattern = crate::cldr_datetime_patterns::time_style(locale, style);
    if !matches!(
        dtf_slot(o, "__dtf_hourcycle").as_deref(),
        Some("h23" | "h24")
    ) {
        return pattern.to_string();
    }

    let mut output = String::new();
    let mut chars = pattern.chars().peekable();
    let mut quoted = false;
    while let Some(character) = chars.next() {
        if character == '\'' {
            output.push(character);
            if chars.peek() == Some(&'\'') {
                output.push(chars.next().unwrap());
            } else {
                quoted = !quoted;
            }
            continue;
        }
        if !quoted && matches!(character, 'a' | 'b' | 'B') {
            while chars.peek() == Some(&character) {
                chars.next();
            }
            while output.chars().last().is_some_and(char::is_whitespace) {
                output.pop();
            }
            if output.is_empty() {
                while chars.peek().is_some_and(|next| next.is_whitespace()) {
                    chars.next();
                }
            }
            continue;
        }
        if !quoted && matches!(character, 'h' | 'H' | 'K' | 'k') {
            output.push('H');
        } else {
            output.push(character);
        }
    }
    output
}

/// Select a CLDR style or availableFormats pattern for the formatter's resolved fields. UTS #35
/// handles mixed date/time requests by matching each half and joining them with dateTimeFormats.
fn resolved_dtf_pattern(o: &Gc, parts: &[(&'static str, String)]) -> Option<String> {
    let locale = dtf_cldr_locale(o);
    let date_style = dtf_slot(o, "__dtf_datestyle");
    let time_style = dtf_slot(o, "__dtf_timestyle");
    if date_style.is_some() || time_style.is_some() {
        let has_date = parts.iter().any(|part| {
            matches!(
                part.0,
                "weekday" | "era" | "year" | "relatedYear" | "yearName" | "month" | "day"
            )
        });
        let has_time = parts.iter().any(|part| {
            matches!(
                part.0,
                "dayPeriod" | "hour" | "minute" | "second" | "fractionalSecond" | "timeZoneName"
            )
        });
        let date = date_style
            .as_deref()
            .filter(|_| has_date)
            .map(|style| crate::cldr_datetime_patterns::date_style(locale, style).to_string());
        let time = time_style
            .as_deref()
            .filter(|_| has_time)
            .map(|style| time_style_pattern(o, locale, style));
        return match (date, time) {
            (Some(date), Some(time)) => {
                let connector = crate::cldr_datetime_patterns::connector(
                    locale,
                    date_style.as_deref().unwrap(),
                );
                Some(connector.replace("{1}", &date).replace("{0}", &time))
            }
            (Some(date), None) => Some(date),
            (None, Some(time)) => Some(time),
            _ => None,
        };
    }

    let has = |kind: &str| parts.iter().any(|part| part.0 == kind);
    // The bespoke related-year/year-name cluster for Chinese/Dangi cannot be represented by one
    // ordinary `y` token; retain its calendar-specific assembly.
    if has("relatedYear") || has("yearName") {
        return None;
    }
    let mut date = String::new();
    if has("era") {
        date.push_str(&skeleton_width(
            &dtf_slot(o, "__dtf_era").unwrap_or_else(|| "short".to_string()),
            'G',
            'G',
        ));
    }
    if has("year") {
        date.push_str(&skeleton_width(
            &dtf_slot(o, "__dtf_year").unwrap_or_else(|| "numeric".to_string()),
            'y',
            'y',
        ));
    }
    if has("month") {
        date.push_str(&skeleton_width(
            &dtf_slot(o, "__dtf_month").unwrap_or_else(|| "numeric".to_string()),
            'M',
            'M',
        ));
    }
    if has("weekday") {
        date.push_str(&skeleton_width(
            &dtf_slot(o, "__dtf_weekday").unwrap_or_else(|| "short".to_string()),
            'E',
            'E',
        ));
    }
    if has("day") {
        date.push_str(&skeleton_width(
            &dtf_slot(o, "__dtf_day").unwrap_or_else(|| "numeric".to_string()),
            'd',
            'd',
        ));
    }

    let mut time = String::new();
    if dtf_slot(o, "__dtf_dayperiod").is_some() {
        time.push('B');
    }
    if has("hour") {
        let hour = if matches!(
            dtf_slot(o, "__dtf_hourcycle").as_deref(),
            Some("h11" | "h12")
        ) {
            'h'
        } else {
            'H'
        };
        time.push(hour);
    }
    if has("minute") {
        time.push('m');
    }
    if has("second") {
        time.push('s');
    }
    if has("timeZoneName") {
        time.push('v');
    }

    let find = |skeleton: &str| closest_available_pattern(locale, skeleton).map(str::to_string);
    match (date.is_empty(), time.is_empty()) {
        (false, false) => {
            let date_pattern = find(&date)?;
            let time_pattern = find(&time)?;
            Some(
                crate::cldr_datetime_patterns::connector(locale, "medium")
                    .replace("{1}", &date_pattern)
                    .replace("{0}", &time_pattern),
            )
        }
        (false, true) => find(&date),
        (true, false) => find(&time),
        (true, true) => None,
    }
}

fn push_dtf_literal(parts: &mut Vec<(&'static str, String)>, value: &str) {
    if value.is_empty() {
        return;
    }
    // DateTimeFormat locale data is implementation-defined. Use the web-platform's ordinary-space
    // rendering for CLDR's day-period NNBSP so `format()` remains the concatenation of
    // `formatToParts()` and matches Test262's fixed numbering-system expectations.
    let value = value.replace('\u{202f}', " ");
    if let Some(("literal", previous)) = parts.last_mut() {
        previous.push_str(&value);
    } else {
        parts.push(("literal", value));
    }
}

/// Apply a UTS #35 date pattern to already-computed field values, preserving ECMA-402 part types.
fn apply_dtf_pattern(
    o: &Gc,
    pattern: &str,
    values: &[(&'static str, String)],
) -> Vec<(&'static str, String)> {
    let field_type = |symbol: char| match symbol {
        'G' => Some("era"),
        'y' | 'Y' | 'u' | 'U' | 'r' => Some("year"),
        'M' | 'L' => Some("month"),
        'd' => Some("day"),
        'E' | 'e' | 'c' => Some("weekday"),
        'a' | 'b' | 'B' => Some("dayPeriod"),
        'h' | 'H' | 'K' | 'k' => Some("hour"),
        'm' => Some("minute"),
        's' => Some("second"),
        'S' => Some("fractionalSecond"),
        'z' | 'Z' | 'O' | 'v' | 'V' => Some("timeZoneName"),
        _ => None,
    };
    let find = |kind: &str| {
        values
            .iter()
            .find(|value| value.0 == kind)
            .map(|value| &value.1)
    };
    let field_value = |kind: &str, symbol: char, width: usize, value: &str| -> String {
        // A selected interval pattern can refine a numeric field's width relative to the base
        // format (CLDR Japanese yMd ranges use MM/dd even though the ordinary yMd pattern uses
        // M/d). Apply those pattern widths before numbering-system digit substitution.
        let numeric = value.parse::<i64>().ok();
        match (kind, symbol, width, numeric) {
            ("year", _, 2, Some(number)) => format!("{:02}", number.rem_euclid(100)),
            (
                "month" | "day" | "hour" | "minute" | "second",
                'M' | 'L' | 'd' | 'h' | 'H' | 'K' | 'k' | 'm' | 's',
                2,
                Some(number),
            ) => format!("{number:02}"),
            ("fractionalSecond", 'S', width, _) => {
                let mut result: String = value.chars().take(width).collect();
                result.extend(std::iter::repeat_n('0', width.saturating_sub(result.len())));
                result
            }
            _ => value.to_string(),
        }
    };
    let mut output = Vec::new();
    let mut chars = pattern.chars().peekable();
    let mut quoted = false;
    let mut fraction_emitted = false;
    while let Some(character) = chars.next() {
        if character == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
                push_dtf_literal(&mut output, "'");
            } else {
                quoted = !quoted;
            }
            continue;
        }
        if !quoted && character.is_ascii_alphabetic() {
            let mut width = 1;
            while chars.peek() == Some(&character) {
                chars.next();
                width += 1;
            }
            if let Some(kind) = field_type(character) {
                if let Some(value) = find(kind) {
                    output.push((kind, field_value(kind, character, width, value)));
                    if kind == "fractionalSecond" {
                        fraction_emitted = true;
                    }
                    if kind == "second" && !pattern.contains('S') {
                        if let Some(fraction) = find("fractionalSecond") {
                            let symbols =
                                crate::cldr_numbers::symbols(dtf_cldr_locale(o), &dtf_nu(o));
                            push_dtf_literal(&mut output, symbols.decimal);
                            output.push(("fractionalSecond", fraction.clone()));
                            fraction_emitted = true;
                        }
                    }
                }
            }
        } else {
            let mut literal = character.to_string();
            while let Some(next) = chars.peek().copied() {
                if next == '\'' || (!quoted && next.is_ascii_alphabetic()) {
                    break;
                }
                literal.push(next);
                chars.next();
            }
            push_dtf_literal(&mut output, &literal);
        }
    }
    if !fraction_emitted && find("fractionalSecond").is_some() && find("second").is_none() {
        output.push((
            "fractionalSecond",
            find("fractionalSecond").unwrap().clone(),
        ));
    }
    output
}

fn interval_skeleton(pattern: &str) -> String {
    let mut widths: std::collections::BTreeMap<char, usize> = Default::default();
    let mut chars = pattern.chars().peekable();
    let mut quoted = false;
    while let Some(character) = chars.next() {
        if character == '\'' {
            if chars.peek() == Some(&'\'') {
                chars.next();
            } else {
                quoted = !quoted;
            }
            continue;
        }
        if quoted || !character.is_ascii_alphabetic() {
            continue;
        }
        let mut count = 1;
        while chars.peek() == Some(&character) {
            chars.next();
            count += 1;
        }
        let field = match character {
            'G' => 'G',
            'y' | 'Y' | 'u' | 'U' | 'r' => 'y',
            'M' | 'L' => 'M',
            'd' => 'd',
            'E' | 'e' | 'c' => 'E',
            'B' | 'b' => 'B',
            'h' | 'K' => 'h',
            'H' | 'k' => 'H',
            'm' => 'm',
            's' | 'S' => 's',
            'z' | 'Z' | 'O' | 'v' | 'V' => 'v',
            _ => continue,
        };
        let width = match field {
            // CLDR interval data normally supplies one numeric and one named-month skeleton; the
            // closest-match algorithm then adjusts the selected pattern back to the requested
            // width. Field values are already width-adjusted here.
            'M' if count >= 3 => 3,
            'M' => 1,
            'E' | 'G' | 'y' | 'd' | 'B' | 'h' | 'H' | 'm' | 's' | 'v' => 1,
            _ => count,
        };
        widths.entry(field).or_insert(width);
    }
    let mut result = String::new();
    for field in ['G', 'y', 'M', 'E', 'd', 'B', 'h', 'H', 'm', 's', 'v'] {
        if let Some(width) = widths.get(&field) {
            result.push_str(&field.to_string().repeat(*width));
        }
    }
    result
}

fn greatest_range_difference(
    o: &Gc,
    skeleton: &str,
    start: &[(&'static str, String)],
    end: &[(&'static str, String)],
) -> Option<&'static str> {
    let changed = |kind: &str| {
        let a = start.iter().find(|part| part.0 == kind).map(|part| &part.1);
        let b = end.iter().find(|part| part.0 == kind).map(|part| &part.1);
        a != b
    };
    if changed("era") {
        Some("G")
    } else if changed("year") || changed("relatedYear") || changed("yearName") {
        Some("y")
    } else if changed("month") {
        Some("M")
    } else if changed("day") || changed("weekday") {
        Some("d")
    } else if changed("dayPeriod") {
        Some(if skeleton.contains('B') { "B" } else { "a" })
    } else if changed("hour") {
        Some(
            if matches!(
                dtf_slot(o, "__dtf_hourcycle").as_deref(),
                Some("h11" | "h12")
            ) {
                "h"
            } else {
                "H"
            },
        )
    } else if changed("minute") {
        Some("m")
    } else if changed("second") || changed("fractionalSecond") {
        Some("s")
    } else {
        None
    }
}

fn split_interval_pattern(pattern: &str) -> Option<(&str, &str)> {
    let mut seen = Vec::new();
    let mut chars = pattern.char_indices().peekable();
    let mut quoted = false;
    while let Some((index, character)) = chars.next() {
        if character == '\'' {
            if chars.peek().is_some_and(|(_, next)| *next == '\'') {
                chars.next();
            } else {
                quoted = !quoted;
            }
            continue;
        }
        if quoted || !character.is_ascii_alphabetic() {
            continue;
        }
        while chars.peek().is_some_and(|(_, next)| *next == character) {
            chars.next();
        }
        let field = match character {
            'M' | 'L' => 'M',
            'E' | 'e' | 'c' => 'E',
            'h' | 'H' | 'K' | 'k' => 'h',
            other => other,
        };
        if seen.contains(&field) {
            return Some((&pattern[..index], &pattern[index..]));
        }
        seen.push(field);
    }
    None
}

fn fallback_range_parts(
    o: &Gc,
    start: Vec<(&'static str, String)>,
    end: Vec<(&'static str, String)>,
) -> Vec<(&'static str, String, &'static str)> {
    let fallback = crate::cldr_datetime_patterns::range_fallback(dtf_cldr_locale(o));
    let mut output = Vec::new();
    let is_time = |kind: &str| {
        matches!(
            kind,
            "dayPeriod" | "hour" | "minute" | "second" | "fractionalSecond" | "timeZoneName"
        )
    };
    let start_time = start.iter().position(|part| is_time(part.0));
    let end_time = end.iter().position(|part| is_time(part.0));
    let share_date = match (start_time, end_time) {
        (Some(a), Some(b)) => {
            start[..a] == end[..b]
                && start[..a].iter().any(|part| {
                    matches!(
                        part.0,
                        "weekday" | "era" | "year" | "relatedYear" | "yearName" | "month" | "day"
                    )
                })
        }
        _ => false,
    };
    let (start, end) = if share_date {
        let prefix = start_time.unwrap();
        output.extend(
            start[..prefix]
                .iter()
                .map(|(kind, value)| (*kind, value.clone(), "shared")),
        );
        (&start[prefix..], &end[end_time.unwrap()..])
    } else {
        (&start[..], &end[..])
    };
    let mut rest = fallback;
    while let Some(index) = rest.find(['{']) {
        if index > 0 {
            output.push(("literal", rest[..index].to_string(), "shared"));
        }
        let marker = &rest[index..];
        if let Some(tail) = marker.strip_prefix("{0}") {
            output.extend(
                start
                    .iter()
                    .map(|(kind, value)| (*kind, value.clone(), "startRange")),
            );
            rest = tail;
        } else if let Some(tail) = marker.strip_prefix("{1}") {
            output.extend(
                end.iter()
                    .map(|(kind, value)| (*kind, value.clone(), "endRange")),
            );
            rest = tail;
        } else {
            output.push(("literal", "{".to_string(), "shared"));
            rest = &marker[1..];
        }
    }
    if !rest.is_empty() {
        output.push(("literal", rest.to_string(), "shared"));
    }
    output
}

fn build_range_parts(
    o: &Gc,
    start_ms: f64,
    end_ms: f64,
    kind: u8,
) -> Vec<(&'static str, String, &'static str)> {
    let start = build_parts(o, start_ms, kind);
    let end = build_parts(o, end_ms, kind);
    if start == end {
        return start
            .into_iter()
            .map(|(kind, value)| (kind, value, "shared"))
            .collect();
    }
    let Some(base_pattern) = resolved_dtf_pattern(o, &start) else {
        return fallback_range_parts(o, start, end);
    };
    let skeleton = interval_skeleton(&base_pattern);
    let Some(difference) = greatest_range_difference(o, &skeleton, &start, &end) else {
        return fallback_range_parts(o, start, end);
    };
    let Some(pattern) =
        crate::cldr_datetime_patterns::interval(dtf_cldr_locale(o), &skeleton, difference)
    else {
        return fallback_range_parts(o, start, end);
    };
    let Some((first_pattern, second_pattern)) = split_interval_pattern(pattern) else {
        return fallback_range_parts(o, start, end);
    };
    let first = apply_dtf_pattern(o, first_pattern, &start);
    let second = apply_dtf_pattern(o, second_pattern, &end);
    let boundary = first.len();
    let mut combined: Vec<_> = first
        .into_iter()
        .map(|(kind, value)| (kind, value, "startRange"))
        .chain(
            second
                .into_iter()
                .map(|(kind, value)| (kind, value, "endRange")),
        )
        .collect();

    // UTS #35 splits an interval at the first repeated field. The literal immediately adjoining
    // that split is the shared range separator, including non-dash separators such as Japanese ～.
    if boundary > 0 && combined[boundary - 1].0 == "literal" {
        combined[boundary - 1].2 = "shared";
    } else if combined
        .get(boundary)
        .is_some_and(|part| part.0 == "literal")
    {
        combined[boundary].2 = "shared";
    }

    for index in 0..combined.len() {
        if combined[index].0 == "literal" {
            if combined[index].1.contains(['–', '—', '-']) {
                combined[index].2 = "shared";
            }
            continue;
        }
        let kind = combined[index].0;
        let occurrences = combined.iter().filter(|part| part.0 == kind).count();
        let start_value = start.iter().find(|part| part.0 == kind).map(|part| &part.1);
        let end_value = end.iter().find(|part| part.0 == kind).map(|part| &part.1);
        if occurrences == 1 && start_value == end_value {
            combined[index].2 = "shared";
        }
    }
    for index in 0..combined.len() {
        if combined[index].0 != "literal" || combined[index].2 == "shared" {
            continue;
        }
        let previous_shared = combined[..index]
            .iter()
            .rev()
            .find(|part| part.0 != "literal")
            .is_some_and(|part| part.2 == "shared");
        let next_shared = combined[index + 1..]
            .iter()
            .find(|part| part.0 != "literal")
            .is_some_and(|part| part.2 == "shared");
        if previous_shared || next_shared {
            combined[index].2 = "shared";
        }
    }
    combined
}

fn build_parts(o: &Gc, ms: f64, kind: u8) -> Vec<(&'static str, String)> {
    // An absolute instant (number/Date kind 0, Temporal.Instant kind 6) is shifted into the
    // formatter's time zone; Temporal wall-clock values (kinds 1-5) already carry their local time.
    let ms = if kind == 0 || kind == 6 {
        match o.borrow().props.get("__dtf_tz").map(|p| p.value()) {
            Some(Value::Str(tz)) => {
                let epoch_sec = (ms / 1000.0).floor() as i64;
                let off_ms = crate::tz::offset_at(&tz, epoch_sec)
                    .map(|s| s as f64 * 1000.0)
                    .unwrap_or_else(|| tz_offset_ms(&tz));
                ms + off_ms
            }
            _ => ms,
        }
    } else {
        ms
    };
    let (mut y, mut mo, mut d, h, mi, s, wd) = ymd(ms);
    let iso_date = crate::temporal::IsoDate {
        year: y,
        month: mo as u8,
        day: d as u8,
    };
    // A non-Gregorian calendar renders its own numeric year/month/day via the calendar tables.
    let dcal = match o.borrow().props.get("__dtf_ca").map(|p| p.value()) {
        Some(Value::Str(s)) => s.to_string(),
        _ => "iso8601".to_string(),
    };
    let greg_cal = matches!(dcal.as_str(), "" | "gregory" | "iso8601");
    let mut leap_month = false;
    if !greg_cal {
        let f = crate::temporal::cal_fields(&dcal, iso_date);
        // The year uses cal_year_num (era-year, e.g. ethioaa amete-alem = +5500); month/day come
        // from cal_fields.
        y = crate::temporal::cal_year_num(&dcal, iso_date);
        mo = f.1 as u32;
        d = f.2 as u32;
        // Lunisolar months display their month NUMBER (leap months flagged), not the ordinal;
        // Hebrew months use the fixed CLDR index (Adar I is 6 in leap years only).
        if dcal == "chinese" || dcal == "dangi" {
            let (num, leap) = crate::temporal::lunisolar_month_num(&dcal, iso_date);
            mo = num as u32;
            leap_month = leap;
        } else if dcal == "hebrew" {
            mo = crate::temporal::hebrew_cldr_month(iso_date) as u32;
        } else if dcal == "japanese" {
            // The Japanese calendar's year is the era year (Reiwa 32), matching Temporal.
            if let (_, Some(ey)) = crate::temporal::cal_era("japanese", iso_date) {
                y = ey;
            }
        }
    }
    // A Temporal receiver restricts the displayable fields: YearMonth drops day/weekday/time,
    // MonthDay drops year/weekday/time, Date/YearMonth/MonthDay drop time, Time drops date.
    let allow = |slot: &str| -> bool {
        // A Temporal receiver without a time zone (any kind except 0/number and 6/Zoned) never
        // shows a time-zone name.
        if slot == "tzname" && matches!(kind, 1..=5) {
            return false;
        }
        match kind {
            1 => !matches!(slot, "hour" | "minute" | "second" | "dayperiod" | "fracsec"), // Date
            2 => matches!(slot, "hour" | "minute" | "second" | "dayperiod" | "fracsec"),  // Time
            4 => matches!(slot, "year" | "month" | "era"), // YearMonth
            5 => matches!(slot, "month" | "day"),          // MonthDay
            _ => true,
        }
    };
    // Read a component slot (gated by `allow`), falling back to the dateStyle/timeStyle expansion.
    let defaulted = o.borrow().props.contains("__dtf_defaults");
    let get = |k: &str| {
        let field = k.trim_start_matches("__dtf_");
        if !allow(field) {
            return None;
        }
        // A fully-defaulted formatter adapts to the receiver: a PlainTime shows no date fields.
        if defaulted && kind == 2 && matches!(field, "year" | "month" | "day" | "era") {
            return None;
        }
        let read = |key: &str| match o.borrow().props.get(key).map(|p| p.value()) {
            Some(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        };
        read(k).or_else(|| read(&k.replacen("__dtf_", "__dtfx_", 1)))
    };
    let mut parts: Vec<(&'static str, String)> = Vec::new();
    let lit = |parts: &mut Vec<(&'static str, String)>, s: &str| {
        parts.push(("literal", s.to_string()));
    };
    let cldr_loc = dtf_cldr_locale(o);

    if let Some(w) = get("__dtf_weekday") {
        const DAYS: [&str; 7] = ["sun", "mon", "tue", "wed", "thu", "fri", "sat"];
        let name = crate::cldr_datetime_patterns::weekday(cldr_loc, &w, DAYS[wd as usize])
            .unwrap_or_else(|| {
                if w == "long" {
                    WD_LONG[wd as usize]
                } else {
                    WD_SHORT[wd as usize]
                }
            });
        parts.push(("weekday", name.to_string()));
        lit(&mut parts, ", ");
    }

    let cal_key = if greg_cal { "gregory" } else { dcal.as_str() };
    // Named months come from CLDR (localized, per-calendar); missing → numeric.
    let mut month_is_named = false;
    // ICU marks a chinese/dangi leap month with a "bis" suffix in numeric contexts.
    let bis = if leap_month { "bis" } else { "" };
    let month_str = get("__dtf_month").map(|m| {
        // Hebrew months always render by name regardless of the requested width (CLDR-15510).
        if dcal == "hebrew" {
            if let Some(n) = crate::cldr_dates::month_name(cldr_loc, "hebrew", "long", mo as u8) {
                month_is_named = true;
                return n.to_string();
            }
        }
        match m.as_str() {
            w @ ("long" | "short" | "narrow") => {
                if let Some(n) = crate::cldr_dates::month_name(cldr_loc, cal_key, w, mo as u8) {
                    month_is_named = true;
                    n.to_string()
                } else {
                    format!("{mo}{bis}")
                }
            }
            "2-digit" => format!("{mo:02}{bis}"),
            _ => format!("{mo}{bis}"),
        }
    });
    let day_str = get("__dtf_day").map(|dd| {
        if dd == "2-digit" {
            format!("{d:02}")
        } else {
            format!("{d}")
        }
    });
    // When an era is shown for the Gregorian calendar, the year is the positive era year.
    let disp_year = if greg_cal && get("__dtf_era").is_some() && y <= 0 {
        1 - y
    } else {
        y
    };
    let year_str = get("__dtf_year").map(|yy| {
        if yy == "2-digit" {
            format!("{:02}", (disp_year % 100 + 100) % 100)
        } else {
            format!("{disp_year}")
        }
    });
    // chinese/dangi years render as related Gregorian year + sexagenary cycle name.
    let year_cluster: Option<Vec<(&'static str, String)>> =
        if (dcal == "chinese" || dcal == "dangi") && year_str.is_some() {
            const STEMS: [&str; 10] = ["甲", "乙", "丙", "丁", "戊", "己", "庚", "辛", "壬", "癸"];
            const BRANCHES: [&str; 12] = [
                "子", "丑", "寅", "卯", "辰", "巳", "午", "未", "申", "酉", "戌", "亥",
            ];
            let name = format!(
                "{}{}",
                STEMS[(y - 4).rem_euclid(10) as usize],
                BRANCHES[(y - 4).rem_euclid(12) as usize]
            );
            Some(if cldr_loc.starts_with("zh") {
                vec![
                    ("relatedYear", y.to_string()),
                    ("yearName", name),
                    ("literal", "年".to_string()),
                ]
            } else {
                vec![
                    ("relatedYear", y.to_string()),
                    ("literal", "(".to_string()),
                    ("yearName", name),
                    ("literal", ")".to_string()),
                ]
            })
        } else {
            None
        };
    let named_month = month_is_named;
    let have_date = month_str.is_some() || day_str.is_some() || year_str.is_some();

    if named_month {
        if let Some(m) = &month_str {
            parts.push(("month", m.clone()));
        }
        if let Some(dd) = &day_str {
            lit(&mut parts, " ");
            parts.push(("day", dd.clone()));
        }
        if let Some(yy) = &year_str {
            lit(&mut parts, ", ");
            match &year_cluster {
                Some(c) => parts.extend(c.iter().cloned()),
                None => parts.push(("year", yy.clone())),
            }
        }
    } else if have_date {
        // Numeric date: field order and separator follow the locale's CLDR short pattern
        // (en: M/D/Y, de: D.M.Y, ja: Y/M/D, ...). Default is M/D/Y with "/".
        let lang = {
            let loc = match o.borrow().props.get("__dtf_locale").map(|p| p.value()) {
                Some(Value::Str(s)) => s.to_string(),
                _ => "en".to_string(),
            };
            loc.split('-').next().unwrap_or("en").to_string()
        };
        // (order, separator, trailing separator after the year — de/fi style "1.2.1970" has none)
        let (order, sep): (&[u8], &str) = match lang.as_str() {
            "de" | "ru" | "fi" | "cs" | "sk" | "uk" | "bg" | "sr" | "lv" | "et" | "ro" | "no"
            | "nb" | "da" | "is" | "mk" | "sq" | "hr" => (b"dmy", "."),
            "fr" | "es" | "it" | "pt" | "el" | "id" | "ms" | "vi" | "he" | "ar" | "th" | "ca"
            | "gl" | "uz" => (b"dmy", "/"),
            "nl" | "sw" => (b"dmy", "-"),
            "pl" | "sl" => (b"dmy", "."),
            "hu" | "lt" | "sv" => (b"ymd", "."),
            "ja" | "zh" | "ko" | "eu" => (b"ymd", "/"),
            _ => (b"mdy", "/"),
        };
        // Hungarian/Swedish/Lithuanian actually use "y. m. d." / "y-m-d"; keep it simple with
        // the dominant separator per language above.
        let (order, sep): (&[u8], &str) = match lang.as_str() {
            "hu" => (b"ymd", ". "),
            "lt" | "sv" => (b"ymd", "-"),
            _ => (order, sep),
        };
        let mut first = true;
        for f in order {
            match f {
                b'm' => {
                    if let Some(m) = &month_str {
                        if !first {
                            lit(&mut parts, sep);
                        }
                        parts.push(("month", m.clone()));
                        first = false;
                    }
                }
                b'd' => {
                    if let Some(dd) = &day_str {
                        if !first {
                            lit(&mut parts, sep);
                        }
                        parts.push(("day", dd.clone()));
                        first = false;
                    }
                }
                _ => {
                    if let Some(yy) = &year_str {
                        if !first {
                            lit(&mut parts, sep);
                        }
                        match &year_cluster {
                            Some(c) => parts.extend(c.iter().cloned()),
                            None => parts.push(("year", yy.clone())),
                        }
                        first = false;
                    }
                }
            }
        }
    }
    // The era follows the date (CLDR name for the calendar/locale; the code is the fallback).
    if let (true, Some(width)) = (have_date, get("__dtf_era")) {
        let (code, _) =
            crate::temporal::cal_era(if greg_cal { "gregory" } else { &dcal }, iso_date);
        if let Some(code) = code {
            // Japanese named eras resolve by code; its ce/bce fallback uses the Gregorian names.
            let jp_name = if dcal == "japanese" {
                match code {
                    "reiwa" => Some("Reiwa"),
                    "heisei" => Some("Heisei"),
                    "showa" => Some("Shōwa"),
                    "taisho" => Some("Taishō"),
                    "meiji" => Some("Meiji"),
                    _ => None,
                }
            } else {
                None
            };
            let idx = era_cldr_index(if greg_cal { "gregory" } else { &dcal }, code);
            let ekey = if cal_key == "japanese" {
                "gregory"
            } else {
                cal_key
            };
            let name = jp_name
                .map(|s| s.to_string())
                .or_else(|| {
                    crate::cldr_dates::era_name(cldr_loc, ekey, &width, idx).map(|s| s.to_string())
                })
                .unwrap_or_else(|| code.to_uppercase());
            lit(&mut parts, " ");
            parts.push(("era", name));
        }
    }

    // A PlainTime (kind 2), or a fully-defaulted formatter over a PlainDateTime (kind 3), shows the
    // natural h:m:s even without explicit time components (the PlainDateTime format defaults to all).
    let dtf_defaulted = o.borrow().props.contains("__dtf_defaults");
    let has_frac = o.borrow().props.contains("__dtf_fracsec");
    let day_period = get("__dtf_dayperiod");
    let time_defaulted = (kind == 2 || ((kind == 3 || kind == 6) && dtf_defaulted))
        && get("__dtf_hour").is_none()
        && get("__dtf_minute").is_none()
        && get("__dtf_second").is_none()
        && day_period.is_none()
        && !has_frac;
    let have_time = time_defaulted
        || day_period.is_some()
        || has_frac
        || get("__dtf_hour").is_some()
        || get("__dtf_minute").is_some()
        || get("__dtf_second").is_some();
    if have_time {
        if have_date {
            lit(&mut parts, ", ");
        }
        let has_hour = time_defaulted || get("__dtf_hour").is_some();
        // An explicit dayPeriod field replaces the AM/PM marker with a flexible period word; a plain
        // AM/PM marker only appears alongside a 12-hour clock. When the hour cycle wasn't resolved
        // (a default formatter over a PlainTime), use the locale default (en is 12-hour, others 24).
        let cycle = match o.borrow().props.get("__dtf_hourcycle").map(|p| p.value()) {
            Some(Value::Str(s)) => Some(s.to_string()),
            _ => None,
        };
        let use12 = day_period.is_some()
            || match cycle.as_deref() {
                Some("h11") | Some("h12") => true,
                Some(_) => false,
                None => match o.borrow().props.get("__dtf_hour12").map(|p| p.value()) {
                    Some(Value::Bool(b)) => b,
                    _ => cldr_loc == "en",
                },
            };
        // Map the 0..23 hour into the display range of the resolved cycle: h11 [0,11], h12 [1,12],
        // h23 [0,23], h24 [1,24].
        let disp_h = match cycle.as_deref() {
            Some("h11") => h % 12,
            Some("h12") => {
                if h % 12 == 0 {
                    12
                } else {
                    h % 12
                }
            }
            Some("h24") => {
                if h == 0 {
                    24
                } else {
                    h
                }
            }
            Some("h23") => h,
            Some(_) => h,
            None if use12 => {
                if h % 12 == 0 {
                    12
                } else {
                    h % 12
                }
            }
            None => h,
        };
        let ampm = if use12 {
            match &day_period {
                Some(width) => {
                    let period = day_period_key(h);
                    Some(
                        crate::cldr_datetime_patterns::day_period(cldr_loc, width, period)
                            .unwrap_or_else(|| day_period_word(h, width))
                            .to_string(),
                    )
                }
                None if has_hour => {
                    let period = if h < 12 { "am" } else { "pm" };
                    Some(
                        crate::cldr_datetime_patterns::day_period(cldr_loc, "short", period)
                            .unwrap_or(if h < 12 { "AM" } else { "PM" })
                            .to_string(),
                    )
                }
                None => None,
            }
        } else {
            None
        };
        let has_clock = time_defaulted
            || get("__dtf_hour").is_some()
            || get("__dtf_minute").is_some()
            || get("__dtf_second").is_some();
        let mut first = true;
        if time_defaulted || get("__dtf_hour").is_some() {
            // A 24-hour cycle (h23/h24) renders 2-digit even for a numeric hour (CLDR "HH"); a
            // 12-hour cycle uses 1-digit unless "2-digit" was explicitly requested.
            let pad = get("__dtf_hour").as_deref() == Some("2-digit") || !use12;
            parts.push((
                "hour",
                if pad {
                    format!("{disp_h:02}")
                } else {
                    format!("{disp_h}")
                },
            ));
            first = false;
        }
        if time_defaulted || get("__dtf_minute").is_some() {
            if !first {
                lit(&mut parts, ":");
            }
            parts.push(("minute", format!("{mi:02}")));
            first = false;
        }
        let frac_alone = has_frac && !(time_defaulted || get("__dtf_second").is_some());
        if frac_alone {
            if let Some(Value::Num(fd)) = o.borrow().props.get("__dtf_fracsec").map(|p| p.value()) {
                let ms_frac = (ms.rem_euclid(1000.0)) as u32;
                let digits = format!("{ms_frac:03}");
                parts.push(("fractionalSecond", digits[..fd as usize].to_string()));
            }
        }
        if time_defaulted || get("__dtf_second").is_some() {
            if !first {
                lit(&mut parts, ":");
            }
            parts.push(("second", format!("{s:02}")));
            // fractionalSecondDigits appends the leading digits of the millisecond fraction.
            if let Some(Value::Num(fd)) = o.borrow().props.get("__dtf_fracsec").map(|p| p.value()) {
                let ms_frac = (ms.rem_euclid(1000.0)) as u32;
                let digits = format!("{ms_frac:03}");
                // The fractional separator is the numbering system's decimal symbol (arab: U+066B).
                let sep = if matches!(dtf_nu(o).as_str(), "arab" | "arabext") {
                    "\u{066b}"
                } else {
                    "."
                };
                lit(&mut parts, sep);
                parts.push(("fractionalSecond", digits[..fd as usize].to_string()));
            }
        }
        if let Some(ap) = ampm {
            // Separate the day-period marker from the clock only when clock digits were emitted.
            if has_clock {
                lit(&mut parts, " ");
            }
            parts.push(("dayPeriod", ap));
        }
    }

    // Time-zone name (UTC only; the display form depends on the requested style).
    if let Some(style) = get("__dtf_tzname") {
        let tz = match o.borrow().props.get("__dtf_tz").map(|p| p.value()) {
            Some(Value::Str(s)) => s.to_string(),
            _ => "UTC".to_string(),
        };
        let name = tz_display_name(&tz, &style);
        if !parts.is_empty() {
            lit(&mut parts, " ");
        }
        parts.push(("timeZoneName", name));
    }

    if parts.is_empty() {
        // Default numeric date.
        parts.push(("month", format!("{mo}")));
        lit(&mut parts, "/");
        parts.push(("day", format!("{d}")));
        lit(&mut parts, "/");
        parts.push(("year", format!("{y}")));
    }
    match resolved_dtf_pattern(o, &parts) {
        Some(pattern) => apply_dtf_pattern(o, &pattern, &parts),
        None => parts,
    }
}

/// Validate and canonicalize a `timeZone` option. Offset forms (`±HH`, `±HH:MM`, `±HHMM`) are
/// checked strictly (ASCII sign, hour 00-23, minute 00-59) and normalized to `±HH:MM`; the UTC
/// aliases collapse to "UTC"; any other IANA-looking name is accepted as-is (we lack a full DB).
fn canonicalize_time_zone(tz: &str) -> Option<String> {
    let first = tz.bytes().next()?;
    if first == b'+' || first == b'-' {
        return canon_utc_offset(tz);
    }
    // A named IANA zone keeps the given identifier (case-normalized), matching Temporal — the
    // resolved timeZone preserves an alias like Asia/Calcutta rather than canonicalizing it.
    crate::tz::registry_name(tz).map(|s| s.to_string())
}

/// The signed millisecond offset of a canonical `±HH:MM[:SS]` UTC-offset zone (0 for anything else).
fn tz_offset_ms(tz: &str) -> f64 {
    let b = tz.as_bytes();
    if b.first().is_none_or(|&c| c != b'+' && c != b'-') {
        return 0.0;
    }
    let sign = if b[0] == b'-' { -1.0 } else { 1.0 };
    let mut parts = tz[1..].split(':');
    let h: f64 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
    let m: f64 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
    let s: f64 = parts.next().and_then(|x| x.parse().ok()).unwrap_or(0.0);
    sign * (h * 3_600_000.0 + m * 60_000.0 + s * 1000.0)
}

/// Parse a strict minute-precision UTC offset string, returning the `±HH:MM` canonical form.
fn canon_utc_offset(s: &str) -> Option<String> {
    let sign = s.as_bytes()[0];
    let rest = &s[1..];
    if rest.len() < 2 || !rest.as_bytes()[..2].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let h: u32 = rest[..2].parse().ok()?;
    if h > 23 {
        return None;
    }
    let after = &rest[2..];
    let m: u32 = if after.is_empty() {
        0
    } else {
        let digits = after.strip_prefix(':').unwrap_or(after);
        if digits.len() != 2 || !digits.bytes().all(|c| c.is_ascii_digit()) {
            return None;
        }
        digits.parse().ok()?
    };
    if m > 59 {
        return None;
    }
    // A zero offset canonicalizes to "+00:00" regardless of the written sign.
    let sign_c = if sign == b'-' && (h != 0 || m != 0) {
        '-'
    } else {
        '+'
    };
    Some(format!("{sign_c}{h:02}:{m:02}"))
}

/// The English flexible day-period word for an hour under a `dayPeriod` width (`long`/`short` share
/// forms; only `narrow` noon differs).
fn day_period_key(h: u32) -> &'static str {
    if h == 12 {
        "noon"
    } else if h < 12 {
        "morning1"
    } else if h < 18 {
        "afternoon1"
    } else if h < 21 {
        "evening1"
    } else {
        "night1"
    }
}

fn day_period_word(h: u32, width: &str) -> &'static str {
    if h == 12 {
        if width == "narrow" {
            "n"
        } else {
            "noon"
        }
    } else if h < 12 {
        "in the morning"
    } else if h < 18 {
        "in the afternoon"
    } else if h < 21 {
        "in the evening"
    } else {
        "at night"
    }
}

/// The time-zone display name for the (UTC) zone under a `timeZoneName` style.
fn tz_display_name(tz: &str, style: &str) -> String {
    if tz == "UTC" {
        return match style {
            "long" | "longGeneric" => "Coordinated Universal Time",
            "longOffset" => "GMT",
            _ => "UTC", // short, shortOffset, shortGeneric
        }
        .to_string();
    }
    // An offset zone renders as a GMT offset (a zero offset is plain "GMT"; minutes only when
    // nonzero).
    if tz.starts_with('+') || tz.starts_with('-') {
        let (sign, rest) = (&tz[..1], &tz[1..]);
        let mut it = rest.split(':');
        let h: i64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        let m: i64 = it.next().and_then(|x| x.parse().ok()).unwrap_or(0);
        return if h == 0 && m == 0 {
            "GMT".to_string()
        } else if m == 0 {
            format!("GMT{sign}{h}")
        } else {
            format!("GMT{sign}{h}:{m:02}")
        };
    }
    // Named zones: the short styles render a generic GMT offset; the long styles use the CLDR
    // standard-time metazone name where known, else the canonical identifier.
    let canon = crate::tz::canonicalize(tz).unwrap_or(tz).to_string();
    match style {
        "short" | "shortOffset" | "shortGeneric" => {
            let off = crate::tz::offset_at(&canon, 0).unwrap_or(0) as i64;
            let sign = if off < 0 { "-" } else { "+" };
            let a = off.abs();
            let (h, m) = (a / 3600, (a % 3600) / 60);
            if m == 0 {
                format!("GMT{sign}{h}")
            } else {
                format!("GMT{sign}{h}:{m:02}")
            }
        }
        _ => match canon.as_str() {
            "Europe/Vienna" | "Europe/Berlin" | "Europe/Paris" | "Europe/Rome"
            | "Europe/Madrid" | "Europe/Amsterdam" | "Europe/Brussels" | "Europe/Prague"
            | "Europe/Warsaw" | "Europe/Budapest" | "Europe/Stockholm" | "Europe/Oslo"
            | "Europe/Copenhagen" | "Europe/Zurich" => "Central European Standard Time".to_string(),
            "Europe/London" => "Greenwich Mean Time".to_string(),
            "America/New_York" => "Eastern Standard Time".to_string(),
            "America/Chicago" => "Central Standard Time".to_string(),
            "America/Denver" => "Mountain Standard Time".to_string(),
            "America/Los_Angeles" => "Pacific Standard Time".to_string(),
            "Asia/Tokyo" => "Japan Standard Time".to_string(),
            "Asia/Shanghai" => "China Standard Time".to_string(),
            "Asia/Kolkata" => "India Standard Time".to_string(),
            _ => canon,
        },
    }
}

fn resolved_options(i: &mut Interp, this: Value, _a: &[Value]) -> Result<Value, Value> {
    let o = crate::intl::brand_slot_legacy(i, &this, "__dtf", "Intl.DateTimeFormat")?;
    let res = i.new_object();
    let put = |i: &mut Interp, res: &Gc, k: &str, slot: &str| {
        if let Some(v) = o.borrow().props.get(slot).map(|p| p.value()) {
            set_data(res, k, v);
        }
        let _ = i;
    };
    put(i, &res, "locale", "__dtf_locale");
    put(i, &res, "calendar", "__dtf_ca");
    put(i, &res, "numberingSystem", "__dtf_nu");
    put(i, &res, "timeZone", "__dtf_tz");
    // hourCycle/hour12 are resolved internally for every formatter but only surface in
    // resolvedOptions when the resolved pattern actually shows an hour.
    if matches!(
        o.borrow().props.get("__dtf_hourshown").map(|p| p.value()),
        Some(Value::Bool(true))
    ) {
        put(i, &res, "hourCycle", "__dtf_hourcycle");
        put(i, &res, "hour12", "__dtf_hour12");
    }
    put(i, &res, "weekday", "__dtf_weekday");
    put(i, &res, "era", "__dtf_era");
    put(i, &res, "year", "__dtf_year");
    put(i, &res, "month", "__dtf_month");
    put(i, &res, "day", "__dtf_day");
    put(i, &res, "dayPeriod", "__dtf_dayperiod");
    put(i, &res, "hour", "__dtf_hour");
    put(i, &res, "minute", "__dtf_minute");
    put(i, &res, "second", "__dtf_second");
    put(i, &res, "fractionalSecondDigits", "__dtf_fracsec");
    put(i, &res, "timeZoneName", "__dtf_tzname");
    put(i, &res, "dateStyle", "__dtf_datestyle");
    put(i, &res, "timeStyle", "__dtf_timestyle");
    Ok(Value::Obj(res))
}
