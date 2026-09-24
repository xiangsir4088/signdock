#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Off,
    Remind,
    Auto,
}

/// 出厂重试策略：临时网络失败后补试 2 次、间隔 5 分钟（与原调度循环里写死的值一致）。
pub const DEFAULT_RETRY_TIMES: u32 = 2;
pub const DEFAULT_RETRY_INTERVAL_MIN: u32 = 5;

/// 引擎与调度器使用的完整配置（含凭证），只存在于 Rust 侧。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductConfig {
    pub product_id: String,
    pub mode: Mode,
    pub time_of_day: String,          // "HH:MM"
    pub token: Option<String>,
    pub retry_times: u32,
    pub retry_interval_min: u32,
}

impl ProductConfig {
    /// 测试与兜底用的最小配置：其余字段取出厂值。
    pub fn base(product_id: &str, mode: Mode, time_of_day: &str, token: Option<String>) -> Self {
        ProductConfig {
            product_id: product_id.into(), mode, time_of_day: time_of_day.into(), token,
            retry_times: DEFAULT_RETRY_TIMES, retry_interval_min: DEFAULT_RETRY_INTERVAL_MIN,
        }
    }
}

/// 界面可见/可改的配置。token 刻意缺席：凭证既不该送进 webview，也不该由前端回传
/// （前端没有原值，回传即等于清空）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Settings {
    pub product_id: String,
    pub mode: Mode,
    pub time_of_day: String,
    pub retry_times: u32,
    pub retry_interval_min: u32,
}

impl From<&ProductConfig> for Settings {
    fn from(c: &ProductConfig) -> Self {
        Settings {
            product_id: c.product_id.clone(),
            mode: c.mode,
            time_of_day: c.time_of_day.clone(),
            retry_times: c.retry_times,
            retry_interval_min: c.retry_interval_min,
        }
    }
}

/// 「临时失败」的落库 outcome：这类失败不等于今天已经处理完，调度器可按重试设置补试。
pub const OUTCOME_RETRYABLE: &str = "retryable";

/// 今日执行摘要：调度器据此决定「今天还要不要再跑一次」。
#[derive(Debug, Clone, PartialEq, Default)]
pub struct TodaySummary {
    pub last_at: Option<i64>,
    pub last_outcome: Option<String>,
    pub retry_failures: u32,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRow {
    pub id: i64,
    pub product_id: String,
    pub at: String,                    // RFC3339
    pub outcome: String,               // "success" | "alreadySigned" | "reminded" | "needManual" | "failed"
    pub detail: String,
}
