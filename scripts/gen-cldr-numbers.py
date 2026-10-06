#!/usr/bin/env python3
"""Generate NumberFormat locale data for every locale Lumen advertises from CLDR 48."""

from __future__ import annotations

import hashlib
import json
import pathlib
import re
import urllib.request


TAG = "48.0.0"
SOURCE_COMMIT = "4d06be52b51bb2f75688d0abe55c52a66afed790"
ROOT = pathlib.Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "crates/lumen/src/cldr_numbers.rs"
BLOB_OUTPUT = ROOT / "crates/lumen/src/cldr_numbers.data"
MANIFEST_OUTPUT = ROOT / "crates/lumen/src/cldr_numbers.inputs.json"
REPOSITORY = "https://github.com/unicode-org/cldr-json"
BASE = (
    f"https://raw.githubusercontent.com/unicode-org/cldr-json/{SOURCE_COMMIT}/"
    "cldr-json"
)
LOCALES = [
    "en", "en-IN", "de", "fr", "es", "it", "pt", "pt-PT", "nl", "ja",
    "zh", "zh-Hant", "ko", "ru", "ar", "sr", "th", "gv", "sl", "pl",
    "si", "ln", "sv", "hi",
]
INPUT_HASHES: dict[str, str] = {}


def download(package: str, locale: str, file: str) -> dict:
    url = f"{BASE}/{package}/main/{locale}/{file}.json"
    request = urllib.request.Request(
        url, headers={"User-Agent": "Lumen CLDR NumberFormat generator"}
    )
    with urllib.request.urlopen(request) as response:
        payload = response.read()
    INPUT_HASHES[url] = hashlib.sha256(payload).hexdigest()
    return json.loads(payload)["main"][locale]


def download_url(url: str) -> dict:
    request = urllib.request.Request(
        url, headers={"User-Agent": "Lumen CLDR NumberFormat generator"}
    )
    with urllib.request.urlopen(request) as response:
        payload = response.read()
    INPUT_HASHES[url] = hashlib.sha256(payload).hexdigest()
    return json.loads(payload)


def rust_string(value: str) -> str:
    rendered = json.dumps(value, ensure_ascii=False)
    # Keep source auditable and satisfy Rust's invisible-character lint without altering CLDR.
    for character in ("\u200b", "\u2060", "\ufeff"):
        rendered = rendered.replace(character, f"\\u{{{ord(character):x}}}")
    return rendered


class StringPool:
    """Stable, allocation-free-at-runtime storage for generated CLDR text."""

    U16_MAX = (1 << 16) - 1
    U32_MAX = (1 << 32) - 1

    def __init__(self) -> None:
        self.data = bytearray()
        self.refs: dict[str, tuple[int, int, int]] = {}

    def intern(self, value: str) -> int:
        found = self.refs.get(value)
        if found is not None:
            return found[0]
        string_id = len(self.refs)
        if string_id > self.U16_MAX:
            raise ValueError("CLDR string count exceeds the u16 id format")
        encoded = value.encode("utf-8")
        offset = len(self.data)
        end = offset + len(encoded)
        if end > self.U32_MAX:
            raise ValueError("CLDR string blob exceeds the u32 offset format")
        self.data.extend(encoded)
        self.refs[value] = (string_id, offset, len(encoded))
        return string_id


RUST_STRING = re.compile(r'"(?:\\.|[^"\\])*"')


def decode_rust_string(literal: str) -> str:
    """Decode the JSON-compatible Rust literals emitted by `rust_string`."""
    body = literal[1:-1]
    body = re.sub(
        r"\\u\{([0-9a-fA-F]+)\}",
        lambda match: "\\u" + f"{int(match.group(1), 16):04x}",
        body,
    )
    return json.loads('"' + body + '"')


