use crate::types::{Mode, ProductConfig};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub fn jitter_minutes(product_id: &str, day: NaiveDate) -> i64 {
    let mut h = DefaultHasher::new();
    product_id.hash(&mut h);
    day.hash(&mut h);
    (h.finish() % 21) as i64 - 10
}

pub fn is_due(now: DateTime<Local>, product_id: &str, cfg: &ProductConfig, last_success_at: Option<i64>) -> bool {
    if cfg.mode == Mode::Off { return false; }
    let mut parts = cfg.time_of_day.split(':');
    let (Some(h), Some(m)) = (parts.next().and_then(|s| s.parse::<u32>().ok()), parts.next().and_then(|s| s.parse::<u32>().ok())) else {
        return false;
    };
    let today = now.date_naive();
    if let Some(ts) = last_success_at {
        if Local.timestamp_opt(ts, 0).single()
            .map(|d| d.date_naive() == today).unwrap_or(false) { return false; }
    }
    let Some(scheduled) = today.and_hms_opt(h, m, 0) else { return false };
    let scheduled = scheduled + chrono::Duration::minutes(jitter_minutes(product_id, today));
    let now_naive = now.naive_local();
    now_naive >= scheduled
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Mode, ProductConfig};
    use chrono::{Local, TimeZone};

    fn cfg(mode: Mode, time: &str) -> ProductConfig {
        ProductConfig { product_id: "workbuddy".into(), mode, time_of_day: time.into(), token: None }
    }
    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn jitter_is_deterministic_in_range() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
        let a = jitter_minutes("workbuddy", day);
        let b = jitter_minutes("workbuddy", day);
        assert_eq!(a, b);
        assert!((-10..=10).contains(&a));
    }

    #[test]
    fn off_mode_never_due() {
        assert!(!is_due(at(2026, 9, 22, 9, 0), "workbuddy", &cfg(Mode::Off, "09:00"), None));
    }

    #[test]
    fn not_due_before_schedule() {
        // jitter 最大 +10：08:40 时任何 jitter 都未到期
        assert!(!is_due(at(2026, 9, 22, 8, 40), "workbuddy", &cfg(Mode::Auto, "09:00"), None));
    }

    #[test]
    fn due_after_schedule_when_never_signed() {
        // jitter 最小 -10：09:15 时任何 jitter 都已到期
        assert!(is_due(at(2026, 9, 22, 9, 15), "workbuddy", &cfg(Mode::Auto, "09:00"), None));
    }

    #[test]
    fn not_due_if_success_today() {
        let today_ts = at(2026, 9, 22, 9, 20).timestamp();
        assert!(!is_due(at(2026, 9, 22, 9, 30), "workbuddy", &cfg(Mode::Auto, "09:00"), Some(today_ts)));
    }

    #[test]
    fn due_again_next_day_even_if_signed_yesterday() {
        let yesterday = at(2026, 9, 21, 9, 20).timestamp();
        assert!(is_due(at(2026, 9, 22, 9, 15), "workbuddy", &cfg(Mode::Remind, "09:00"), Some(yesterday)));
    }
}
