#!/usr/bin/env python3
"""Generate Intl.DisplayNames data for every locale Lumen advertises from CLDR 48.

Sources are the pinned unicode-org/cldr-json 48.0.0 localenames, numbers, dates,
and BCP-47 packages.  Long/base names and sparse short/narrow overrides are kept
separate so the generated binary does not repeat the overwhelmingly identical
style fallbacks.
"""

from __future__ import annotations

import json
import pathlib
import urllib.request
from urllib.error import HTTPError


TAG = "48.0.0"
ROOT = pathlib.Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "crates/lumen/src/cldr_display_names.rs"
BASE = (
    f"https://raw.githubusercontent.com/unicode-org/cldr-json/{TAG}/"
    "cldr-json"
)
LOCALES = [
    "en", "de", "fr", "es", "it", "pt", "nl", "ja", "zh", "ko", "ru", "ar",
    "sr", "th", "gv", "sl", "pl", "si", "ln", "sv", "hi",
]
DATE_FIELDS = {
    "era": "era",
    "year": "year",
    "quarter": "quarter",
    "month": "month",
    "weekOfYear": "week",
    "weekday": "weekday",
    "day": "day",
    "dayPeriod": "dayperiod",
    "hour": "hour",
    "minute": "minute",
    "second": "second",
    "timeZoneName": "zone",
}


def download(package: str, locale: str, file: str) -> dict:
    url = f"{BASE}/{package}/main/{locale}/{file}.json"
    request = urllib.request.Request(
        url, headers={"User-Agent": "Lumen CLDR DisplayNames generator"}
    )
    with urllib.request.urlopen(request) as response:
        return json.load(response)["main"][locale]


def rust_string(value: str) -> str:
    return json.dumps(value, ensure_ascii=False)


def split_alternates(
    source: dict[str, str], *, standalone: bool = False, lowercase: bool = False
) -> tuple[dict[str, str], dict[tuple[str, str], str]]:
    base: dict[str, str] = {}
    alternate: dict[tuple[str, str], str] = {}
    standalones: dict[str, str] = {}
    for raw_code, name in source.items():
        if "-alt-" in raw_code:
            code, alt = raw_code.rsplit("-alt-", 1)
            code = code.lower() if lowercase else code
            if alt == "stand-alone":
                standalones[code] = name
            elif alt in ("short", "narrow"):
                alternate[(alt, code)] = name
            continue
        code = raw_code.lower() if lowercase else raw_code
        base[code] = name
    if standalone:
        base.update(standalones)
    return base, alternate


