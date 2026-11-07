//! 灵镜AIGC（lingjing.uumit.com，UUMit 自研 AIGC 短剧工作台）。
//!
//! 签到机制：积分制。每日签到累计积分抵扣生成额度（生图免费、失败不扣积分）。
//! 2026-11-07 逆向确认端点（前端 bundle `accountApi` chunk）：
//!   GET  /api/v1/checkin/status  -> {enabled, today_checked, daily_credits, streak_days, streak_cycle_days}
//!   POST /api/v1/checkin         -> {status:"claimed", credits, bonus_credits, streak_days}
//!   GET  /api/v1/credits/account -> {balance, ...}
//!   GET  /api/v1/users/me        -> {profile:{nickname, email}}
//! 认证：JWT Bearer（accessToken + refreshToken），前端存 localStorage `uumit-aigc-auth`；
//!   401 时 POST /api/v1/auth/refresh {refresh_token} -> {access_token, refresh_token} 自动续期。
//!   未认证响应：HTTP 401 `{code:40100,message:"请先登录"}`；成功：`{code:0,data:...}`。
//!
//! 凭据来自 SignDock 自己的 WebView2 profile：用户在设置页登录一次，我们从该窗口的
//! localStorage 读出整包 token 封存下来（DPAPI），之后每天只转发它，401 自动刷新并回写。
//! 绝不读用户浏览器的任何存储。

use std::path::{Path, PathBuf};

use reqwest::{header::HeaderMap, Method};
use serde_json::Value;

use super::{
    transport_error, AdapterError, CreditsSnapshot, ProductAdapter, SignOutcome, SignStatus,
};
use crate::adapter::http_client;

pub const LINGJING_ORIGIN: &str = "https://lingjing.uumit.com";

const CHECKIN_STATUS_PATH: &str = "/api/v1/checkin/status";
const CHECKIN_CLAIM_PATH: &str = "/api/v1/checkin";
const CREDITS_PATH: &str = "/api/v1/credits/account";
const ME_PATH: &str = "/api/v1/users/me";
const REFRESH_PATH: &str = "/api/v1/auth/refresh";

const NOT_LOGGED_IN_HINT: &str = "尚未登录灵镜；点「打开登录窗口」登录并取会话";
const SESSION_DEAD_HINT: &str = "灵镜会话已失效，请在设置页重新登录一次";

fn app_data_dir() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(Path::new(&appdata).join("com.signdock.app"))
}

/// SignDock 自有的灵镜凭据（可以回写；产品自己的 localStorage 我们从不碰）
pub fn managed_cred_path() -> Option<PathBuf> {
    Some(app_data_dir()?.join("lingjing-cred.json"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct LingjingCred {
    pub access_token: String,
    pub refresh_token: String,
    pub saved_at_ms: i64,
}

pub fn read_cred(path: &Path) -> Result<LingjingCred, AdapterError> {
    let v = crate::secret::read_json(path).map_err(|e| match e {
        crate::secret::SecretError::NotFound => {
            AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into())
        }
        _ => AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()),
    })?;
    let access_token = str_field(&v, "accessToken");
    if access_token.is_empty() {
        return Err(AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()));
    }
    Ok(LingjingCred {
        access_token,
        refresh_token: str_field(&v, "refreshToken"),
        saved_at_ms: v.get("savedAtMs").and_then(Value::as_i64).unwrap_or(0),
    })
}

