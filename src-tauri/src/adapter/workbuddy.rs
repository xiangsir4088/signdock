use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use super::{AdapterError, ProductAdapter, SignOutcome, SignStatus};

pub const PROD_BASE: &str = "https://www.codebuddy.cn";
const STATUS_PATH: &str = "/v2/billing/meter/checkin-activity-status";
const SIGN_PATH: &str = "/v2/billing/meter/daily-checkin";
const REFRESH_PATH: &str = "/v2/plugin/auth/token/refresh";
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";
/// 距过期不足 24h 即尝试刷新（对齐 workbuddy-switch 的惰性刷新阈值）
const REFRESH_AHEAD_MS: i64 = 24 * 3600 * 1000;

#[derive(Debug, Clone, PartialEq)]
pub struct WorkbuddyCred {
    pub access_token: String,
    pub refresh_token: String,
    pub uid: String,
    pub domain: String,
    pub expires_at_ms: i64,
}

pub fn default_auth_path() -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    Some(Path::new(&local)
        .join("CodeBuddyExtension")
        .join("Data").join("Public").join("auth")
        .join("workbuddy-desktop.info"))
}

fn str_field(obj: &Value, key: &str) -> String {
    obj.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn msg_of(v: &Value) -> String {
    v.get("msg").or_else(|| v.get("message"))
        .and_then(Value::as_str).unwrap_or("").to_string()
}

pub fn read_cred(path: &Path) -> Result<WorkbuddyCred, AdapterError> {
    let raw = std::fs::read_to_string(path).map_err(|_| AdapterError::AuthExpired)?;
    let v: Value = serde_json::from_str(&raw).map_err(|_| AdapterError::AuthExpired)?;
    let auth = v.get("auth").cloned().unwrap_or(Value::Null);
    let account = v.get("account").cloned().unwrap_or(Value::Null);
    let access_token = str_field(&auth, "accessToken");
    if access_token.is_empty() {
        return Err(AdapterError::AuthExpired);
    }
    Ok(WorkbuddyCred {
        access_token,
        refresh_token: str_field(&auth, "refreshToken"),
        uid: str_field(&account, "uid"),
        domain: str_field(&auth, "domain"),
        expires_at_ms: auth.get("expiresAt").and_then(Value::as_i64).unwrap_or(0),
    })
}

pub struct WorkbuddyAdapter {
    base_url: String,
    client: reqwest::Client,
    /// None=自动探测本机 auth 文件；测试用 Some 注入临时文件
    cred_path: Option<PathBuf>,
}

impl WorkbuddyAdapter {
    pub fn new() -> Self { Self::with_base_url(PROD_BASE) }

    pub fn with_base_url(base_url: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(UA)
            .build().unwrap_or_default();
        Self { base_url: base_url.trim_end_matches('/').to_string(), client, cred_path: None }
    }

    pub fn with_cred_path(mut self, path: PathBuf) -> Self { self.cred_path = Some(path); self }

    fn headers_for(&self, cred: &WorkbuddyCred) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Ok(v) = HeaderValue::from_str(&format!("Bearer {}", cred.access_token)) { h.insert(AUTHORIZATION, v); }
        h.insert(ACCEPT, HeaderValue::from_static("application/json"));
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if !cred.uid.is_empty() {
            if let Ok(v) = HeaderValue::from_str(&cred.uid) { h.insert("X-User-Id", v); }
        }
        if !cred.domain.is_empty() {
            if let Ok(v) = HeaderValue::from_str(&cred.domain) { h.insert("X-Domain", v); }
        }
        h
    }

    /// 手动 token 优先；否则读 auth 文件，过期则内存刷新（绝不回写产品文件）
    async fn resolve_cred(&self, manual_token: &str) -> Result<WorkbuddyCred, AdapterError> {
        if !manual_token.trim().is_empty() {
            return Ok(WorkbuddyCred {
                access_token: manual_token.trim().to_string(),
                refresh_token: String::new(),
                uid: String::new(),
                domain: String::new(),
                expires_at_ms: i64::MAX,
            });
        }
        let path = match &self.cred_path {
            Some(p) => p.clone(),
            None => default_auth_path().ok_or(AdapterError::AuthExpired)?,
        };
        let cred = read_cred(&path)?;
        let now = Utc::now().timestamp_millis();
        let expired = cred.expires_at_ms <= now;
        if expired || cred.expires_at_ms - now < REFRESH_AHEAD_MS {
            match self.refresh_cred(&cred).await {
                Ok(fresh) => return Ok(fresh),
                Err(e) if expired => return Err(e),
                // 未过期但主动刷新失败：照旧使用现有 token
                Err(_) => return Ok(cred),
            }
        }
        Ok(cred)
    }

    async fn refresh_cred(&self, cred: &WorkbuddyCred) -> Result<WorkbuddyCred, AdapterError> {
        if cred.refresh_token.is_empty() {
            return Err(AdapterError::AuthExpired);
        }
        let mut headers = self.headers_for(cred);
        if let Ok(v) = HeaderValue::from_str(&cred.refresh_token) { headers.insert("X-Refresh-Token", v); }
        headers.insert("X-Auth-Refresh-Source", HeaderValue::from_static("plugin"));
        let resp = self.client
            .post(format!("{}{REFRESH_PATH}", self.base_url))
            .headers(headers).json(&json!({})).send().await?;
        if resp.status().as_u16() == 401 {
            return Err(AdapterError::AuthExpired);
        }
        let v: Value = resp.json().await?;
        if v.get("code").and_then(Value::as_i64) != Some(0) {
            return Err(AdapterError::AuthExpired);
        }
        let d = v.get("data").cloned().unwrap_or(Value::Null);
        let access_token = str_field(&d, "accessToken");
        if access_token.is_empty() {
            return Err(AdapterError::AuthExpired);
        }
        let now = Utc::now().timestamp_millis();
        let expires_at_ms = d.get("expiresAt").and_then(Value::as_i64)
            .or_else(|| d.get("expiresIn").and_then(Value::as_i64).map(|s| now + s * 1000))
            .unwrap_or(0);
        let new_refresh = str_field(&d, "refreshToken");
        let new_domain = str_field(&d, "domain");
        Ok(WorkbuddyCred {
            access_token,
            refresh_token: if new_refresh.is_empty() { cred.refresh_token.clone() } else { new_refresh },
            uid: cred.uid.clone(),
            domain: if new_domain.is_empty() { cred.domain.clone() } else { new_domain },
            expires_at_ms,
        })
    }

    async fn post_meter(&self, api_path: &str, cred: &WorkbuddyCred) -> Result<Value, AdapterError> {
        let resp = self.client
            .post(format!("{}{api_path}", self.base_url))
            .headers(self.headers_for(cred)).json(&json!({})).send().await?;
        // workbuddy 无验证码机制：401/403 一律按鉴权失败（决策见计划 Global Constraints）
        if matches!(resp.status().as_u16(), 401 | 403) {
            return Err(AdapterError::AuthExpired);
        }
        let v: Value = resp.json().await
            .map_err(|_| AdapterError::SchemaChanged(format!("{api_path} 返回非 JSON")))?;
        Ok(v)
    }
}

