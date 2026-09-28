use super::{AdapterError, ProductAdapter, SignOutcome, SignStatus};

pub struct MockAdapter {
    product_id: String,
    base_url: String,
    client: reqwest::Client,
}

impl MockAdapter {
    pub fn new(product_id: &str, base_url: String) -> Self {
        Self { product_id: product_id.into(), base_url, client: http_client() }
    }
}

/// 20s 总超时：防止挂起的连接让调度一直等（tick 循环里是 await，不是 sleep）
fn http_client() -> reqwest::Client {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    HTTP.get_or_init(|| reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build().unwrap_or_default()).clone()
}

/// mock 服务器不校验凭证，固定串足够
const MOCK_CRED: &str = "mock-cred";

#[async_trait::async_trait]
impl ProductAdapter for MockAdapter {
    fn id(&self) -> String { self.product_id.clone() }

    async fn query_sign_status(&self) -> Result<SignStatus, AdapterError> {
        let resp = self.client.get(format!("{}/sign/status", self.base_url))
            .bearer_auth(MOCK_CRED).send().await?;
        match resp.status().as_u16() {
            200 => {
                let v: serde_json::Value = resp.json().await?;
                match v.get("signed").and_then(|s| s.as_bool()) {
                    Some(true) => Ok(SignStatus::SignedToday),
                    Some(false) => Ok(SignStatus::NotSigned),
                    // 显式点名状态：滚动窗口未开 / 状态不明。给引擎分支测试和手工演练用
                    None => match v.get("state").and_then(|s| s.as_str()) {
                        Some("windowPending") => Ok(SignStatus::WindowPending),
                        Some("unknown") => Ok(SignStatus::Unknown),
                        _ => Err(AdapterError::SchemaChanged("status 缺少 signed".into())),
                    },
                }
            }
            401 => Err(AdapterError::AuthExpired),
            code => Err(AdapterError::SchemaChanged(format!("status 意外状态码 {code}"))),
        }
    }

    async fn sign_in(&self) -> Result<SignOutcome, AdapterError> {
        let resp = self.client.post(format!("{}/sign", self.base_url))
            .bearer_auth(MOCK_CRED).send().await?;
        match resp.status().as_u16() {
            200 => {
                let v: serde_json::Value = resp.json().await?;
                match v.get("points").and_then(|p| p.as_i64()) {
                    Some(n) => Ok(SignOutcome::Success(format!("+{n}积分"))),
                    None => Err(AdapterError::SchemaChanged("sign 缺少 points".into())),
                }
            }
            401 => Err(AdapterError::AuthExpired),
            403 => Ok(SignOutcome::NeedManual("触发验证码/风控，请手动签到".into())),
            409 => Ok(SignOutcome::AlreadySigned),
            code => Err(AdapterError::SchemaChanged(format!("sign 意外状态码 {code}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    async fn adapter_with(server: &MockServer) -> MockAdapter {
        MockAdapter::new("workbuddy", server.uri())
    }

    #[tokio::test]
    async fn status_not_signed() {
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"signed": false})))
            .mount(&s).await;
        assert_eq!(adapter_with(&s).await.query_sign_status().await.unwrap(), SignStatus::NotSigned);
    }

    #[tokio::test]
    async fn status_signed_today() {
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"signed": true})))
            .mount(&s).await;
        assert_eq!(adapter_with(&s).await.query_sign_status().await.unwrap(), SignStatus::SignedToday);
    }

    #[tokio::test]
    async fn status_401_is_auth_expired() {
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(401)).mount(&s).await;
        assert!(matches!(adapter_with(&s).await.query_sign_status().await, Err(AdapterError::AuthExpired)));
    }

    #[tokio::test]
    async fn sign_in_success_with_points() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path("/sign"))
            .and(header("authorization", "Bearer mock-cred"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "points": 10})))
            .mount(&s).await;
        let out = adapter_with(&s).await.sign_in().await.unwrap();
        assert!(matches!(out, SignOutcome::Success(d) if d.contains("10")));
    }

    #[tokio::test]
    async fn sign_in_409_already_signed() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path("/sign"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({"signed": true})))
            .mount(&s).await;
        assert_eq!(adapter_with(&s).await.sign_in().await.unwrap(), SignOutcome::AlreadySigned);
    }

    #[tokio::test]
    async fn sign_in_403_need_manual() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path("/sign"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({"captcha": true})))
            .mount(&s).await;
        assert!(matches!(adapter_with(&s).await.sign_in().await, Ok(SignOutcome::NeedManual(_))));
    }

    #[tokio::test]
    async fn unknown_schema_maps_to_schema_changed() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path("/sign"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"weird": 1})))
            .mount(&s).await;
        assert!(matches!(adapter_with(&s).await.sign_in().await, Err(AdapterError::SchemaChanged(_))));
    }
}
