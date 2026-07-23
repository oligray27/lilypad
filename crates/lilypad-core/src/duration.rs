//! Formatting a session's duration for display in notifications/toasts. Shared by the Tauri
//! and GTK builds so the sub-minute case can't drift between them.

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
    fn switches_to_hours_at_60_minutes() {
        assert_eq!(format_session_duration(59.0 * 60.0), "59m");
        assert_eq!(format_session_duration(60.0 * 60.0), "1h 0m");
        assert_eq!(format_session_duration(125.0 * 60.0), "2h 5m");
    }
}
