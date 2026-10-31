//! Dates in lists and the preview: relative, Dolphin-style ("5 minutes ago",
//! "Yesterday at 14:32") with the exact time on hover, or exact ("2026-10-30
//! 14:32") when the user turns relative dates off. Times use a 12-hour clock
//! when the user's locale does.

use std::{
    sync::OnceLock,
    time::{Duration, SystemTime},
};

use chrono::{DateTime, Local, TimeDelta};
use egui::{Context, Id, Response, RichText, Ui};

use crate::fs;

fn setting_id() -> Id {
    Id::new("relative-dates")
}

/// On by default. Kept in egui's memory, so it's saved along with the list
/// column choices.
pub fn relative(ctx: &Context) -> bool {
    ctx.data_mut(|d| d.get_persisted(setting_id()))
        .unwrap_or(true)
}

/// The "Relative dates" toggle, for the View menu and the list header menus.
pub fn checkbox(ui: &mut Ui) {
    let mut on = relative(ui.ctx());
    if ui
        .checkbox(&mut on, "Relative dates")
        .on_hover_text("“5 minutes ago”, “Yesterday at 14:32”")
        .changed()
    {
        ui.data_mut(|d| d.insert_persisted(setting_id(), on));
    }
}

/// A date in the style the user chose; `weak` for secondary text.
pub fn label(ui: &mut Ui, time: Option<SystemTime>, weak: bool) -> Response {
    let Some(time) = time else {
        return ui.label("");
    };
    let text = if relative(ui.ctx()) {
        let now = Local::now();
        let time = time.into();
        ui.ctx().request_repaint_after(until_change(time, now));
        format_relative(time, now, clock_format())
    } else {
        fs::format_time(Some(time))
    };
    let text = if weak {
        RichText::new(text).weak()
    } else {
        RichText::new(text)
    };
    let response = ui.label(text);
    if relative(ui.ctx()) {
        response.on_hover_text(fs::format_time(Some(time)))
    } else {
        response
    }
}

/// How long until [`format_relative`] gives a different text: when the age
/// passes the next whole minute while it's under an hour, else at midnight.
fn until_change(time: DateTime<Local>, now: DateTime<Local>) -> Duration {
    let age = (now - time).num_milliseconds();
    if (0..3_600_000).contains(&age) {
        return Duration::from_millis((60_000 - age % 60_000) as u64);
    }
    let midnight = (now.date_naive() + TimeDelta::days(1))
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(Local).earliest());
    midnight
        .and_then(|m| (m - now).to_std().ok())
        .unwrap_or(Duration::from_secs(60))
}

/// The exact date and time.
pub fn format_exact(time: DateTime<Local>, clock: &str) -> String {
    time.format(&format!("%Y-%m-%d {clock}")).to_string()
}

/// A chrono format for the time of day: "2:32 PM" or "14:32", by the locale.
pub fn clock_format() -> &'static str {
    static FORMAT: OnceLock<&str> = OnceLock::new();
    FORMAT.get_or_init(|| {
        if locale_uses_12_hour_clock() {
            "%-I:%M %p"
        } else {
            "%H:%M"
        }
    })
}

/// Asks the C library for the LC_TIME locale's time format, in a locale
/// object of its own: setting the process locale would upset libmpv, which
/// needs LC_NUMERIC to stay "C".
#[cfg(target_os = "linux")]
fn locale_uses_12_hour_clock() -> bool {
    // SAFETY: the locale is checked for null and freed after the format,
    // which it owns, is copied out.
    unsafe {
        let locale = libc::newlocale(libc::LC_TIME_MASK, c"".as_ptr(), std::ptr::null_mut());
        if locale.is_null() {
            return false;
        }
        let format = libc::nl_langinfo_l(libc::T_FMT, locale);
        let format = (!format.is_null()).then(|| {
            std::ffi::CStr::from_ptr(format)
                .to_string_lossy()
                .into_owned()
        });
        libc::freelocale(locale);
        format.is_some_and(|f| strftime_is_12_hour(&f))
    }
}

/// The user's "Short time" format from Region settings, e.g. "h:mm tt".
#[cfg(windows)]
fn locale_uses_12_hour_clock() -> bool {
    use windows::Win32::Globalization::{GetLocaleInfoEx, LOCALE_STIMEFORMAT};
    use windows::core::PCWSTR;
    let mut buffer = [0u16; 80];
    // SAFETY: a null name is the user's default locale; the length is the buffer's.
    let len = unsafe { GetLocaleInfoEx(PCWSTR::null(), LOCALE_STIMEFORMAT, Some(&mut buffer)) };
    let len = usize::try_from(len).unwrap_or(0).saturating_sub(1);
    windows_pattern_is_12_hour(&String::from_utf16_lossy(&buffer[..len]))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn locale_uses_12_hour_clock() -> bool {
    false
}

/// glibc gives "%r" or "%I:%M:%S %p" for 12-hour locales and "%T" or
/// "%H:%M:%S" for 24-hour ones.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn strftime_is_12_hour(format: &str) -> bool {
    ["%r", "%I", "%l", "%p"].iter().any(|f| format.contains(f))
}

