//! Formatting a session's duration for display in notifications/toasts, and the date it is
//! logged under. Shared by the Tauri and GTK builds so neither can drift from the other.

/// The local date a session of `hours` that ended at `ended_secs` must have started on: a
/// 90-minute session ending at 01:00 belongs to the day before. Matches the website and the
/// server, which date Quick Sessions the same way.
pub fn session_date(ended_secs: u64, hours: f64) -> String {
    let played = (hours.max(0.0) * 3600.0).round() as u64;
    let start = i64::try_from(ended_secs.saturating_sub(played)).unwrap_or(i64::MAX);
    chrono::DateTime::from_timestamp(start, 0)
        .unwrap_or_else(chrono::Utc::now)
        .with_timezone(&chrono::Local)
        .format("%Y-%m-%d")
        .to_string()
}

/// Formats a session duration for human display: "Xh Ym" once past an hour, otherwise "Xm" --
/// except a session under 30 seconds (rounds to "0m") shows as "<1m" instead, since a real
/// play session and "nothing happened" both existing as "(0m)" made the notification/pending-
/// submission text read like a bug rather than a genuinely brief session (e.g. a UAC-elevated
/// game's pre-elevation stub, or just checking a shop for a few seconds).
pub fn format_session_duration(duration_secs: f64) -> String {
    let total_mins = (duration_secs / 60.0).round() as u64;
    if total_mins >= 60 {
        format!("{}h {}m", total_mins / 60, total_mins % 60)
    } else if total_mins == 0 {
        "<1m".to_string()
    } else {
        format!("{total_mins}m")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sub_minute_shows_as_lt_1m() {
        assert_eq!(format_session_duration(0.0), "<1m");
        assert_eq!(format_session_duration(29.0), "<1m");
    }

    #[test]
    fn rounds_to_whole_minutes() {
        assert_eq!(format_session_duration(30.0), "1m");
        assert_eq!(format_session_duration(89.0), "1m");
        assert_eq!(format_session_duration(90.0), "2m");
    }

    #[test]
    fn session_date_steps_back_a_day_when_the_session_is_longer_than_the_day_so_far() {
        use chrono::TimeZone;
        let one_am = chrono::Local.with_ymd_and_hms(2026, 11, 23, 1, 0, 0).unwrap().timestamp() as u64;
        assert_eq!(session_date(one_am, 1.5), "2026-11-22");
        assert_eq!(session_date(one_am, 1.0), "2026-11-23");
        assert_eq!(session_date(one_am, 0.0), "2026-11-23");
    }

    #[test]
    fn switches_to_hours_at_60_minutes() {
        assert_eq!(format_session_duration(59.0 * 60.0), "59m");
        assert_eq!(format_session_duration(60.0 * 60.0), "1h 0m");
        assert_eq!(format_session_duration(125.0 * 60.0), "2h 5m");
    }
}
