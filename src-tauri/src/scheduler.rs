use crate::types::{Mode, ProductConfig, TodaySummary, OUTCOME_RETRYABLE, OUTCOME_WINDOW_PENDING};
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

/// 今日零点的时间戳，供 store.today_summary 划界。
pub fn day_start(now: DateTime<Local>) -> i64 {
    now.date_naive().and_hms_opt(0, 0, 0)
        .and_then(|t| Local.from_local_datetime(&t).earliest())
        .map(|d| d.timestamp())
        .unwrap_or(0)
}

/// 是否到期。**临时失败与「今日窗口未开」都不占用本次 tick**：前者在补偿额度与间隔都满足时
/// 再试一次，后者只等间隔（它不是失败，不该吃补偿额度）。其余任何当日结果（成功/已签/已提醒/
/// 需人工/终态失败）都视为今天已处理完。
/// 调度器因此不需要在循环里 sleep —— 下一轮 tick（60s）自然会再看一眼。
pub fn is_due(now: DateTime<Local>, product_id: &str, cfg: &ProductConfig, today: &TodaySummary) -> bool {
    if cfg.mode == Mode::Off { return false; }
    let mut parts = cfg.time_of_day.split(':');
    let (Some(h), Some(m)) = (parts.next().and_then(|s| s.parse::<u32>().ok()), parts.next().and_then(|s| s.parse::<u32>().ok())) else {
        return false;
    };
    let day = now.date_naive();
    let Some(scheduled) = day.and_hms_opt(h, m, 0) else { return false };
    let scheduled = scheduled + chrono::Duration::minutes(jitter_minutes(product_id, day));
    if now.naive_local() < scheduled { return false; }

    let (Some(last_outcome), Some(last_at)) = (&today.last_outcome, today.last_at) else {
        return true;   // 今天还没跑过
    };
    if last_outcome == OUTCOME_WINDOW_PENDING {
        // 「今日窗口未开」只等间隔，不看补偿额度：它一次都不是失败
        return now.timestamp() >= last_at + cfg.retry_interval_min as i64 * 60;
    }
    if last_outcome != OUTCOME_RETRYABLE { return false; }
    // last_outcome == retryable：today.retry_failures 是已经吃掉的失败次数，
    // 首次执行不算补偿，所以额度判断用 <=（retry_times=2 → 最多跑 3 次）。
    today.retry_failures <= cfg.retry_times
        && now.timestamp() >= last_at + cfg.retry_interval_min as i64 * 60
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Mode;
    use chrono::TimeZone;

    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }
    fn cfg(mode: Mode, time: &str) -> ProductConfig {
        ProductConfig::base("workbuddy", mode, time)
    }
    fn cfg_retry(times: u32, interval: u32) -> ProductConfig {
        ProductConfig { retry_times: times, retry_interval_min: interval, ..cfg(Mode::Auto, "09:00") }
    }
    fn summary(ts: i64, outcome: &str, fails: u32) -> TodaySummary {
        TodaySummary { last_at: Some(ts), last_outcome: Some(outcome.into()), retry_failures: fails }
    }
    /// retryable 失败摘要：09:20 跑过一次并失败
    fn failed_at(h: u32, mi: u32, fails: u32) -> TodaySummary {
        summary(at(2026, 9, 22, h, mi).timestamp(), OUTCOME_RETRYABLE, fails)
    }

    #[test]
    fn jitter_is_deterministic_in_range() {
        let day = NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
        let a = jitter_minutes("workbuddy", day);
        let b = jitter_minutes("workbuddy", day);
        assert_eq!(a, b);
        // 只允许正向：负抖动会让"窗口开启后 5 分钟"这类贴着下界的默认时间落回窗口外
        assert!((0..=10).contains(&a), "抖动应为非负，实得 {a}");
    }

    /// 回归：qoder 默认 10:05，任何一天的抖动都不该让它在 10:00 之前触发
    #[test]
    fn qoder_default_time_never_fires_before_window_opens() {
        let cfg = ProductConfig::base("qoder", Mode::Auto, "10:05");
        for d in 0..30 {
            let day = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap() + chrono::Duration::days(d);
            let at_1000 = day.and_hms_opt(10, 0, 0).unwrap().and_local_timezone(Local).unwrap();
            assert!(!is_due(at_1000, "qoder", &cfg, &TodaySummary::default()), "{day} 10:00 不该到期");
        }
    }

    #[test]
    fn off_mode_never_due() {
        assert!(!is_due(at(2026, 9, 22, 9, 0), "workbuddy", &cfg(Mode::Off, "09:00"), &TodaySummary::default()));
    }

    #[test]
    fn not_due_before_schedule() {
        // jitter 最大 +10：08:40 时任何 jitter 都未到期
        assert!(!is_due(at(2026, 9, 22, 8, 40), "workbuddy", &cfg(Mode::Auto, "09:00"), &TodaySummary::default()));
    }

    #[test]
    fn due_after_schedule_when_never_signed() {
        // jitter 最小 0：09:15 时任何 jitter 都已到期
        assert!(is_due(at(2026, 9, 22, 9, 15), "workbuddy", &cfg(Mode::Auto, "09:00"), &TodaySummary::default()));
    }

    #[test]
    fn not_due_if_success_today() {
        let s = summary(at(2026, 9, 22, 9, 20).timestamp(), "success", 0);
        assert!(!is_due(at(2026, 9, 22, 9, 30), "workbuddy", &cfg(Mode::Auto, "09:00"), &s));
    }

    #[test]
    fn not_due_if_reminded_today() {
        // 提醒发过一次就够，不该全天反复提醒
        let s = summary(at(2026, 9, 22, 9, 10).timestamp(), "reminded", 0);
        assert!(!is_due(at(2026, 9, 22, 23, 0), "workbuddy", &cfg(Mode::Remind, "09:00"), &s));
    }

    /// 终态失败（凭证过期等）重试无意义：今天到此为止，等用户处理
    #[test]
    fn not_due_if_permanent_failed_today() {
        let s = summary(at(2026, 9, 22, 9, 10).timestamp(), "failed", 0);
        assert!(!is_due(at(2026, 9, 22, 9, 30), "workbuddy", &cfg(Mode::Auto, "09:00"), &s));
    }

    #[test]
    fn retryable_failure_gets_another_chance_after_interval() {
        let s = failed_at(9, 20, 1);
        assert!(is_due(at(2026, 9, 22, 9, 25), "workbuddy", &cfg_retry(2, 5), &s), "间隔到了就该补试");
    }

    #[test]
    fn not_due_before_retry_interval_elapses() {
        let s = failed_at(9, 20, 1);
        assert!(!is_due(at(2026, 9, 22, 9, 22), "workbuddy", &cfg_retry(2, 5), &s), "4 分钟不到 5 分钟间隔");
    }

    #[test]
    fn not_due_once_retry_budget_is_spent() {
        // retry_times=2 → 首跑 + 2 次补偿 = 3 次执行；已失败 3 次即额度用尽
        let s = failed_at(9, 20, 3);
        assert!(!is_due(at(2026, 9, 22, 23, 0), "workbuddy", &cfg_retry(2, 5), &s));
    }

    #[test]
    fn zero_retry_means_no_second_chance() {
        let s = failed_at(9, 20, 1);
        assert!(!is_due(at(2026, 9, 22, 23, 0), "workbuddy", &cfg_retry(0, 5), &s));
    }

    #[test]
    fn due_again_next_day_even_if_signed_yesterday() {
        // 次日摘要按当日划界，昨天的运行不在其中
        assert!(is_due(at(2026, 9, 22, 9, 15), "workbuddy", &cfg(Mode::Remind, "09:00"), &TodaySummary::default()));
    }

    /// windowPending 不等于「今天处理完了」：等一个重试间隔还要再看一眼，
    /// 否则窗口一开也没人去领（回归：10:00 前手点一次就把整天堵死）。
    #[test]
    fn window_pending_gets_reevaluated_after_retry_interval() {
        let s = summary(at(2026, 9, 22, 9, 20).timestamp(), OUTCOME_WINDOW_PENDING, 0);
        assert!(!is_due(at(2026, 9, 22, 9, 22), "workbuddy", &cfg_retry(2, 5), &s), "间隔还没到");
        assert!(is_due(at(2026, 9, 22, 9, 30), "workbuddy", &cfg_retry(2, 5), &s), "间隔到了就该再看");
    }

    /// 它也不是失败：补偿额度一次都不该被它吃掉
    #[test]
    fn window_pending_does_not_consume_retry_budget() {
        let s = summary(at(2026, 9, 22, 9, 20).timestamp(), OUTCOME_WINDOW_PENDING, 9);
        assert!(is_due(at(2026, 9, 22, 9, 30), "workbuddy", &cfg_retry(2, 5), &s));
    }

    /// 10:00 前点「立即执行一次」→ 落 windowPending → 当天 10:05 的定时执行必须还活着
    #[test]
    fn manual_run_before_window_keeps_scheduled_run_alive() {
        let cfg = ProductConfig::base("qoder", Mode::Auto, "10:05");
        let s = summary(at(2026, 9, 22, 9, 30).timestamp(), OUTCOME_WINDOW_PENDING, 0);
        assert!(is_due(at(2026, 9, 22, 10, 20), "qoder", &cfg, &s), "窗口开了就得去领");
    }

    #[test]
    fn day_start_is_local_midnight() {
        let now = at(2026, 9, 22, 13, 45);
        assert_eq!(day_start(now), at(2026, 9, 22, 0, 0).timestamp());
    }
}
