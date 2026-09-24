pub mod trae;
pub mod mock;
pub mod workbuddy;
pub mod qoder;

#[derive(Debug, Clone, PartialEq)]
pub enum SignStatus {
    SignedToday,
    NotSigned,
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
    /// token 允许留空（适配器会读本机产品登录态自动提取）时为 true。
    fn accepts_empty_token(&self) -> bool { false }
    /// Err(_) = 网络/临时错误或凭证问题（可重试性由上层按变体判断）；Ok = 签到系统明确应答
    async fn query_sign_status(&self, token: &str) -> Result<SignStatus, AdapterError>;
    async fn sign_in(&self, token: &str) -> Result<SignOutcome, AdapterError>;
    /// 积分快照；未支持的产品返回 SchemaChanged（前端仅展示错误文本）。
    async fn fetch_credits(&self, _token: &str) -> Result<CreditsSnapshot, AdapterError> {
        Err(AdapterError::SchemaChanged("该产品暂不支持积分查询".into()))
    }
    /// 当前登录账号的可读标识（昵称/用户名，退化到完整 id）。只读本地，绝不联网、绝不回显 token。
    async fn account_label(&self, _token: &str) -> Result<String, AdapterError> {
        Err(AdapterError::SchemaChanged("该产品暂不支持账号查询".into()))
    }
}
