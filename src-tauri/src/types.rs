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

/// 产品配置：调度与界面共用的唯一视图。凭证刻意不在此列——各适配器自己取凭据（读本机登录态、
/// 官方 OAuth，或 SignDock 自有封存文件），手工粘贴 token 这条路径已整体删除。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductConfig {
    pub product_id: String,
    pub mode: Mode,
    pub time_of_day: String,          // "HH:MM"
    pub retry_times: u32,
    pub retry_interval_min: u32,
}

impl ProductConfig {
    /// 测试与兜底用的最小配置：其余字段取出厂值。
    pub fn base(product_id: &str, mode: Mode, time_of_day: &str) -> Self {
        ProductConfig {
            product_id: product_id.into(), mode, time_of_day: time_of_day.into(),
            retry_times: DEFAULT_RETRY_TIMES, retry_interval_min: DEFAULT_RETRY_INTERVAL_MIN,
        }
    }
}

/// 「临时失败」的落库 outcome：这类失败不等于今天已经处理完，调度器可按重试设置补试。
pub const OUTCOME_RETRYABLE: &str = "retryable";
/// 「今日窗口未开」的落库 outcome：同样不等于处理完了，但它不是失败，
/// 所以只按重试间隔回头再看，**不消耗补偿额度**。
pub const OUTCOME_WINDOW_PENDING: &str = "windowPending";

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
    pub outcome: String,               // "success" | "alreadySigned" | "windowPending" | "reminded" | "needManual" | "retryable" | "failed"
    pub detail: String,
}