def pack_generated_source(source: str) -> tuple[str, bytes, int]:
    """Pack every string-valued CLDR table field while retaining lookup APIs.

    The existing generator first builds a canonical source representation from its
    parsed CLDR rows. This deterministic final pass converts only table literals;
    code constants such as currency digits and locale fallback arms stay ordinary
    Rust literals.
    """
    pool = StringPool()
    output: list[str] = []
    active: str | None = None
    table_count = 0
    declaration_types = {
        "SYMBOLS": "static SYMBOLS: &[(TextId, TextId, SymbolsIds)] = &[",
        "PATTERNS": "static PATTERNS: &[(TextId, TextId, TextId, PatternIds)] = &[",
        "COMPACT": "static COMPACT: &[(TextId, TextId, TextId, u8, TextId, CompactIds)] = &[",
        "COMPACT_MAX": "static COMPACT_MAX: &[(TextId, TextId, TextId, u8)] = &[",
        "CURRENCY_UNITS": "static CURRENCY_UNITS: &[(TextId, TextId, TextId)] = &[",
    }

    for line in source.splitlines():
        if line.startswith("static "):
            name = line[7:].split(":", 1)[0]
            if name in declaration_types:
                active = name
            elif name.startswith("C_"):
                active = "CURRENCY"
                line = line.replace(
                    "&[(&str, Currency)] = &[",
                    "&[(TextId, CurrencyIds)] = &[",
                )
            elif name.startswith("CN_"):
                active = "CURRENCY_NAMES"
                line = line.replace(
                    "&[(&str, &str, &str)] = &[",
                    "&[(TextId, TextId, TextId)] = &[",
                )
            if name in declaration_types:
                line = declaration_types[name]
            if active is not None:
                table_count += int(line.startswith("static "))
                if line.startswith("static "):
                    output.append("#[rustfmt::skip]")

        if active is not None and not line.startswith("static "):
            if line.strip() == "];":
                output.append(line)
                active = None
                continue
            if line.strip():
                def to_id(match: re.Match[str]) -> str:
                    return str(pool.intern(decode_rust_string(match.group(0))))

                line = RUST_STRING.sub(to_id, line)
                if active == "SYMBOLS":
                    line = line.replace("Symbols {", "SymbolsIds {")
                elif active == "PATTERNS":
                    line = line.replace("Pattern {", "PatternIds {")
                elif active == "COMPACT":
                    line = line.replace("Compact {", "CompactIds {")
                elif active == "CURRENCY":
                    line = line.replace("Currency {", "CurrencyIds {")
        output.append(line)

    expected_tables = 4 + len(LOCALES) * 2 + 1
    if table_count != expected_tables:
        raise ValueError(
            f"expected {expected_tables} CLDR number tables, transformed {table_count}"
        )
    if active is not None:
        raise ValueError(f"unterminated CLDR table: {active}")

    pool_source = [
        "const CLDR_STRINGS: &str = include_str!(\"cldr_numbers.data\");",
        "type TextId = u16;",
        "",
        "#[derive(Clone, Copy)]",
        "struct SymbolsIds {",
        "    decimal: TextId,",
        "    group: TextId,",
        "    percent: TextId,",
        "    plus: TextId,",
        "    minus: TextId,",
        "    approximately: TextId,",
        "    exponential: TextId,",
        "    infinity: TextId,",
        "    nan: TextId,",
        "    primary_group: usize,",
        "    secondary_group: usize,",
        "    minimum_grouping: usize,",
        "}",
        "#[derive(Clone, Copy)]",
        "struct PatternIds {",
        "    positive_prefix: TextId,",
        "    positive_suffix: TextId,",
        "    negative_prefix: TextId,",
        "    negative_suffix: TextId,",
        "}",
        "#[derive(Clone, Copy)]",
        "struct CompactIds {",
        "    exponent: i32,",
        "    has_number: bool,",
        "    prefix: TextId,",
        "    suffix: TextId,",
        "}",
        "#[derive(Clone, Copy)]",
        "struct CurrencyIds {",
        "    name: TextId,",
        "    symbol: TextId,",
        "    narrow: TextId,",
        "}",
        "",
        "#[rustfmt::skip]",
        f"static CLDR_REFS: [(u32, u32); {len(pool.refs)}] = [",
    ]
    pool_source.extend(f"    ({offset}, {length})," for _, offset, length in pool.refs.values())
    pool_source.extend(
        [
            "];",
            "",
            "#[inline]",
            "fn text(id: TextId) -> &'static str {",
            "    let (offset, len) = CLDR_REFS[id as usize];",
            "    let start = offset as usize;",
            "    let end = start + len as usize;",
            "    &CLDR_STRINGS[start..end]",
            "}",
            "",
        ]
    )
    result = "\n".join(output)
    header = "\n".join(pool_source)
    first_newline = result.find("\n")
    result = result[: first_newline + 1] + "\n" + header + result[first_newline + 1 :]

    def replace_function(start: str, end: str, replacement: str) -> None:
        nonlocal result
        begin = result.index(start)
        finish = result.index(end, begin)
        result = result[:begin] + replacement + "\n\n" + result[finish:]

    replace_function(
        "pub(crate) fn symbols(",
        "pub(crate) fn pattern(",
        '''pub(crate) fn symbols(loc: &str, nu: &str) -> Symbols {
    let find = |locale: &str, system: &str| {
        SYMBOLS
            .binary_search_by(|row| {
                text(row.0)
                    .cmp(locale)
                    .then_with(|| text(row.1).cmp(system))
            })
            .ok()
            .map(|index| {
                let value = SYMBOLS[index].2;
                Symbols {
                    decimal: text(value.decimal),
                    group: text(value.group),
                    percent: text(value.percent),
                    plus: text(value.plus),
                    minus: text(value.minus),
                    approximately: text(value.approximately),
                    exponential: text(value.exponential),
                    infinity: text(value.infinity),
                    nan: text(value.nan),
                    primary_group: value.primary_group,
                    secondary_group: value.secondary_group,
                    minimum_grouping: value.minimum_grouping,
                }
            })
    };
    let system_fallback = match nu {
        "arab" | "arabext" => find("ar", "arab"),
        "deva" => find("hi", "deva"),
        _ => None,
    };
    find(loc, nu)
        .or(system_fallback)
        .or_else(|| find(loc, "latn"))
        .or_else(|| find("en", "latn"))
        .expect("generated English number symbols")
}''',
    )
    replace_function(
        "pub(crate) fn pattern(",
        "fn compact_find(",
        '''pub(crate) fn pattern(loc: &str, nu: &str, kind: &str) -> Pattern {
    let find = |locale: &str, system: &str| {
        PATTERNS
            .binary_search_by(|row| {
                text(row.0)
                    .cmp(locale)
                    .then_with(|| text(row.1).cmp(system))
                    .then_with(|| text(row.2).cmp(kind))
            })
            .ok()
            .map(|index| {
                let value = PATTERNS[index].3;
                Pattern {
                    positive_prefix: text(value.positive_prefix),
                    positive_suffix: text(value.positive_suffix),
                    negative_prefix: text(value.negative_prefix),
                    negative_suffix: text(value.negative_suffix),
                }
            })
    };
    find(loc, nu)
        .or_else(|| find(loc, "latn"))
        .or_else(|| find("en", "latn"))
        .expect("generated English number pattern")
}''',
    )
    replace_function(
        "fn compact_find(",
        "fn compact_max(",
        '''fn compact_find(
    loc: &str,
    nu: &str,
    display: &str,
    magnitude: u8,
    selector: &str,
) -> Option<Compact> {
    COMPACT
        .binary_search_by(|row| {
            text(row.0)
                .cmp(loc)
                .then_with(|| text(row.1).cmp(nu))
                .then_with(|| text(row.2).cmp(display))
                .then_with(|| row.3.cmp(&magnitude))
                .then_with(|| text(row.4).cmp(selector))
        })
        .ok()
        .map(|index| {
            let value = COMPACT[index].5;
            Compact {
                exponent: value.exponent,
                has_number: value.has_number,
                prefix: text(value.prefix),
                suffix: text(value.suffix),
            }
        })
}''',
    )
    replace_function(
        "fn compact_max(",
        "pub(crate) fn compact(",
        '''fn compact_max(loc: &str, nu: &str, display: &str) -> Option<u8> {
    let find = |system: &str| {
        COMPACT_MAX
            .binary_search_by(|row| {
                text(row.0)
                    .cmp(loc)
                    .then_with(|| text(row.1).cmp(system))
                    .then_with(|| text(row.2).cmp(display))
            })
            .ok()
            .map(|index| COMPACT_MAX[index].3)
    };
    find(nu).or_else(|| find("latn"))
}''',
    )
    replace_function(
        "pub(crate) fn compact(",
        "fn currency_rows(",
        '''pub(crate) fn compact(
    loc: &str,
    nu: &str,
    display: &str,
    magnitude: u8,
    category: &str,
    exact_one: bool,
) -> Option<Compact> {
    // ComputeExponentForMagnitude caps an out-of-range magnitude at the largest
    // available compact pattern before deriving that pattern's exponent.
    let magnitude = magnitude.min(compact_max(loc, nu, display)?);
    let get = |locale: &str, system: &str, selector: &str| {
        compact_find(locale, system, display, magnitude, selector)
    };
    if exact_one {
        if let Some(value) = get(loc, nu, "1").or_else(|| get(loc, "latn", "1")) {
            return Some(value);
        }
    }
    for selector in [category, "other"] {
        if let Some(value) = get(loc, nu, selector).or_else(|| get(loc, "latn", selector)) {
            return Some(value);
        }
    }
    None
}''',
    )
    result = result.replace(
        "fn currency_rows(loc: &str) -> &'static [(&'static str, Currency)] {",
        "fn currency_rows(loc: &str) -> &'static [(TextId, CurrencyIds)] {",
    )
    result = result.replace(
        "    C_EN.iter().map(|row| row.0)",
        "    C_EN.iter().map(|row| text(row.0))",
    )
    result = result.replace(
        "fn currency_name_rows(loc: &str) -> &'static [(&'static str, &'static str, &'static str)] {",
        "fn currency_name_rows(loc: &str) -> &'static [(TextId, TextId, TextId)] {",
    )
    replace_function(
        "pub(crate) fn currency(loc:",
        "static CURRENCY_UNITS:",
        '''pub(crate) fn currency(loc: &str, code: &str, category: &str) -> Option<Currency> {
    let rows = currency_rows(loc);
    let index = rows.binary_search_by(|row| text(row.0).cmp(code)).ok()?;
    let packed = rows[index].1;
    let mut value = Currency {
        name: text(packed.name),
        symbol: text(packed.symbol),
        narrow: text(packed.narrow),
    };
    let names = currency_name_rows(loc);
    if let Ok(index) = names.binary_search_by(|row| {
        text(row.0)
            .cmp(code)
            .then_with(|| text(row.1).cmp(category))
    }) {
        value.name = text(names[index].2);
    }
    Some(value)
}''',
    )
    replace_function(
        "pub(crate) fn currency_unit(",
        "pub(crate) fn currency_digits(",
        '''pub(crate) fn currency_unit(loc: &str, category: &str) -> &'static str {
    CURRENCY_UNITS
        .binary_search_by(|row| text(row.0).cmp(loc).then_with(|| text(row.1).cmp(category)))
        .ok()
        .map(|index| text(CURRENCY_UNITS[index].2))
        .or_else(|| {
            CURRENCY_UNITS
                .binary_search_by(|row| text(row.0).cmp(loc).then_with(|| text(row.1).cmp("other")))
                .ok()
                .map(|index| text(CURRENCY_UNITS[index].2))
        })
        .unwrap_or("{0} {1}")
}''',
    )

    result += "\n#[cfg(test)]\nconst GENERATED_LOCALES: &[&str] = &[\n"
    locale_line = "    "
    for locale in LOCALES:
        entry = f'"{locale}", '
        if len(locale_line) + len(entry) > 100:
            result += locale_line.rstrip() + "\n"
            locale_line = "    "
        locale_line += entry
    if locale_line.strip():
        result += locale_line.rstrip() + "\n"
    result += "];\n\n#[cfg(test)]\n#[path = \"cldr_numbers_packed_tests.rs\"]\nmod packed_tests;\n"
    return result, bytes(pool.data), len(pool.refs)


