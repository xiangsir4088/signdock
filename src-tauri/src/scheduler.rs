use crate::types::{Mode, ProductConfig};
use chrono::{DateTime, Local, NaiveDate, TimeZone};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// 抖动只取正向：Qoder 的领取窗口 10:00 才开，出厂默认 10:05，负抖动会把触发时间拉回
/// 窗口外 → 命中的是昨日 campaign（已 CLAIMED）→ 记 alreadySigned → 当天再也不会重试。
pub fn jitter_minutes(product_id: &str, day: NaiveDate) -> i64 {
    let mut h = DefaultHasher::new();
    product_id.hash(&mut h);
    day.hash(&mut h);
    (h.finish() % 11) as i64
}

/// 当日防重触发：只要今天已有任何一次执行记录（last_handled_at 为任意 outcome 的最近运行时间），今日不再到期。
pub fn is_due(now: DateTime<Local>, product_id: &str, cfg: &ProductConfig, last_handled_at: Option<i64>) -> bool {
    if cfg.mode == Mode::Off { return false; }
    let mut parts = cfg.time_of_day.split(':');
    let (Some(h), Some(m)) = (parts.next().and_then(|s| s.parse::<u32>().ok()), parts.next().and_then(|s| s.parse::<u32>().ok())) else {
        return false;
    };
    let today = now.date_naive();
    if let Some(ts) = last_handled_at {
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
        // 只允许正向：负抖动会让"窗口开启后 5 分钟"这类贴着下界的默认时间落回窗口外
        assert!((0..=10).contains(&a), "抖动应为非负，实得 {a}");
    }

    /// 回归：qoder 默认 10:05，任何一天的抖动都不该让它在 10:00 之前触发
    #[test]
    fn qoder_default_time_never_fires_before_window_opens() {
        let cfg = ProductConfig {
            product_id: "qoder".into(), mode: Mode::Auto, time_of_day: "10:05".into(), token: None,
        };
        for d in 0..30 {
            let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 1).unwrap() + chrono::Duration::days(d);
            let at_1000 = day.and_hms_opt(10, 0, 0).unwrap().and_local_timezone(Local).unwrap();
            assert!(!is_due(at_1000, "qoder", &cfg, None), "{day} 10:00 不该到期");
        }
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
        // 当日已有成功运行 → 不再到期（last_handled_at 取任意 outcome 最近一次）
        let today_ts = at(2026, 9, 22, 9, 20).timestamp();
        assert!(!is_due(at(2026, 9, 22, 9, 30), "workbuddy", &cfg(Mode::Auto, "09:00"), Some(today_ts)));
    }

    #[test]
    fn not_due_if_reminded_today() {
        // 回归：非 success（如 reminded）的当日运行同样阻断重复触发
        let today_ts = at(2026, 9, 22, 9, 10).timestamp();
        assert!(!is_due(at(2026, 9, 22, 23, 0), "workbuddy", &cfg(Mode::Remind, "09:00"), Some(today_ts)));
    }

    #[test]
    fn due_again_next_day_even_if_signed_yesterday() {
        let yesterday = at(2026, 9, 21, 9, 20).timestamp();
        assert!(is_due(at(2026, 9, 22, 9, 15), "workbuddy", &cfg(Mode::Remind, "09:00"), Some(yesterday)));
    }
}