/// Windows patterns use "h" for 12-hour, "H" for 24-hour and "tt" for AM/PM;
/// text in single quotes is literal.
#[cfg_attr(not(windows), allow(dead_code))]
fn windows_pattern_is_12_hour(pattern: &str) -> bool {
    pattern
        .split('\'')
        .step_by(2)
        .any(|unquoted| unquoted.contains(['h', 't']))
}

fn format_relative(time: DateTime<Local>, now: DateTime<Local>, clock: &str) -> String {
    let age = now - time;
    // A little ahead is clock skew between machines; further is just odd.
    if age < TimeDelta::seconds(-60) {
        return format_exact(time, clock);
    }
    if age < TimeDelta::minutes(1) {
        return "Just now".into();
    }
    if age < TimeDelta::hours(1) {
        return match age.num_minutes() {
            1 => "1 minute ago".into(),
            minutes => format!("{minutes} minutes ago"),
        };
    }
    let time_of_day = time.format(clock);
    let days = (now.date_naive() - time.date_naive()).num_days();
    match days {
        0 => format!("Today at {time_of_day}"),
        1 => format!("Yesterday at {time_of_day}"),
        2..=6 => format!("{} at {time_of_day}", time.format("%A")),
        _ => format_exact(time, clock),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn at(day: u32, hour: u32, minute: u32, second: u32) -> DateTime<Local> {
        Local
            .with_ymd_and_hms(2026, 10, day, hour, minute, second)
            .unwrap()
    }

    #[test]
    fn relative_dates() {
        // Friday 30 October 2026, 14:32:30.
        let now = at(30, 14, 32, 30);
        let cases = [
            (at(30, 14, 32, 0), "Just now"),
            (at(30, 14, 33, 0), "Just now"),
            (at(30, 14, 31, 0), "1 minute ago"),
            (at(30, 14, 27, 10), "5 minutes ago"),
            (at(30, 13, 33, 0), "59 minutes ago"),
            (at(30, 13, 32, 0), "Today at 13:32"),
            (at(30, 0, 5, 0), "Today at 00:05"),
            (at(29, 23, 59, 0), "Yesterday at 23:59"),
            (at(28, 9, 0, 0), "Wednesday at 09:00"),
            (at(24, 9, 0, 0), "Saturday at 09:00"),
            (at(23, 9, 0, 0), "2026-10-23 09:00"),
            (at(31, 9, 0, 0), "2026-10-31 09:00"),
        ];
        for (time, expected) in cases {
            assert_eq!(format_relative(time, now, "%H:%M"), expected, "{time}");
        }
    }

    #[test]
    fn twelve_hour_clock() {
        let now = at(30, 14, 32, 30);
        let clock = "%-I:%M %p";
        assert_eq!(
            format_relative(at(30, 13, 5, 0), now, clock),
            "Today at 1:05 PM"
        );
        assert_eq!(
            format_relative(at(29, 0, 5, 0), now, clock),
            "Yesterday at 12:05 AM"
        );
        assert_eq!(
            format_relative(at(1, 9, 0, 0), now, clock),
            "2026-10-01 9:00 AM"
        );
    }

    #[test]
    fn locale_time_formats() {
        assert!(strftime_is_12_hour("%r"));
        assert!(strftime_is_12_hour("%I:%M:%S %p"));
        assert!(!strftime_is_12_hour("%T"));
        assert!(!strftime_is_12_hour("%H:%M:%S"));
        assert!(windows_pattern_is_12_hour("h:mm:ss tt"));
        assert!(windows_pattern_is_12_hour("tt hh:mm"));
        assert!(!windows_pattern_is_12_hour("HH:mm:ss"));
        assert!(!windows_pattern_is_12_hour("HH'h'mm"));
    }

    #[test]
    fn repaints_when_the_text_changes() {
        let now = at(30, 14, 32, 30);
        // 5 minutes 20 seconds old: "6 minutes ago" in 40 seconds.
        assert_eq!(
            until_change(at(30, 14, 27, 10), now),
            Duration::from_secs(40)
        );
        // Older than an hour: "Today" becomes "Yesterday" at midnight.
        let midnight = Duration::from_secs(9 * 3600 + 27 * 60 + 30);
        assert_eq!(until_change(at(30, 9, 0, 0), now), midnight);
    }
}
