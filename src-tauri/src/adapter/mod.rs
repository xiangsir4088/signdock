pub mod trae;
pub mod mock;
pub mod workbuddy;
pub mod qoder;
pub mod miaoda;
use std::sync::OnceLock;

/// 全进程共享的 HTTP 客户端。适配器每轮调度、每次点击都会重建，
/// 而 Client 背后是连接池 + TLS 配置——重建等于每次重新握手。
/// Client 内部是 Arc，clone 只是加一次引用。
pub fn http_client() -> reqwest::Client {
    static HTTP: OnceLock<reqwest::Client> = OnceLock::new();
    HTTP.get_or_init(reqwest::Client::new).clone()
}

#[derive(Debug, Clone, PartialEq)]
pub enum SignStatus {
    SignedToday,
    NotSigned,
    /// 滚动窗口产品专用：今天这一轮的窗口还没开，看到的「已领取」属于上一轮。
    /// 它既不是「今天处理完了」（不能落终态），也不该动手领（领了就是上一轮的重放）。
    WindowPending,
    Unknown,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SignOutcome {
    Success(String),    // 详情，如 "+10积分"
    AlreadySigned,
    NeedManual(String), // 验证码/风控提示，转人工
    Failed(String),     // 业务性失败，不重试
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("网络错误: {0}")]
    Http(#[from] reqwest::Error),
    #[error("凭证已过期，请重新登录该产品")]
    AuthExpired,
    /// 带自定义文案的凭证失效（trae：提示打开 TRAE 让它自己刷新）
    #[error("{0}")]
    AuthExpiredMsg(String),
    #[error("接口可能已变更: {0}")]
    SchemaChanged(String),
    /// 服务端明确回了业务失败码（额度、活动开关、参数）。结构没变，只是这件事现在不行；
    /// 与 SchemaChanged 分开，否则用户看到的永远是误导性的「接口可能已变更」。
    #[error("{0}")]
    Business(String),
}

#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditsSnapshot {
    pub balance: f64,
    pub today_used: f64,
    pub expiring_today: f64,
    pub expiring_tomorrow: f64,
    pub fetched_at_ms: i64,
}

#[async_trait::async_trait]
pub trait ProductAdapter: Send + Sync {
    fn id(&self) -> String;
    /// 凭证由适配器自备：读本机登录态、SignDock 自有凭据文件或产品自己的官方登录流程。
    /// 调用方不再传 token —— 手工粘贴的 token 一旦能进来，就会压住会自动续期的凭据。
    /// Err(_) = 网络/临时错误或凭证问题（可重试性由上层按变体判断）；Ok = 签到系统明确应答
    async fn query_sign_status(&self) -> Result<SignStatus, AdapterError>;
    async fn sign_in(&self) -> Result<SignOutcome, AdapterError>;
    /// 积分快照；未支持的产品返回 SchemaChanged（前端仅展示错误文本）。
    async fn fetch_credits(&self) -> Result<CreditsSnapshot, AdapterError> {
        Err(AdapterError::SchemaChanged("该产品暂不支持积分查询".into()))
    }
    /// 当前登录账号的可读标识（昵称/用户名，退化到完整 id）。只读本地，绝不联网。
    async fn account_label(&self) -> Result<String, AdapterError> {
        Err(AdapterError::SchemaChanged("该产品暂不支持账号查询".into()))
    }
}
