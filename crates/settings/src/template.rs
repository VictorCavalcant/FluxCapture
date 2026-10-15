//! File name templates.
//!
//! Placeholders: `{date}` (2026-10-13), `{time}` (14-05-09), `{year}` `{month}` `{day}` `{hour}`
//! `{minute}` `{second}`, `{profile}`, `{scene}`, `{counter}`. The result is a valid Windows file
//! name (no reserved characters or device names, bounded length).

/// Local date and time pieces.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Stamp {
    pub year: i32,
    pub month: u32,
    pub day: u32,
    pub hour: u32,
    pub minute: u32,
    pub second: u32,
}

impl Stamp {
    /// From seconds since the Unix epoch plus a UTC offset in seconds (civil-from-days algorithm).
    pub fn from_unix(secs: i64, utc_offset: i64) -> Stamp {
        let t = secs.saturating_add(utc_offset);
        let days = t.div_euclid(86_400);
        let rem = t.rem_euclid(86_400);
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        Stamp {
            year: (if m <= 2 { y + 1 } else { y }) as i32,
            month: m as u32,
            day: d as u32,
            hour: (rem / 3600) as u32,
            minute: ((rem / 60) % 60) as u32,
            second: (rem % 60) as u32,
        }
    }
}

/// Expands `template`.
pub fn expand(template: &str, stamp: Stamp, profile: &str, scene: &str, counter: u32) -> String {
    let s = template
        .replace("{date}", &format!("{:04}-{:02}-{:02}", stamp.year, stamp.month, stamp.day))
        .replace("{time}", &format!("{:02}-{:02}-{:02}", stamp.hour, stamp.minute, stamp.second))
        .replace("{year}", &format!("{:04}", stamp.year))
        .replace("{month}", &format!("{:02}", stamp.month))
        .replace("{day}", &format!("{:02}", stamp.day))
        .replace("{hour}", &format!("{:02}", stamp.hour))
        .replace("{minute}", &format!("{:02}", stamp.minute))
        .replace("{second}", &format!("{:02}", stamp.second))
        .replace("{profile}", profile)
        .replace("{scene}", scene)
        .replace("{counter}", &format!("{counter:03}"));
    sanitize_file_name(&s)
}

/// A valid Windows file name (without extension).
pub fn sanitize_file_name(s: &str) -> String {
    let mut out: String = s.chars().map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c }).take(150).collect();
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }
    let out = out.trim_start().to_string();
    let stem = out.split('.').next().unwrap_or("").to_ascii_uppercase();
    let reserved = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6",
        "LPT7", "LPT8", "LPT9",
    ];
    if out.is_empty() {
        "FluxCapture".into()
    } else if reserved.contains(&stem.as_str()) {
        format!("_{out}")
    } else {
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_dates() {
        assert_eq!(Stamp::from_unix(0, 0), Stamp { year: 1970, month: 1, day: 1, hour: 0, minute: 0, second: 0 });
        // 2026-10-13 14:05:09 UTC = 1791900309
        assert_eq!(Stamp::from_unix(1_791_900_309, 0), Stamp { year: 2026, month: 10, day: 13, hour: 14, minute: 5, second: 9 });
        assert_eq!(Stamp::from_unix(1_791_900_309, -3 * 3600).hour, 11);
        let _ = Stamp::from_unix(i64::MIN, i64::MIN);
    }

    #[test]
    fn templates_make_valid_names() {
        let st = Stamp { year: 2026, month: 1, day: 2, hour: 3, minute: 4, second: 5 };
        assert_eq!(expand("FluxCapture {date} {time}", st, "Gaming", "Cena 1", 1), "FluxCapture 2026-01-02 03-04-05");
        assert_eq!(expand("{scene}/{profile}:{counter}", st, "a?b", "c", 7), "c_a_b_007");
        assert_eq!(sanitize_file_name("CON"), "_CON");
        assert_eq!(sanitize_file_name("nul.txt"), "_nul.txt");
        assert_eq!(sanitize_file_name("  ..."), "FluxCapture");
        assert_eq!(sanitize_file_name(&"x".repeat(1000)).len(), 150);
    }
}