pub fn write_cred(path: &Path, cred: &LingjingCred) -> Result<(), AdapterError> {
    let v = serde_json::json!({
        "accessToken": cred.access_token,
        "refreshToken": cred.refresh_token,
        "savedAtMs": cred.saved_at_ms,
    });
    crate::secret::write_json(path, &v).map_err(|_| AdapterError::AuthExpired)
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

pub struct LingjingAdapter {
    base_url: String,
    client: reqwest::Client,
    cred_path: Option<PathBuf>,
}

impl Default for LingjingAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl LingjingAdapter {
    pub fn new() -> Self {
        Self::with_base_url(LINGJING_ORIGIN).with_cred_path(managed_cred_path())
    }

    pub fn with_base_url(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: http_client(),
            cred_path: None,
        }
    }

    pub fn with_cred_path(mut self, path: Option<PathBuf>) -> Self {
        self.cred_path = path;
        self
    }

    fn headers(&self, access_token: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Ok(v) =
            reqwest::header::HeaderValue::from_str(&format!("Bearer {access_token}"))
        {
            h.insert(reqwest::header::AUTHORIZATION, v);
        }
        h
    }

    /// 读凭据；无凭据返回带可操作提示的 AuthExpiredMsg
    fn cred(&self) -> Result<LingjingCred, AdapterError> {
        let path = self.cred_path.clone().ok_or_else(|| {
            AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into())
        })?;
        read_cred(&path)
    }

    /// 用 refresh_token 换新对并回写。灵镜 refresh 失败（401/400）= 会话彻底死，
    /// 只能重新登录，不是接口变更 —— 与其它 4xx 分开。
    async fn refresh(&self, cred: &LingjingCred) -> Result<LingjingCred, AdapterError> {
        if cred.refresh_token.is_empty() {
            return Err(AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()));
        }
        let url = format!("{}{REFRESH_PATH}", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "refresh_token": cred.refresh_token }))
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()));
        }
        if let Some(e) = transport_error(resp.status()) {
            return Err(e);
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|_| AdapterError::SchemaChanged("refresh 返回非 JSON".into()))?;
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        let access_token = str_field(&data, "access_token");
        if access_token.is_empty() {
            return Err(AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()));
        }
        let fresh = LingjingCred {
            access_token,
            refresh_token: str_field(&data, "refresh_token"),
            saved_at_ms: cred.saved_at_ms,
        };
        if let Some(path) = self.cred_path.clone() {
            let _ = write_cred(&path, &fresh);
        }
        Ok(fresh)
    }

    /// 登录探针：用传入的裸 access token 验证是否有效（未写凭据前调用）。
    /// 成功=token 被服务端认账；401=未登录（继续等）；网络=临时失败。
    pub async fn probe(&self, access_token: &str) -> Result<(), AdapterError> {
        let url = format!("{}{ME_PATH}", self.base_url);
        let resp = self
            .client
            .get(&url)
            .headers(self.headers(access_token))
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AdapterError::AuthExpired);
        }
        if let Some(e) = transport_error(resp.status()) {
            return Err(e);
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|_| AdapterError::SchemaChanged("users/me 返回非 JSON".into()))?;
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == 0 {
            Ok(())
        } else {
            Err(AdapterError::Business(format!("code={code}")))
        }
    }

    /// 带鉴权的请求。401 时用 refresh_token 换新后重试一次；重试仍 401 = 会话死。
    /// `body: None` 表示不发 body（灵镜 POST /checkin 无 body；refresh 才带）。
    async fn authed(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, AdapterError> {
        let cred = self.cred()?;
        self.authed_with(cred, method, path, body).await
    }

    async fn authed_with(
        &self,
        cred: LingjingCred,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<Value, AdapterError> {
        let url = format!("{}{}", self.base_url, path);
        let resp = self
            .send_auth(&method, &url, &cred.access_token, body.clone())
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED && !cred.refresh_token.is_empty() {
            let fresh = self.refresh(&cred).await?;
            let resp2 = self
                .send_auth(&method, &url, &fresh.access_token, body)
                .await?;
            if resp2.status() == reqwest::StatusCode::UNAUTHORIZED {
                return Err(AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()));
            }
            return Self::parse(resp2).await;
        }
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()));
        }
        Self::parse(resp).await
    }

    async fn send_auth(
        &self,
        method: &Method,
        url: &str,
        access_token: &str,
        body: Option<Value>,
    ) -> Result<reqwest::Response, reqwest::Error> {
        let mut req = self
            .client
            .request(method.clone(), url)
            .headers(self.headers(access_token));
        if let Some(b) = body {
            req = req.json(&b);
        }
        req.send().await
    }

    /// 解析灵镜响应：`{code, message, data}`，code==0 时返回 data。
    /// 传输层错误（5xx/408/429）优先于业务解析——网关 5xx 也可能带合法 JSON。
    async fn parse(resp: reqwest::Response) -> Result<Value, AdapterError> {
        if let Some(e) = transport_error(resp.status()) {
            return Err(e);
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|_| AdapterError::SchemaChanged("返回非 JSON".into()))?;
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == 0 {
            Ok(v.get("data").cloned().unwrap_or(Value::Null))
        } else {
            let msg = v
                .get("message")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if code == 40100 {
                // 未登录（HTTP 401 已被上层拦截，这里是兜底）
                Err(AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()))
            } else {
                Err(AdapterError::Business(format!("code={code} {msg}")))
            }
        }
    }
}

