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
    #[error("凭证已过期，请重新登录或更新 token")]
    AuthExpired,
    #[error("接口可能已变更: {0}")]
    SchemaChanged(String),
}

#[async_trait::async_trait]
pub trait ProductAdapter: Send + Sync {
    fn id(&self) -> String;
    /// Err(_) = 网络/临时错误或凭证问题（可重试性由上层按变体判断）；Ok = 签到系统明确应答
    async fn query_sign_status(&self, token: &str) -> Result<SignStatus, AdapterError>;
    async fn sign_in(&self, token: &str) -> Result<SignOutcome, AdapterError>;
}