#[async_trait::async_trait]
impl ProductAdapter for WorkbuddyAdapter {
    fn id(&self) -> String { "workbuddy".into() }
    fn accepts_empty_token(&self) -> bool { true }

    async fn query_sign_status(&self, token: &str) -> Result<SignStatus, AdapterError> {
        let cred = self.resolve_cred(token).await?;
        let v = self.post_meter(STATUS_PATH, &cred).await?;
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code != 0 && code != 200 {
            return Err(AdapterError::SchemaChanged(format!("status code={code} msg={}", msg_of(&v))));
        }
        let d = v.get("data").cloned().unwrap_or(Value::Null);
        if d.get("active").and_then(Value::as_bool) == Some(false) {
            return Ok(SignStatus::Unknown);
        }
        let signed = d.get("today_checked_in").and_then(Value::as_bool)
            .or_else(|| d.get("todayCheckedIn").and_then(Value::as_bool));
        match signed {
            Some(true) => Ok(SignStatus::SignedToday),
            Some(false) => Ok(SignStatus::NotSigned),
            None => Err(AdapterError::SchemaChanged("status 缺少 today_checked_in".into())),
        }
    }

    async fn sign_in(&self, token: &str) -> Result<SignOutcome, AdapterError> {
        let cred = self.resolve_cred(token).await?;
        let v = self.post_meter(SIGN_PATH, &cred).await?;
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == 0 || code == 200 {
            let d = v.get("data").cloned().unwrap_or(Value::Null);
            let detail = d.get("today_credit").and_then(Value::as_i64)
                .or_else(|| d.get("points").and_then(Value::as_i64))
                .map(|n| format!("+{n}积分"))
                .unwrap_or_else(|| "ok".into());
            return Ok(SignOutcome::Success(detail));
        }
        let msg = msg_of(&v);
        if msg.contains("已签到") || msg.to_lowercase().contains("repeat") {
            return Ok(SignOutcome::AlreadySigned);
        }
        if ["未开启", "未开放", "已过期"].iter().any(|k| msg.contains(k)) {
            return Ok(SignOutcome::NeedManual(format!("签到活动不可用：{msg}")));
        }
        Err(AdapterError::SchemaChanged(format!("sign code={code} msg={msg}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn write_cred(tag: &str, access: &str, refresh: &str, expires_at: i64) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("signdock-wb-{tag}-{}.info", std::process::id()));
        let v = json!({
            "account": { "uid": "uid-1" },
            "auth": { "accessToken": access, "refreshToken": refresh,
                      "domain": "www.codebuddy.cn", "expiresAt": expires_at }
        });
        std::fs::write(&p, v.to_string()).unwrap();
        p
    }
    fn future_ms() -> i64 { Utc::now().timestamp_millis() + 30 * 24 * 3600 * 1000 }
    fn past_ms() -> i64 { Utc::now().timestamp_millis() - 1000 }

    #[test]
    fn read_cred_parses_fields() {
        let p = write_cred("parse", "AT", "RT", 123);
        let c = read_cred(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(c.access_token, "AT");
        assert_eq!(c.refresh_token, "RT");
        assert_eq!(c.uid, "uid-1");
        assert_eq!(c.domain, "www.codebuddy.cn");
        assert_eq!(c.expires_at_ms, 123);
    }

    #[test]
    fn read_cred_missing_file_is_auth_expired() {
        assert!(matches!(read_cred(Path::new("Z:\\definitely-not-here.info")), Err(AdapterError::AuthExpired)));
    }

    #[test]
    fn read_cred_empty_token_is_auth_expired() {
        let p = write_cred("empty", "", "RT", future_ms());
        assert!(matches!(read_cred(&p), Err(AdapterError::AuthExpired)));
        std::fs::remove_file(&p).ok();
    }

    fn adapter(s: &MockServer, cred: PathBuf) -> WorkbuddyAdapter {
        WorkbuddyAdapter::with_base_url(&s.uri()).with_cred_path(cred)
    }

    #[tokio::test]
    async fn status_signed_today_sends_bearer_and_uid_headers() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .and(header("authorization", "Bearer AT"))
            .and(header("x-user-id", "uid-1"))
            .and(header("x-domain", "www.codebuddy.cn"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"msg":"OK","data":{"active":true,"today_checked_in":true}})))
            .mount(&s).await;
        let cred = write_cred("st", "AT", "RT", future_ms());
        assert_eq!(adapter(&s, cred.clone()).query_sign_status("").await.unwrap(), SignStatus::SignedToday);
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn status_not_signed_camel_case_fallback() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":true,"todayCheckedIn":false}})))
            .mount(&s).await;
        let cred = write_cred("camel", "AT", "RT", future_ms());
        assert_eq!(adapter(&s, cred.clone()).query_sign_status("").await.unwrap(), SignStatus::NotSigned);
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn status_inactive_is_unknown() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":false}})))
            .mount(&s).await;
        let cred = write_cred("inact", "AT", "RT", future_ms());
        assert_eq!(adapter(&s, cred.clone()).query_sign_status("").await.unwrap(), SignStatus::Unknown);
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn status_missing_field_is_schema_changed() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"weird":1}})))
            .mount(&s).await;
        let cred = write_cred("weird", "AT", "RT", future_ms());
        assert!(matches!(adapter(&s, cred.clone()).query_sign_status("").await,
            Err(AdapterError::SchemaChanged(_))));
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn sign_success_reads_credit_detail() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(SIGN_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"today_credit":100}})))
            .mount(&s).await;
        let cred = write_cred("ok", "AT", "RT", future_ms());
        let out = adapter(&s, cred.clone()).sign_in("").await.unwrap();
        assert!(matches!(&out, SignOutcome::Success(d) if d.contains("100")));
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn sign_already_by_message() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(SIGN_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":1,"msg":"今日已签到"})))
            .mount(&s).await;
        let cred = write_cred("again", "AT", "RT", future_ms());
        assert_eq!(adapter(&s, cred.clone()).sign_in("").await.unwrap(), SignOutcome::AlreadySigned);
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn sign_inactive_activity_is_need_manual() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(SIGN_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":10011,"message":"签到活动已过期"})))
            .mount(&s).await;
        let cred = write_cred("na", "AT", "RT", future_ms());
        assert!(matches!(adapter(&s, cred.clone()).sign_in("").await, Ok(SignOutcome::NeedManual(_))));
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn sign_401_is_auth_expired() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(SIGN_PATH))
            .respond_with(ResponseTemplate::new(401)).mount(&s).await;
        let cred = write_cred("401", "AT", "RT", future_ms());
        assert!(matches!(adapter(&s, cred.clone()).sign_in("").await, Err(AdapterError::AuthExpired)));
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn expired_file_triggers_in_memory_refresh() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(REFRESH_PATH))
            .and(header("x-refresh-token", "RT"))
            .and(header("x-auth-refresh-source", "plugin"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"accessToken":"NEW","expiresIn":3600}})))
            .mount(&s).await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .and(header("authorization", "Bearer NEW"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":true,"today_checked_in":false}})))
            .mount(&s).await;
        let cred = write_cred("refresh", "OLD", "RT", past_ms());
        let a = adapter(&s, cred.clone());
        assert_eq!(a.query_sign_status("").await.unwrap(), SignStatus::NotSigned);
        // 绝不回写：文件里的 token 必须原样是 OLD
        assert_eq!(read_cred(&cred).unwrap().access_token, "OLD");
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn refresh_failure_when_expired_is_auth_expired() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(REFRESH_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":12153,"msg":"invalid_grant"})))
            .mount(&s).await;
        let cred = write_cred("rf", "OLD", "RT", past_ms());
        assert!(matches!(adapter(&s, cred.clone()).query_sign_status("").await,
            Err(AdapterError::AuthExpired)));
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn manual_token_skips_auth_file_entirely() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .and(header("authorization", "Bearer MANUAL"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":true,"today_checked_in":false}})))
            .mount(&s).await;
        // cred_path 指向不存在的文件：只有完全跳过文件读取才能成功
        let a = WorkbuddyAdapter::with_base_url(&s.uri())
            .with_cred_path(PathBuf::from("Z:\\no-such.info"));
        assert_eq!(a.query_sign_status("MANUAL").await.unwrap(), SignStatus::NotSigned);
    }
}
