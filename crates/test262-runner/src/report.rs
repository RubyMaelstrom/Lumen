//! Complete conformance evidence, independent of optional console samples.
//! JSON strings follow RFC 8259 sections 7 and 8 (local official RFC snapshot).

use crate::{Outcome, Tally};
use std::collections::BTreeMap;
use std::io::Write;

pub fn write(
    by_cat: &BTreeMap<String, Tally>,
    total: Tally,
    targets: &[String],
    results: &[(String, Outcome)],
) -> std::io::Result<()> {
    let out = render(by_cat, total, targets, results)?;
    std::fs::create_dir_all("test262-report")?;
    let mut f = std::fs::File::create("test262-report/summary.json")?;
    f.write_all(out.as_bytes())?;
    println!("\nwrote test262-report/summary.json (complete failures and skips)");
    Ok(())
}

fn render(
    by_cat: &BTreeMap<String, Tally>,
    total: Tally,
    targets: &[String],
    results: &[(String, Outcome)],
) -> std::io::Result<String> {
    let mut counted = Tally::default();
    for (_, outcome) in results {
        match outcome {
            Outcome::Pass => counted.pass += 1,
            Outcome::Fail(_) => counted.fail += 1,
            Outcome::Skip(_) => counted.skip += 1,
        }
    }
    if (counted.pass, counted.fail, counted.skip) != (total.pass, total.fail, total.skip) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "case outcomes disagree with report totals",
        ));
    }
    let mut out = String::new();
    out.push_str("{\n");
    out.push_str("  \"schema_version\": 2,\n");
    out.push_str(&format!("  \"targets\": [{}],\n", json_str_array(targets)));
    out.push_str(&format!(
        "  \"total\": {{ \"pass\": {}, \"fail\": {}, \"skip\": {} }},\n",
        total.pass, total.fail, total.skip
    ));
    let ran = total.pass + total.fail;
    let pct = if ran > 0 {
        100.0 * total.pass as f64 / ran as f64
    } else {
        0.0
    };
    out.push_str(&format!("  \"pass_rate\": {pct:.6},\n"));
    out.push_str(&format!(
        "  \"all_executed_passed\": {},\n",
        total.pass > 0 && total.fail == 0
    ));
    out.push_str("  \"categories\": {\n");
    let mut first = true;
    for (cat, t) in by_cat {
        if !first {
            out.push_str(",\n");
        }
        first = false;
        out.push_str(&format!(
            "    {}: {{ \"pass\": {}, \"fail\": {}, \"skip\": {} }}",
            json_string(cat),
            t.pass,
            t.fail,
            t.skip
        ));
    }
    out.push_str("\n  },\n");
    for (key, failures) in [("failures", true), ("skips", false)] {
        out.push_str(&format!("  \"{key}\": ["));
        let mut first = true;
        for (path, outcome) in results {
            let reason = match (failures, outcome) {
                (true, Outcome::Fail(reason)) | (false, Outcome::Skip(reason)) => reason,
                _ => continue,
            };
            if !first {
                out.push(',');
            }
            first = false;
            out.push_str(&format!(
                "\n    {{\"path\": {}, \"reason\": {}}}",
                json_string(path),
                json_string(reason)
            ));
        }
        out.push_str(if failures { "\n  ],\n" } else { "\n  ]\n}\n" });
    }
    Ok(out)
}

fn json_str_array(items: &[String]) -> String {
    items
        .iter()
        .map(|s| json_string(s))
        .collect::<Vec<_>>()
        .join(", ")
}

fn json_string(s: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            ch if ch <= '\u{1f}' => {
                write!(out, "\\u{:04x}", ch as u32).unwrap();
            }
            ch => out.push(ch),
        }
    }
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strings_escape_all_json_controls_and_preserve_unicode() {
        for ch in '\0'..='\u{1f}' {
            let value = json_string(&ch.to_string());
            assert!(!value.chars().any(|c| c < ' '));
            assert!(value.starts_with("\"\\") && value.ends_with('"'));
        }
        assert_eq!(
            json_string("\"\\\n\r\t\u{08}\u{0c}\0é🦀"),
            "\"\\\"\\\\\\n\\r\\t\\b\\f\\u0000é🦀\""
        );
    }

    #[test]
    fn complete_failures_and_skips_are_not_console_sample_limited() {
        let mut results: Vec<_> = (0..80)
            .map(|i| {
                (
                    format!("case/{i}.js"),
                    Outcome::Fail(format!("error {i}\nwith details")),
                )
            })
            .collect();
        results.push(("skipped.js".into(), Outcome::Skip("upstream reason".into())));
        results.push(("passed.js".into(), Outcome::Pass));
        let report = render(
            &BTreeMap::new(),
            Tally {
                pass: 1,
                fail: 80,
                skip: 1,
            },
            &[".".into()],
            &results,
        )
        .unwrap();
        assert_eq!(report.matches("\"path\":").count(), 81);
        assert!(report.contains("case/79.js"));
        assert!(report.contains("error 79\\nwith details"));
        assert!(report.contains("\"all_executed_passed\": false"));
        assert!(report.contains("\"skips\": [\n    {\"path\": \"skipped.js\""));
    }

    #[test]
    fn empty_or_inconsistent_reports_cannot_claim_a_clean_run() {
        assert!(render(
            &BTreeMap::new(),
            Tally {
                pass: 1,
                fail: 0,
                skip: 0
            },
            &[],
            &[]
        )
        .is_err());
        let empty = render(&BTreeMap::new(), Tally::default(), &[], &[]).unwrap();
        assert!(empty.contains("\"all_executed_passed\": false"));
    }
}
