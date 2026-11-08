//! Human-friendly timestamps in the local time zone.

use chrono::{DateTime, Datelike as _, Local, NaiveDate, TimeZone as _};
use mattermost::Millis;

fn local(at: Millis) -> Option<DateTime<Local>> {
    Local.timestamp_millis_opt(at).single()
}

/// The calendar day of a timestamp, for grouping posts under date headers.
pub fn day(at: Millis) -> Option<NaiveDate> {
    local(at).map(|t| t.date_naive())
}

/// `14:05`, the time shown beside each post.
pub fn clock(at: Millis) -> String {
    local(at)
        .map(|t| t.format("%H:%M").to_string())
        .unwrap_or_default()
}

/// `Monday, March 3, 2025 at 14:05`, for a tooltip with the exact moment.
pub fn full(at: Millis) -> String {
    local(at)
        .map(|t| t.format("%A, %B %-d, %Y at %H:%M").to_string())
        .unwrap_or_default()
}

/// `Today`, `Yesterday`, `Monday, March 3`, or `March 3, 2024` for older years.
pub fn day_label(at: Millis) -> String {
    let Some(date) = day(at) else {
        return String::new();
    };
    let today = Local::now().date_naive();
    if date == today {
        "Today".into()
    } else if Some(date) == today.pred_opt() {
        "Yesterday".into()
    } else if date.year() == today.year() {
        date.format("%A, %B %-d").to_string()
    } else {
        date.format("%B %-d, %Y").to_string()
    }
}

/// The current moment as RFC 3339 UTC, for comparing status expiries.
pub fn now_rfc3339() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// When a custom status set now with Mattermost `duration` should expire,
/// as RFC 3339 UTC; `None` for "don't clear".
pub fn status_expiry(duration: &str) -> Option<String> {
    use chrono::{Duration, Local, NaiveTime, Utc};
    let now = Local::now();
    let end_of = |date: chrono::NaiveDate| {
        date.and_time(NaiveTime::from_hms_opt(23, 59, 59)?)
            .and_local_timezone(Local)
            .single()
            .map(|t| t.with_timezone(&Utc))
    };
    let at = match duration {
        "thirty_minutes" => Some((now + Duration::minutes(30)).with_timezone(&Utc)),
        "one_hour" => Some((now + Duration::hours(1)).with_timezone(&Utc)),
        "four_hours" => Some((now + Duration::hours(4)).with_timezone(&Utc)),
        "today" => end_of(now.date_naive()),
        "this_week" => {
            let days_to_sunday = 6 - now.weekday().num_days_from_monday() as i64;
            end_of(now.date_naive() + Duration::days(days_to_sunday))
        }
        _ => None,
    }?;
    Some(at.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}
