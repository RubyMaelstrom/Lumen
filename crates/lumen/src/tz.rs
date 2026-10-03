//! IANA time-zone lookups over the generated [`crate::tzdata`] offset tables.

use crate::tzdata::{Zone, LINKS, ZONES};

/// The canonical registry name for a case-insensitive IANA zone id (`None` if unknown).
pub fn canonicalize(name: &str) -> Option<&'static str> {
    // The UTC aliases canonicalize to "UTC"; the GMT/Greenwich family stays as "Etc/GMT" (resolved
    // below through the link table), and Etc/GMT+N remain distinct fixed-offset zones.
    let lc = name.to_ascii_lowercase();
    if matches!(
        lc.as_str(),
        "utc"
            | "etc/utc"
            | "etc/uct"
            | "uct"
            | "universal"
            | "etc/universal"
            | "zulu"
            | "etc/zulu"
            | "gmt"
            | "etc/gmt"
            | "etc/gmt0"
            | "gmt0"
            | "gmt+0"
            | "gmt-0"
            | "etc/gmt+0"
            | "etc/gmt-0"
            | "etc/greenwich"
            | "greenwich"
    ) {
        return Some("UTC");
    }
    if let Some(z) = ZONES.iter().find(|z| z.name.eq_ignore_ascii_case(name)) {
        return Some(z.name);
    }
    LINKS
        .iter()
        .find(|(a, _)| a.eq_ignore_ascii_case(name))
        .map(|(_, canon)| *canon)
}

/// The registry name for a case-insensitive zone id, PRESERVING aliases (an input of "Asia/Calcutta"
/// stays "Asia/Calcutta", not the canonical "Asia/Kolkata"). Temporal keeps the identifier as given;
/// only `equals`/`compare` canonicalize. `None` if the id is unknown.
pub fn registry_name(name: &str) -> Option<&'static str> {
    if name.eq_ignore_ascii_case("UTC") {
        return Some("UTC");
    }
    if let Some(z) = ZONES.iter().find(|z| z.name.eq_ignore_ascii_case(name)) {
        return Some(z.name);
    }
    LINKS
        .iter()
        .find(|(a, _)| a.eq_ignore_ascii_case(name))
        .map(|(a, _)| *a)
}

/// Every canonical IANA zone name in the registry (for `Intl.supportedValuesOf("timeZone")`).
#[cfg(feature = "intl")]
pub fn canonical_zone_names() -> Vec<&'static str> {
    ZONES.iter().map(|z| z.name).collect()
}

fn zone(name: &str) -> Option<&'static Zone> {
    let canon = canonicalize(name)?;
    ZONES.iter().find(|z| z.name == canon)
}

/// The UTC offset (seconds) in effect at `epoch_sec` for a named zone.
pub fn offset_at(name: &str, epoch_sec: i64) -> Option<i32> {
    let z = zone(name)?;
    let idx = z.transitions.partition_point(|&(t, _)| t <= epoch_sec);
    Some(if idx == 0 {
        z.initial
    } else {
        z.transitions[idx - 1].1
    })
}

/// The epoch-second of the next (`forward`) or previous offset transition strictly after/before
/// `epoch_sec`, or `None` when the zone has no further transition in that direction.
pub fn next_transition(name: &str, epoch_sec: i64, forward: bool) -> Option<i64> {
    let z = zone(name)?;
    let ts = z.transitions;
    if forward {
        ts.iter().find(|&&(t, _)| t > epoch_sec).map(|&(t, _)| t)
    } else {
        ts.iter()
            .rev()
            .find(|&&(t, _)| t < epoch_sec)
            .map(|&(t, _)| t)
    }
}