def main() -> None:
    base_rows: list[tuple[str, str, str, str]] = []
    alt_rows: list[tuple[str, str, str, str, str]] = []
    patterns: list[tuple[str, str, str]] = []

    # CLDR uses old LDML calendar keys in display-name data. Translate them to
    # their canonical Unicode BCP-47 spellings and retain the well-formed
    # deprecated alias that ECMA-402 accepts as a distinct input code.
    calendar_json = json.load(
        urllib.request.urlopen(f"{BASE}/cldr-bcp47/bcp47/calendar.json")
    )["keyword"]["u"]["ca"]
    calendar_codes: dict[str, list[str]] = {}
    for code, record in calendar_json.items():
        if code.startswith("_"):
            continue
        source = record.get("_alias", code)
        calendar_codes.setdefault(source, []).append(code)
    calendar_codes.setdefault("islamic-civil", []).append("islamicc")

    for locale in LOCALES:
        localenames: dict[str, dict[str, str]] = {}
        for file, kind, standalone, lowercase in (
            ("languages", "language", False, False),
            ("territories", "region", False, False),
            ("scripts", "script", True, False),
            ("variants", "variant", False, True),
        ):
            try:
                data = download("cldr-localenames-full", locale, file)
            except HTTPError as error:
                # A few locales intentionally inherit an entire sparse category from root.
                # Root's values are codes, so omitting them has the same observable fallback.
                if error.code == 404:
                    continue
                raise
            values = data["localeDisplayNames"][file]
            names, alternates = split_alternates(
                values, standalone=standalone, lowercase=lowercase
            )
            localenames[kind] = names
            base_rows.extend((locale, kind, code, name) for code, name in names.items())
            alt_rows.extend(
                (locale, kind, style, code, name)
                for (style, code), name in alternates.items()
            )

        locale_data = download(
            "cldr-localenames-full", locale, "localeDisplayNames"
        )["localeDisplayNames"]
        locale_pattern = locale_data["localeDisplayPattern"]
        patterns.append(
            (
                locale,
                locale_pattern["localePattern"],
                locale_pattern["localeSeparator"],
            )
        )
        calendars = locale_data.get("types", {}).get("calendar", {})
        for raw_code, name in calendars.items():
            if raw_code == "core" or "-alt-" in raw_code:
                continue
            for code in calendar_codes.get(raw_code, [raw_code]):
                base_rows.append((locale, "calendar", code, name))

        currencies = download("cldr-numbers-full", locale, "currencies")[
            "numbers"
        ]["currencies"]
        for code, record in currencies.items():
            name = record.get("displayName")
            if name is not None:
                base_rows.append((locale, "currency", code, name))

        fields = download("cldr-dates-full", locale, "dateFields")["dates"][
            "fields"
        ]
        for code, cldr_code in DATE_FIELDS.items():
            for style, suffix in (("long", ""), ("short", "-short"), ("narrow", "-narrow")):
                name = fields.get(cldr_code + suffix, {}).get("displayName")
                if name is None:
                    continue
                if style == "long":
                    base_rows.append((locale, "dateTimeField", code, name))
                else:
                    alt_rows.append(
                        (locale, "dateTimeField", style, code, name)
                    )

    # A source row can intentionally be reached by more than one alias. Last
    # writer wins deterministically.
    base_rows = sorted({row[:3]: row for row in base_rows}.values())
    alt_rows = sorted({row[:4]: row for row in alt_rows}.values())
    patterns.sort()

    def ident(prefix: str, *parts: str) -> str:
        text = "_".join((prefix, *parts)).upper()
        return "".join(character if character.isalnum() else "_" for character in text)

    grouped_base: dict[tuple[str, str], list[tuple[str, str]]] = {}
    for locale, kind, code, value in base_rows:
        grouped_base.setdefault((locale, kind), []).append((code, value))
    grouped_alt: dict[tuple[str, str, str], list[tuple[str, str]]] = {}
    for locale, kind, style, code, value in alt_rows:
        grouped_alt.setdefault((locale, kind, style), []).append((code, value))

    output = [
        "//! GENERATED by scripts/gen-cldr-display-names.py from CLDR 48. Do not edit.",
        "//! Locale names use partitioned base records plus sparse style overrides and binary search.",
        "",
        "type Name = (&'static str, &'static str);",
        "",
    ]
    for key, rows in grouped_base.items():
        output.append(f"static {ident('B', *key)}: &[Name] = &[")
        output.extend(
            f"    ({rust_string(code)}, {rust_string(value)})," for code, value in rows
        )
        output.extend(["];", ""])
    for key, rows in grouped_alt.items():
        output.append(f"static {ident('A', *key)}: &[Name] = &[")
        output.extend(
            f"    ({rust_string(code)}, {rust_string(value)})," for code, value in rows
        )
        output.extend(["];", ""])

    output.extend(
        [
            "fn lookup(rows: &[Name], code: &str) -> Option<&'static str> {",
            "    rows.binary_search_by_key(&code, |row| row.0)",
            "        .ok()",
            "        .map(|index| rows[index].1)",
            "}",
            "",
            "pub(crate) fn name(",
            "    lang: &str, kind: &str, style: &str, code: &str,",
            ") -> Option<&'static str> {",
            "    // CLDR style fallback is narrow → short → long.",
            "    for &candidate in match style {",
            '        "narrow" => &["narrow", "short"][..],',
            '        "short" => &["short"][..],',
            "        _ => &[][..],",
            "    } {",
            "        let rows = match (lang, kind, candidate) {",
        ]
    )
    output.extend(
        f"            ({rust_string(locale)}, {rust_string(kind)}, {rust_string(style)}) => {ident('A', locale, kind, style)},"
        for locale, kind, style in grouped_alt
    )
    output.extend(
        [
            "            _ => &[],",
            "        };",
            "        if let Some(value) = lookup(rows, code) {",
            "            return Some(value);",
            "        }",
            "    }",
            "    let rows = match (lang, kind) {",
        ]
    )
    output.extend(
        f"        ({rust_string(locale)}, {rust_string(kind)}) => {ident('B', locale, kind)},"
        for locale, kind in grouped_base
    )
    output.extend(
        [
            "        _ => &[],",
            "    };",
            "    lookup(rows, code)",
            "}",
            "",
            "pub(crate) fn locale_patterns(lang: &str) -> (&'static str, &'static str) {",
            "    match lang {",
        ]
    )
    output.extend(
        f"        {rust_string(locale)} => ({rust_string(pattern)}, {rust_string(separator)}),"
        for locale, pattern, separator in patterns
    )
    output.extend(
        [
            '        _ => ("{0} ({1})", "{0}, {1}"),',
            "    }",
            "}",
            "",
        ]
    )
    OUTPUT.write_text("\n".join(output), encoding="utf-8")
    print(
        f"wrote {OUTPUT.relative_to(ROOT)} "
        f"({len(base_rows)} base names, {len(alt_rows)} style overrides)"
    )


if __name__ == "__main__":
    main()