#[async_trait::async_trait]
impl ProductAdapter for LingjingAdapter {
    fn id(&self) -> String {
        "lingjing".into()
    }

    async fn query_sign_status(&self) -> Result<SignStatus, AdapterError> {
        let data = self.authed(Method::GET, CHECKIN_STATUS_PATH, None).await?;
        let enabled = data.get("enabled").and_then(Value::as_bool).unwrap_or(false);
        let today_checked = data
            .get("today_checked")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !enabled {
            // 活动未开/已下线：不盲发 claim，按 Unknown 收着（上层通知一次转人工）
            Ok(SignStatus::Unknown)
        } else if today_checked {
            Ok(SignStatus::SignedToday)
        } else {
            Ok(SignStatus::NotSigned)
        }
    }

    async fn sign_in(&self) -> Result<SignOutcome, AdapterError> {
        // 前端实测 POST /checkin 不带 body
        let data = self
            .authed(Method::POST, CHECKIN_CLAIM_PATH, None)
            .await?;
        let status = str_field(&data, "status");
        if status == "claimed" {
            let credits = data.get("credits").and_then(Value::as_i64).unwrap_or(0);
            let bonus = data
                .get("bonus_credits")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            let streak = data.get("streak_days").and_then(Value::as_i64).unwrap_or(0);
            let total = credits + bonus.max(0);
            if bonus > 0 {
                Ok(SignOutcome::Success(format!(
                    "+{total}积分（含奖励{bonus}，连签{streak}天）"
                )))
            } else {
                Ok(SignOutcome::Success(format!("+{total}积分（连签{streak}天）")))
            }
        } else {
            // status 非 claimed：状态已变化（他人/本机已签）。按已签收着，不重试、不报错。
            Ok(SignOutcome::AlreadySigned)
        }
    }

    async fn fetch_credits(&self) -> Result<CreditsSnapshot, AdapterError> {
        let data = self.authed(Method::GET, CREDITS_PATH, None).await?;
        let balance = data.get("balance").and_then(Value::as_f64).unwrap_or(0.0);
        Ok(CreditsSnapshot {
            balance,
            today_used: 0.0,
            expiring_today: 0.0,
            expiring_tomorrow: 0.0,
            fetched_at_ms: chrono::Utc::now().timestamp_millis(),
        })
    }

    async fn account_label(&self) -> Result<String, AdapterError> {
        let data = self.authed(Method::GET, ME_PATH, None).await?;
        let profile = data.get("profile").cloned().unwrap_or(Value::Null);
        let nickname = str_field(&profile, "nickname");
        let email = str_field(&profile, "email");
        if !nickname.is_empty() {
            Ok(nickname)
        } else if !email.is_empty() {
            Ok(email)
        } else {
            Ok("灵镜用户".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cred_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "signdock-lj-test-{}-{}",
            std::process::id(),
            tag
        ));
        p
    }

    fn adapter(server_uri: &str, path: &Path) -> LingjingAdapter {
        LingjingAdapter::with_base_url(server_uri).with_cred_path(Some(path.to_path_buf()))
    }

    fn seed_cred(path: &Path, access: &str, refresh: &str) {
        write_cred(
            path,
            &LingjingCred {
                access_token: access.into(),
                refresh_token: refresh.into(),
                saved_at_ms: 1,
            },
        )
        .unwrap();
    }