def split_subpatterns(pattern: str) -> list[str]:
    quoted = False
    for index, character in enumerate(pattern):
        if character == "'":
            if index + 1 < len(pattern) and pattern[index + 1] == "'":
                continue
            quoted = not quoted
        elif character == ";" and not quoted:
            return [pattern[:index], pattern[index + 1 :]]
    return [pattern]


def affix(value: str) -> str:
    output: list[str] = []
    quoted = False
    index = 0
    while index < len(value):
        character = value[index]
        if character == "'":
            if index + 1 < len(value) and value[index + 1] == "'":
                output.append("'")
                index += 2
                continue
            quoted = not quoted
            index += 1
            continue
        if not quoted:
            replacement = {
                "¤": "{currency}",
                "%": "{percentSign}",
                "-": "{minusSign}",
                "+": "{plusSign}",
            }.get(character)
            if replacement is not None:
                output.append(replacement)
                index += 1
                continue
        output.append(character)
        index += 1
    return "".join(output)


def parse_pattern(pattern: str) -> tuple[str, str, str, str]:
    pieces = split_subpatterns(pattern)

    def one(piece: str) -> tuple[str, str]:
        quoted = False
        numeric: list[int] = []
        index = 0
        while index < len(piece):
            character = piece[index]
            if character == "'":
                if index + 1 < len(piece) and piece[index + 1] == "'":
                    index += 2
                    continue
                quoted = not quoted
            elif not quoted and character in "#0,.E+":
                numeric.append(index)
            index += 1
        if not numeric:
            raise RuntimeError(f"number pattern has no numeric field: {piece!r}")
        return affix(piece[: numeric[0]]), affix(piece[numeric[-1] + 1 :])

    positive = one(pieces[0])
    negative = one(pieces[1]) if len(pieces) == 2 else (
        "{minusSign}" + positive[0],
        positive[1],
    )
    return positive[0], positive[1], negative[0], negative[1]