/// The CLDR English long metazone name (`timeZoneName: "long"`, and the name
/// `Date.prototype.toString` appends) for a named zone at `epoch_sec`, for the
/// zones Lumen has names for. Daylight time is an offset above the zone's
/// standard one, the smaller of its January and July offsets that year.
pub fn long_name(name: &str, epoch_sec: i64) -> Option<&'static str> {
    const CENTRAL_EUROPE: (&str, &str) = (
        "Central European Standard Time",
        "Central European Summer Time",
    );
    const EASTERN_EUROPE: (&str, &str) = (
        "Eastern European Standard Time",
        "Eastern European Summer Time",
    );
    const WESTERN_EUROPE: (&str, &str) = (
        "Western European Standard Time",
        "Western European Summer Time",
    );
    const US_EASTERN: (&str, &str) = ("Eastern Standard Time", "Eastern Daylight Time");
    const US_CENTRAL: (&str, &str) = ("Central Standard Time", "Central Daylight Time");
    const US_MOUNTAIN: (&str, &str) = ("Mountain Standard Time", "Mountain Daylight Time");
    const US_PACIFIC: (&str, &str) = ("Pacific Standard Time", "Pacific Daylight Time");
    const ATLANTIC: (&str, &str) = ("Atlantic Standard Time", "Atlantic Daylight Time");
    const AUSTRALIA_EASTERN: (&str, &str) = (
        "Australian Eastern Standard Time",
        "Australian Eastern Daylight Time",
    );
    const AUSTRALIA_CENTRAL: (&str, &str) = (
        "Australian Central Standard Time",
        "Australian Central Daylight Time",
    );
    let canon = canonicalize(name)?;
    let (standard, daylight) = match canon {
        "UTC" => return Some("Coordinated Universal Time"),
        "Europe/Berlin"
        | "Europe/Vienna"
        | "Europe/Paris"
        | "Europe/Rome"
        | "Europe/Madrid"
        | "Europe/Amsterdam"
        | "Europe/Brussels"
        | "Europe/Prague"
        | "Europe/Warsaw"
        | "Europe/Budapest"
        | "Europe/Stockholm"
        | "Europe/Oslo"
        | "Europe/Copenhagen"
        | "Europe/Zurich"
        | "Europe/Belgrade"
        | "Europe/Bratislava"
        | "Europe/Ljubljana"
        | "Europe/Zagreb"
        | "Europe/Luxembourg"
        | "Europe/Monaco"
        | "Europe/Malta"
        | "Europe/Andorra"
        | "Europe/San_Marino"
        | "Europe/Vatican"
        | "Europe/Gibraltar"
        | "Europe/Tirane"
        | "Europe/Sarajevo"
        | "Europe/Skopje"
        | "Europe/Podgorica"
        | "Europe/Busingen"
        | "Europe/Vaduz"
        | "Arctic/Longyearbyen"
        | "Africa/Ceuta" => CENTRAL_EUROPE,
        "Europe/Helsinki" | "Europe/Athens" | "Europe/Bucharest" | "Europe/Sofia"
        | "Europe/Kyiv" | "Europe/Kiev" | "Europe/Riga" | "Europe/Tallinn" | "Europe/Vilnius"
        | "Europe/Chisinau" | "Europe/Mariehamn" | "Asia/Nicosia" | "Asia/Famagusta"
        | "Asia/Beirut" | "Africa/Cairo" => EASTERN_EUROPE,
        "Europe/Lisbon" | "Atlantic/Canary" | "Atlantic/Madeira" | "Atlantic/Faroe" => {
            WESTERN_EUROPE
        }
        "Europe/London" | "Europe/Guernsey" | "Europe/Jersey" | "Europe/Isle_of_Man" => {
            ("Greenwich Mean Time", "British Summer Time")
        }
        "Europe/Dublin" => ("Greenwich Mean Time", "Irish Standard Time"),
        "Europe/Moscow" | "Europe/Simferopol" => ("Moscow Standard Time", "Moscow Summer Time"),
        "America/New_York"
        | "America/Detroit"
        | "America/Toronto"
        | "America/Nassau"
        | "America/Indiana/Indianapolis"
        | "America/Kentucky/Louisville" => US_EASTERN,
        "America/Chicago"
        | "America/Winnipeg"
        | "America/Mexico_City"
        | "America/Monterrey"
        | "America/Indiana/Knox"
        | "America/Menominee" => US_CENTRAL,
        "America/Denver" | "America/Edmonton" | "America/Boise" | "America/Phoenix" => US_MOUNTAIN,
        "America/Los_Angeles" | "America/Vancouver" | "America/Tijuana" => US_PACIFIC,
        "America/Anchorage" | "America/Juneau" => ("Alaska Standard Time", "Alaska Daylight Time"),
        "Pacific/Honolulu" => (
            "Hawaii-Aleutian Standard Time",
            "Hawaii-Aleutian Daylight Time",
        ),
        "America/Halifax" | "America/Puerto_Rico" | "Atlantic/Bermuda" => ATLANTIC,
        "America/St_Johns" => ("Newfoundland Standard Time", "Newfoundland Daylight Time"),
        "America/Sao_Paulo" => ("Brasilia Standard Time", "Brasilia Summer Time"),
        "America/Argentina/Buenos_Aires" | "America/Buenos_Aires" => {
            ("Argentina Standard Time", "Argentina Summer Time")
        }
        "America/Bogota" => ("Colombia Standard Time", "Colombia Summer Time"),
        "America/Lima" => ("Peru Standard Time", "Peru Summer Time"),
        "America/Santiago" => ("Chile Standard Time", "Chile Summer Time"),
        "Asia/Tokyo" => ("Japan Standard Time", "Japan Daylight Time"),
        "Asia/Shanghai" => ("China Standard Time", "China Daylight Time"),
        "Asia/Hong_Kong" => ("Hong Kong Standard Time", "Hong Kong Summer Time"),
        "Asia/Taipei" => ("Taipei Standard Time", "Taipei Daylight Time"),
        "Asia/Seoul" => ("Korean Standard Time", "Korean Daylight Time"),
        "Asia/Kolkata" | "Asia/Calcutta" => ("India Standard Time", "India Standard Time"),
        "Asia/Singapore" => ("Singapore Standard Time", "Singapore Standard Time"),
        "Asia/Bangkok" | "Asia/Ho_Chi_Minh" => ("Indochina Time", "Indochina Time"),
        "Asia/Jakarta" => ("Western Indonesia Time", "Western Indonesia Time"),
        "Asia/Manila" => ("Philippine Standard Time", "Philippine Summer Time"),
        "Asia/Dubai" => ("Gulf Standard Time", "Gulf Standard Time"),
        "Asia/Karachi" => ("Pakistan Standard Time", "Pakistan Summer Time"),
        "Asia/Dhaka" => ("Bangladesh Standard Time", "Bangladesh Summer Time"),
        "Asia/Tehran" => ("Iran Standard Time", "Iran Daylight Time"),
        "Asia/Jerusalem" => ("Israel Standard Time", "Israel Daylight Time"),
        "Asia/Riyadh" => ("Arabian Standard Time", "Arabian Daylight Time"),
        "Australia/Sydney" | "Australia/Melbourne" | "Australia/Hobart" | "Australia/Brisbane" => {
            AUSTRALIA_EASTERN
        }
        "Australia/Adelaide" | "Australia/Darwin" => AUSTRALIA_CENTRAL,
        "Australia/Perth" => (
            "Australian Western Standard Time",
            "Australian Western Daylight Time",
        ),
        "Pacific/Auckland" => ("New Zealand Standard Time", "New Zealand Daylight Time"),
        "Africa/Johannesburg" => ("South Africa Standard Time", "South Africa Standard Time"),
        "Africa/Lagos" => ("West Africa Standard Time", "West Africa Summer Time"),
        "Africa/Nairobi" => ("East Africa Time", "East Africa Time"),
        _ => return None,
    };
    // Days from 1970-01-01 to January 15 and July 15 of epoch_sec's year.
    let year = 1970 + epoch_sec.div_euclid(31_556_952);
    let jan = days_from_epoch(year, 1, 15) * 86_400;
    let jul = days_from_epoch(year, 7, 15) * 86_400;
    let standard_offset = offset_at(canon, jan)?.min(offset_at(canon, jul)?);
    Some(if offset_at(canon, epoch_sec)? > standard_offset {
        daylight
    } else {
        standard
    })
}

fn days_from_epoch(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}