    #[tokio::test]
    async fn status_not_signed_returns_not_signed() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/checkin/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "data": {"enabled": true, "today_checked": false, "daily_credits": 10}
            })))
            .mount(&s)
            .await;
        let p = cred_path("notsigned");
        seed_cred(&p, "tok", "ref");
        let r = adapter(&s.uri(), &p).query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert_eq!(r.unwrap(), SignStatus::NotSigned);
    }

    #[tokio::test]
    async fn status_signed_today_short_circuits() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/checkin/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "data": {"enabled": true, "today_checked": true, "streak_days": 4}
            })))
            .mount(&s)
            .await;
        let p = cred_path("signed");
        seed_cred(&p, "tok", "ref");
        let r = adapter(&s.uri(), &p).query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert_eq!(r.unwrap(), SignStatus::SignedToday);
    }

    #[tokio::test]
    async fn status_disabled_returns_unknown() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/checkin/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "data": {"enabled": false, "today_checked": false}
            })))
            .mount(&s)
            .await;
        let p = cred_path("disabled");
        seed_cred(&p, "tok", "ref");
        let r = adapter(&s.uri(), &p).query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert_eq!(r.unwrap(), SignStatus::Unknown);
    }

    #[tokio::test]
    async fn claim_success_reports_credits() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/checkin"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "data": {"status": "claimed", "credits": 20, "bonus_credits": 0, "streak_days": 3}
            })))
            .mount(&s)
            .await;
        let p = cred_path("claim");
        seed_cred(&p, "tok", "ref");
        let r = adapter(&s.uri(), &p).sign_in().await;
        std::fs::remove_file(&p).ok();
        assert!(matches!(&r, Ok(SignOutcome::Success(d)) if d.contains("20") && d.contains("3")));
    }

    #[tokio::test]
    async fn claim_with_bonus_reports_total() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/api/v1/checkin"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "data": {"status": "claimed", "credits": 20, "bonus_credits": 10, "streak_days": 7}
            })))
            .mount(&s)
            .await;
        let p = cred_path("bonus");
        seed_cred(&p, "tok", "ref");
        let r = adapter(&s.uri(), &p).sign_in().await;
        std::fs::remove_file(&p).ok();
        assert!(matches!(&r, Ok(SignOutcome::Success(d)) if d.contains("30") && d.contains("10")));
    }

    /// 401 → 自动 refresh → 用新 token 重试成功
    #[tokio::test]
    async fn unauthorized_triggers_refresh_and_retries() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/checkin/status"))
            .and(header("authorization", "Bearer old"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "code": 40100, "message": "请先登录"
            })))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/refresh"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "data": {"access_token": "new", "refresh_token": "newr"}
            })))
            .mount(&s)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/v1/checkin/status"))
            .and(header("authorization", "Bearer new"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "code": 0, "data": {"enabled": true, "today_checked": false}
            })))
            .mount(&s)
            .await;
        let p = cred_path("refresh");
        seed_cred(&p, "old", "ref");
        let r = adapter(&s.uri(), &p).query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert_eq!(r.unwrap(), SignStatus::NotSigned);
    }

    /// refresh 也 401 → 会话死，报 AuthExpiredMsg
    #[tokio::test]
    async fn refresh_fail_reports_auth_expired() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/checkin/status"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "code": 40100, "message": "请先登录"
            })))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/v1/auth/refresh"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "code": 40100, "message": "refresh token 失效"
            })))
            .mount(&s)
            .await;
        let p = cred_path("refreshfail");
        seed_cred(&p, "old", "dead");
        let r = adapter(&s.uri(), &p).query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert!(matches!(r, Err(AdapterError::AuthExpiredMsg(_))));
    }

    /// 缺凭据文件 → 可操作的登录提示
    #[tokio::test]
    async fn missing_cred_reports_login_hint() {
        let p = cred_path("nocred");
        std::fs::remove_file(&p).ok();
        let a = LingjingAdapter::with_base_url("http://127.0.0.1:9").with_cred_path(Some(p.clone()));
        let r = a.query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert!(matches!(r, Err(AdapterError::AuthExpiredMsg(m)) if m.contains("登录")));
    }

    /// 网络层失败（连不上）→ Http 可重试
    #[tokio::test]
    async fn network_failure_is_http_retryable() {
        let p = cred_path("net");
        seed_cred(&p, "tok", "ref");
        let a = LingjingAdapter::with_base_url("http://127.0.0.1:1").with_cred_path(Some(p.clone()));
        let r = a.query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert!(matches!(r, Err(AdapterError::Http(_))));
    }
}