def grouping(pattern: str) -> tuple[int, int]:
    number = split_subpatterns(pattern)[0]
    integer = number.split(".", 1)[0]
    groups = integer.split(",")
    if len(groups) == 1:
        return 0, 0
    counts = [sum(character in "#0" for character in group) for group in groups]
    primary = counts[-1]
    secondary = counts[-2] if len(counts) > 2 else primary
    return primary, secondary


def pattern_row(
    rows: list[tuple[str, str, str, tuple[str, str, str, str]]],
    locale: str,
    number_system: str,
    kind: str,
    pattern: str | None,
) -> None:
    if pattern is not None:
        rows.append((locale, number_system, kind, parse_pattern(pattern)))


def ident(prefix: str, *parts: str) -> str:
    text = "_".join((prefix, *parts)).upper()
    return "".join(character if character.isalnum() else "_" for character in text)


def main() -> None:
    INPUT_HASHES.clear()
    symbols_rows: list[tuple] = []
    pattern_rows: list[tuple] = []
    compact_rows: list[tuple] = []
    currency_base: dict[str, list[tuple[str, str, str, str]]] = {}
    currency_names: dict[str, list[tuple[str, str, str]]] = {}
    currency_unit_rows: list[tuple[str, str, str]] = []

    for locale in LOCALES:
        numbers = download("cldr-numbers-full", locale, "numbers")["numbers"]
        minimum_grouping = int(numbers.get("minimumGroupingDigits", "1"))
        number_systems = {
            key.removeprefix("symbols-numberSystem-")
            for key in numbers
            if key.startswith("symbols-numberSystem-")
        }
        for number_system in sorted(number_systems):
            values = numbers[f"symbols-numberSystem-{number_system}"]
            decimal_formats = numbers.get(f"decimalFormats-numberSystem-{number_system}")
            if decimal_formats is None:
                decimal_formats = numbers.get("decimalFormats-numberSystem-latn")
            standard = decimal_formats["standard"]
            primary, secondary = grouping(standard)
            symbols_rows.append(
                (
                    locale,
                    number_system,
                    values.get("decimal", "."),
                    values.get("group", ","),
                    values.get("percentSign", "%"),
                    values.get("plusSign", "+"),
                    values.get("minusSign", "-"),
                    values.get("approximatelySign", "~"),
                    values.get("exponential", "E"),
                    values.get("infinity", "∞"),
                    values.get("nan", "NaN"),
                    primary,
                    secondary,
                    minimum_grouping,
                )
            )

            pattern_row(pattern_rows, locale, number_system, "decimal", standard)
            percent = numbers.get(f"percentFormats-numberSystem-{number_system}")
            if percent is None:
                percent = numbers.get("percentFormats-numberSystem-latn")
            if percent is not None:
                pattern_row(
                    pattern_rows, locale, number_system, "percent", percent.get("standard")
                )
            scientific = numbers.get(f"scientificFormats-numberSystem-{number_system}")
            if scientific is None:
                scientific = numbers.get("scientificFormats-numberSystem-latn")
            if scientific is not None:
                pattern_row(
                    pattern_rows,
                    locale,
                    number_system,
                    "scientific",
                    scientific.get("standard"),
                )
            currency = numbers.get(f"currencyFormats-numberSystem-{number_system}")
            if currency is None:
                currency = numbers.get("currencyFormats-numberSystem-latn")
            if currency is not None:
                pattern_row(
                    pattern_rows, locale, number_system, "currency", currency.get("standard")
                )
                pattern_row(
                    pattern_rows,
                    locale,
                    number_system,
                    "currencyAlpha",
                    currency.get("standard-alphaNextToNumber", currency.get("standard")),
                )
                pattern_row(
                    pattern_rows,
                    locale,
                    number_system,
                    "accounting",
                    currency.get("accounting", currency.get("standard")),
                )
                pattern_row(
                    pattern_rows,
                    locale,
                    number_system,
                    "accountingAlpha",
                    currency.get(
                        "accounting-alphaNextToNumber",
                        currency.get("accounting", currency.get("standard")),
                    ),
                )

            for display in ("short", "long"):
                compact = decimal_formats.get(display, {}).get("decimalFormat", {})
                for raw_key, raw_pattern in compact.items():
                    if "-alt-" in raw_key:
                        continue
                    number, separator, selector = raw_key.partition("-count-")
                    if not separator or not number.isdigit():
                        continue
                    magnitude = len(number) - 1
                    has_number = any(character in "#0" for character in raw_pattern)
                    if has_number:
                        pre, post, _, _ = parse_pattern(raw_pattern)
                    else:
                        pre, post = affix(raw_pattern), ""
                    zero_count = raw_pattern.count("0")
                    exponent = (
                        0
                        if raw_pattern == "0"
                        else magnitude - max(zero_count, 1) + 1
                    )
                    compact_rows.append(
                        (
                            locale,
                            number_system,
                            display,
                            magnitude,
                            selector,
                            exponent,
                            has_number,
                            pre,
                            post,
                        )
                    )

        currencies = download("cldr-numbers-full", locale, "currencies")[
            "numbers"
        ]["currencies"]
        bases = currency_base.setdefault(locale, [])
        names = currency_names.setdefault(locale, [])
        for code, record in currencies.items():
            display_name = record.get("displayName", code)
            symbol = record.get("symbol", code)
            narrow = record.get("symbol-alt-narrow", symbol)
            bases.append((code, display_name, symbol, narrow))
            for key, name in record.items():
                if key.startswith("displayName-count-") and name != display_name:
                    names.append((code, key.removeprefix("displayName-count-"), name))

        latn_currency = numbers.get("currencyFormats-numberSystem-latn", {})
        for key, value in latn_currency.items():
            if key.startswith("unitPattern-count-"):
                currency_unit_rows.append(
                    (locale, key.removeprefix("unitPattern-count-"), value)
                )

    currency_data = download_url(
        f"{BASE}/cldr-core/supplemental/currencyData.json"
    )["supplemental"]["currencyData"]["fractions"]
    default_digits = int(currency_data["DEFAULT"]["_digits"])
    fraction_rows = sorted(
        (code, int(record.get("_digits", default_digits)))
        for code, record in currency_data.items()
        if code != "DEFAULT" and int(record.get("_digits", default_digits)) != default_digits
    )

    symbols_rows = sorted({row[:2]: row for row in symbols_rows}.values())
    pattern_rows = sorted({row[:3]: row for row in pattern_rows}.values())
    compact_rows = sorted({row[:5]: row for row in compact_rows}.values())
    compact_max_rows = sorted(
        (
            locale,
            number_system,
            display,
            max(row[3] for row in compact_rows if row[:3] == key),
        )
        for key in {row[:3] for row in compact_rows}
        for locale, number_system, display in [key]
    )
    currency_unit_rows = sorted(
        {row[:2]: row for row in currency_unit_rows}.values()
    )
    for rows in currency_base.values():
        rows.sort()
    for rows in currency_names.values():
        rows.sort()

    output = [
        "//! GENERATED by scripts/gen-cldr-numbers.py from CLDR 48. Do not edit.",
        "",
        "#[derive(Clone, Copy)]",
        "pub(crate) struct Symbols {",
        "    pub decimal: &'static str,",
        "    pub group: &'static str,",
        "    pub percent: &'static str,",
        "    pub plus: &'static str,",
        "    pub minus: &'static str,",
        "    pub approximately: &'static str,",
        "    pub exponential: &'static str,",
        "    pub infinity: &'static str,",
        "    pub nan: &'static str,",
        "    pub primary_group: usize,",
        "    pub secondary_group: usize,",
        "    pub minimum_grouping: usize,",
        "}",
        "",
        "#[derive(Clone, Copy)]",
        "pub(crate) struct Pattern {",
        "    pub positive_prefix: &'static str,",
        "    pub positive_suffix: &'static str,",
        "    pub negative_prefix: &'static str,",
        "    pub negative_suffix: &'static str,",
        "}",
        "",
        "#[derive(Clone, Copy)]",
        "pub(crate) struct Compact {",
        "    pub exponent: i32,",
        "    pub has_number: bool,",
        "    pub prefix: &'static str,",
        "    pub suffix: &'static str,",
        "}",
        "",
        "#[derive(Clone, Copy)]",
        "pub(crate) struct Currency {",
        "    pub name: &'static str,",
        "    pub symbol: &'static str,",
        "    pub narrow: &'static str,",
        "}",
        "",
        "static SYMBOLS: &[(&str, &str, Symbols)] = &[",
    ]
    for row in symbols_rows:
        locale, number_system, *values = row
        strings = ", ".join(rust_string(value) for value in values[:9])
        groups = ", ".join(str(value) for value in values[9:])
        output.append(
            f"    ({rust_string(locale)}, {rust_string(number_system)}, "
            f"Symbols {{ decimal: {rust_string(values[0])}, group: {rust_string(values[1])}, "
            f"percent: {rust_string(values[2])}, plus: {rust_string(values[3])}, "
            f"minus: {rust_string(values[4])}, approximately: {rust_string(values[5])}, "
            f"exponential: {rust_string(values[6])}, infinity: {rust_string(values[7])}, "
            f"nan: {rust_string(values[8])}, primary_group: {values[9]}, "
            f"secondary_group: {values[10]}, minimum_grouping: {values[11]} }}),"
        )
    output.extend(["];", "", "static PATTERNS: &[(&str, &str, &str, Pattern)] = &["])
    for locale, number_system, kind, values in pattern_rows:
        output.append(
            f"    ({rust_string(locale)}, {rust_string(number_system)}, {rust_string(kind)}, "
            "Pattern { "
            f"positive_prefix: {rust_string(values[0])}, positive_suffix: {rust_string(values[1])}, "
            f"negative_prefix: {rust_string(values[2])}, negative_suffix: {rust_string(values[3])} "
            "}),"
        )
    output.extend(
        [
            "];",
            "",
            "static COMPACT: &[(&str, &str, &str, u8, &str, Compact)] = &[",
        ]
    )
    for locale, number_system, display, magnitude, selector, exponent, has_number, pre, post in compact_rows:
        output.append(
            f"    ({rust_string(locale)}, {rust_string(number_system)}, {rust_string(display)}, "
            f"{magnitude}, {rust_string(selector)}, Compact {{ exponent: {exponent}, "
            f"has_number: {str(has_number).lower()}, "
            f"prefix: {rust_string(pre)}, suffix: {rust_string(post)} }}),"
        )
    output.extend(["];", "", "static COMPACT_MAX: &[(&str, &str, &str, u8)] = &["])
    for locale, number_system, display, magnitude in compact_max_rows:
        output.append(
            f"    ({rust_string(locale)}, {rust_string(number_system)}, "
            f"{rust_string(display)}, {magnitude}),"
        )
    output.extend(["];", ""])

    for locale, rows in currency_base.items():
        output.append(
            f"static {ident('C', locale)}: &[(&str, Currency)] = &["
        )
        for code, name, symbol, narrow in rows:
            output.append(
                f"    ({rust_string(code)}, Currency {{ name: {rust_string(name)}, "
                f"symbol: {rust_string(symbol)}, narrow: {rust_string(narrow)} }}),"
            )
        output.extend(["];", ""])
    for locale, rows in currency_names.items():
        output.append(
            f"static {ident('CN', locale)}: &[(&str, &str, &str)] = &["
        )
        for code, category, name in rows:
            output.append(
                f"    ({rust_string(code)}, {rust_string(category)}, {rust_string(name)}),"
            )
        output.extend(["];", ""])

    output.extend(
        [
            "fn data_locale(lang: &str, script: &str, region: &str) -> &'static str {",
            "    match (lang, script, region) {",
            '        ("en", _, "IN") => "en-IN",',
            '        ("pt", _, "PT") => "pt-PT",',
            '        ("zh", "Hant", _) | ("zh", _, "TW" | "HK" | "MO") => "zh-Hant",',
            "        _ => match lang {",
        ]
    )
    for locale in LOCALES:
        if "-" not in locale:
            output.append(f"            {rust_string(locale)} => {rust_string(locale)},")
    output.extend(
        [
            '            _ => "en",',
            "        },",
            "    }",
            "}",
            "",
            "pub(crate) fn locale(lang: &str, script: &str, region: &str) -> &'static str {",
            "    data_locale(lang, script, region)",
            "}",
            "",
            "pub(crate) fn symbols(loc: &str, nu: &str) -> Symbols {",
            "    let find = |locale: &str, system: &str| {",
            "        SYMBOLS.binary_search_by(|row| row.0.cmp(locale).then_with(|| row.1.cmp(system)))",
            "            .ok().map(|index| SYMBOLS[index].2)",
            "    };",
            "    let system_fallback = match nu {",
            '        "arab" | "arabext" => find("ar", "arab"),',
            '        "deva" => find("hi", "deva"),',
            "        _ => None,",
            "    };",
            '    find(loc, nu).or(system_fallback).or_else(|| find(loc, "latn"))',
            '        .or_else(|| find("en", "latn"))',
            "        .expect(\"generated English number symbols\")",
            "}",
            "",
            "pub(crate) fn pattern(loc: &str, nu: &str, kind: &str) -> Pattern {",
            "    let find = |locale: &str, system: &str| {",
            "        PATTERNS.binary_search_by(|row| {",
            "            row.0.cmp(locale).then_with(|| row.1.cmp(system)).then_with(|| row.2.cmp(kind))",
            "        }).ok().map(|index| PATTERNS[index].3)",
            "    };",
            '    find(loc, nu).or_else(|| find(loc, "latn")).or_else(|| find("en", "latn"))',
            "        .expect(\"generated English number pattern\")",
            "}",
            "",
            "fn compact_find(loc: &str, nu: &str, display: &str, magnitude: u8, selector: &str)",
            "    -> Option<Compact> {",
            "    COMPACT.binary_search_by(|row| {",
            "        row.0.cmp(loc).then_with(|| row.1.cmp(nu)).then_with(|| row.2.cmp(display))",
            "            .then_with(|| row.3.cmp(&magnitude)).then_with(|| row.4.cmp(selector))",
            "    }).ok().map(|index| COMPACT[index].5)",
            "}",
            "",
            "fn compact_max(loc: &str, nu: &str, display: &str) -> Option<u8> {",
            "    let find = |system: &str| {",
            "        COMPACT_MAX.binary_search_by(|row| {",
            "            row.0.cmp(loc).then_with(|| row.1.cmp(system)).then_with(|| row.2.cmp(display))",
            "        }).ok().map(|index| COMPACT_MAX[index].3)",
            "    };",
            '    find(nu).or_else(|| find("latn"))',
            "}",
            "",
            "pub(crate) fn compact(",
            "    loc: &str, nu: &str, display: &str, magnitude: u8, category: &str, exact_one: bool,",
            ") -> Option<Compact> {",
            "    // ComputeExponentForMagnitude caps an out-of-range magnitude at the largest",
            "    // available compact pattern before deriving that pattern's exponent.",
            "    let magnitude = magnitude.min(compact_max(loc, nu, display)?);",
            "    let get = |locale: &str, system: &str, selector: &str|",
            "        compact_find(locale, system, display, magnitude, selector);",
            "    if exact_one {",
            "        if let Some(value) = get(loc, nu, \"1\").or_else(|| get(loc, \"latn\", \"1\")) {",
            "            return Some(value);",
            "        }",
            "    }",
            "    for selector in [category, \"other\"] {",
            "        if let Some(value) = get(loc, nu, selector).or_else(|| get(loc, \"latn\", selector)) {",
            "            return Some(value);",
            "        }",
            "    }",
            "    None",
            "}",
            "",
            "fn currency_rows(loc: &str) -> &'static [(&'static str, Currency)] {",
            "    match loc {",
        ]
    )
    for locale in currency_base:
        output.append(f"        {rust_string(locale)} => {ident('C', locale)},")
    output.extend(
        [
            "        _ => C_EN,",
            "    }",
            "}",
            "",
            "/// CLDR currency identifiers for which both NumberFormat and DisplayNames have English fallback",
            "/// data. `C_EN` is generated in canonical code-unit order.",
            "pub(crate) fn currency_codes() -> impl Iterator<Item = &'static str> {",
            "    C_EN.iter().map(|row| row.0)",
            "}",
            "",
            "fn currency_name_rows(loc: &str) -> &'static [(&'static str, &'static str, &'static str)] {",
            "    match loc {",
        ]
    )
    for locale in currency_names:
        output.append(f"        {rust_string(locale)} => {ident('CN', locale)},")
    output.extend(
        [
            "        _ => CN_EN,",
            "    }",
            "}",
            "",
            "pub(crate) fn currency(loc: &str, code: &str, category: &str) -> Option<Currency> {",
            "    let rows = currency_rows(loc);",
            "    let index = rows.binary_search_by_key(&code, |row| row.0).ok()?;",
            "    let mut value = rows[index].1;",
            "    let names = currency_name_rows(loc);",
            "    if let Ok(index) = names.binary_search_by(|row| row.0.cmp(code).then_with(|| row.1.cmp(category))) {",
            "        value.name = names[index].2;",
            "    }",
            "    Some(value)",
            "}",
            "",
            "static CURRENCY_UNITS: &[(&str, &str, &str)] = &[",
        ]
    )
    output.extend(
        f"    ({rust_string(locale)}, {rust_string(category)}, {rust_string(value)}),"
        for locale, category, value in currency_unit_rows
    )
    output.extend(
        [
            "];",
            "",
            "pub(crate) fn currency_unit(loc: &str, category: &str) -> &'static str {",
            "    CURRENCY_UNITS.binary_search_by(|row| row.0.cmp(loc).then_with(|| row.1.cmp(category)))",
            "        .ok().map(|index| CURRENCY_UNITS[index].2)",
            "        .or_else(|| CURRENCY_UNITS.binary_search_by(|row| row.0.cmp(loc).then_with(|| row.1.cmp(\"other\")))",
            "            .ok().map(|index| CURRENCY_UNITS[index].2))",
            '        .unwrap_or("{0} {1}")',
            "}",
            "",
            "pub(crate) fn currency_digits(code: &str) -> u32 {",
            "    match code {",
        ]
    )
    for code, digits in fraction_rows:
        output.append(f"        {rust_string(code)} => {digits},")
    output.extend(
        [
            f"        _ => {default_digits},",
            "    }",
            "}",
            "",
        ]
    )

    generated = "\n".join(output)
    packed_source, string_blob, string_count = pack_generated_source(generated)
    OUTPUT.write_text(packed_source, encoding="utf-8")
    BLOB_OUTPUT.write_bytes(string_blob)
    generator_hash = hashlib.sha256(pathlib.Path(__file__).read_bytes()).hexdigest()
    manifest = {
        "cldr_json_repository": REPOSITORY,
        "cldr_json_tag": TAG,
        "cldr_json_commit": SOURCE_COMMIT,
        "input_url_base": BASE,
        "distinct_input_count": len(INPUT_HASHES),
        "inputs": [
            {"url": url, "sha256": INPUT_HASHES[url]} for url in sorted(INPUT_HASHES)
        ],
        "generator": "scripts/gen-cldr-numbers.py",
        "generator_sha256": generator_hash,
        "original_generated_snapshot": {
            "commit": "1084161475e2e35b50a73eb06fd6885e66de9239",
            "path": "crates/lumen/src/cldr_numbers.rs",
            "sha256": "57064f5dadeeea45a051ed5ef87c45a90af50f8679ac15a5d65deba24badc0a4",
        },
        "packed_source_sha256": hashlib.sha256(packed_source.encode("utf-8")).hexdigest(),
        "string_blob_path": "crates/lumen/src/cldr_numbers.data",
        "string_blob_sha256": hashlib.sha256(string_blob).hexdigest(),
        "string_blob_bytes": len(string_blob),
        "unique_string_count": string_count,
        "symbol_record_count": len(symbols_rows),
        "pattern_record_count": len(pattern_rows),
        "compact_record_count": len(compact_rows),
        "compact_max_record_count": len(compact_max_rows),
        "currency_record_count": sum(map(len, currency_base.values())),
        "currency_name_record_count": sum(map(len, currency_names.values())),
        "currency_unit_record_count": len(currency_unit_rows),
        "text_reference_count": (
            len(symbols_rows) * 11
            + len(pattern_rows) * 7
            + len(compact_rows) * 6
            + len(compact_max_rows) * 3
            + sum(map(len, currency_base.values())) * 4
            + sum(map(len, currency_names.values())) * 3
            + len(currency_unit_rows) * 3
        ),
    }
    MANIFEST_OUTPUT.write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2, sort_keys=True) + "\n",
        encoding="utf-8",
    )
    print(
        f"wrote {OUTPUT.relative_to(ROOT)} ({len(symbols_rows)} symbol sets, "
        f"{len(pattern_rows)} affix patterns, {len(compact_rows)} compact patterns, "
        f"{string_count} interned strings, {len(string_blob)} UTF-8 bytes)"
    )


if __name__ == "__main__":
    main()
