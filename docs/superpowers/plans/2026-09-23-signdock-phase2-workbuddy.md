# SignDock Phase 2（workbuddy 真实适配器）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 让 workbuddy 实现零输入自动签到：从本机 WorkBuddy 登录态文件自动提取 token（过期自动刷新），替换 MockAdapter。

**Architecture:** 新增 `adapter/workbuddy.rs`（凭证提取 + 真实接口适配器，沿用现有 `ProductAdapter` trait）；trait 增加 `accepts_empty_token()` 钩子让引擎放行"留空 = 自动提取"；`commands.rs` 增加适配器工厂并在 `sign_now`/调度循环接线。

**Tech Stack:** Rust（reqwest/serde_json/chrono 已有，**不新增依赖**）、wiremock 测试。

**Spec:** `docs/superpowers/specs/2026-09-22-signdock-design.md`（方案 C：自动提取为主、手动粘贴兜底；错误表：AuthExpired→提示登录产品或粘贴 token）
**Recon:** `docs/recon/workbuddy.md`（2026-09-23 实测：接口/路径/响应样例均已验证，端点 `POST https://www.codebuddy.cn/v2/billing/meter/checkin-activity-status` 返回 200 实测通过）

## Global Constraints

- **绝不回写产品文件**：只读 `%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`；刷新得到的新 token 仅在内存使用。
- **测试绝不请求真实端点**：全部 wiremock；测试注入 base_url 与凭证文件路径。
- 不新增 crate 依赖（tokio/reqwest/serde_json/chrono/async-trait/wiremock 均已在 Cargo.toml）。
- outcome 字符串契约不变：`success|alreadySigned|reminded|needManual|failed`。
- `RunResult` 枚举只增不改（现有 `FailedRetryable` 语义保持：仅 `AdapterError::Http` 产生）。
- workbuddy 无验证码机制：HTTP 401/403 一律 `AuthExpired`（决策，见 recon 结论）。
- 每步之后 `cargo test` 全绿才提交；PATH 需 `export PATH="$HOME/.cargo/bin:$PATH"`。
- git 提交一律 `git -c user.name="xiangsir" -c user.email="xiangsir@local" commit`。

---

### Task 1: workbuddy 适配器 + 凭证提取（含 trait 钩子）

**Files:**
- Create: `src-tauri/src/adapter/workbuddy.rs`
- Modify: `src-tauri/src/adapter/mod.rs`（注册 `pub mod workbuddy;` + trait 加默认方法）

**Interfaces:**
- Consumes: `super::{AdapterError, ProductAdapter, SignOutcome, SignStatus}`（现有定义不变）
- Produces:
  - `WorkbuddyAdapter::new() -> Self`（真实 base_url）
  - `WorkbuddyAdapter::with_base_url(base: &str) -> Self`、`with_cred_path(self, PathBuf) -> Self`（测试注入）
  - `read_cred(path: &Path) -> Result<WorkbuddyCred, AdapterError>`、`default_auth_path() -> Option<PathBuf>`
  - trait 新方法 `fn accepts_empty_token(&self) -> bool { false }`（默认 false；WorkbuddyAdapter 覆写为 true）——Task 2 的引擎改造消费它

- [ ] **Step 1: 在 adapter/mod.rs 注册模块并给 trait 加钩子**

在 `src-tauri/src/adapter/mod.rs` 的模块声明处加 `pub mod workbuddy;`（与 `pub mod mock;` 并列），并在 `trait ProductAdapter` 内加：

```rust
    /// token 允许留空（适配器会读本机产品登录态自动提取）时为 true。
    fn accepts_empty_token(&self) -> bool { false }
```

- [ ] **Step 2: 写失败测试 + 实现 `workbuddy.rs`**

实现代码（完整）：

```rust
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
        let mut cred = read_cred(&path)?;
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
```

测试代码（完整，放同一文件 `#[cfg(test)] mod tests`）：

```rust
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
```

- [ ] **Step 3: 运行测试**

`export PATH="$HOME/.cargo/bin:$PATH" && cd src-tauri && cargo test adapter::workbuddy 2>&1 | tail -5`
预期：新增 13 个测试全 PASS（先写测试→编译失败=RED→再实现→GREEN 的顺序可自行掌握，但报告需含 TDD 证据）。
然后 `cargo check --all-targets` 零警告。

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/adapter/
git -c user.name="xiangsir" -c user.email="xiangsir@local" commit -m "feat: workbuddy 真实适配器（本机登录态提取+惰性刷新+签到接口）"
```

---

### Task 2: 引擎放行空 token + 工厂接线 + 全量验证

**Files:**
- Modify: `src-tauri/src/engine.rs`（`run_once` 的缺凭证分支 + 新测试）
- Modify: `src-tauri/src/commands.rs`（`adapter_for` 工厂 + `sign_now` 接线）
- Modify: `src-tauri/src/lib.rs`（两处 MockAdapter 构造改工厂）

**Interfaces:**
- Consumes: Task 1 的 `WorkbuddyAdapter`、`ProductAdapter::accepts_empty_token()`
- Produces: `commands::adapter_for(product_id: &str) -> Box<dyn ProductAdapter>`

- [ ] **Step 1: engine.rs 写失败测试 + 改 run_once**

新测试（放 engine.rs tests 模块；需要 `use crate::adapter::{SignStatus, SignOutcome};` 已有则不重复）：

```rust
    struct AutoCred;
    #[async_trait::async_trait]
    impl ProductAdapter for AutoCred {
        fn id(&self) -> String { "auto".into() }
        fn accepts_empty_token(&self) -> bool { true }
        async fn query_sign_status(&self, token: &str) -> Result<SignStatus, AdapterError> {
            Ok(if token.is_empty() { SignStatus::NotSigned } else { SignStatus::SignedToday })
        }
        async fn sign_in(&self, _t: &str) -> Result<SignOutcome, AdapterError> {
            Ok(SignOutcome::Success("ok".into()))
        }
    }

    #[tokio::test]
    async fn empty_token_passes_for_auto_extract_adapters() {
        let (n, _rec) = RecordingNotifier::new();
        let store = store();
        let cfg = ProductConfig { product_id: "auto".into(), mode: Mode::Auto,
            time_of_day: "09:00".into(), token: None };
        let r = run_once(&AutoCred, &cfg, &store, &n, 0).await;
        assert!(matches!(r, RunResult::Success(_)));
    }
```

把 `run_once` 中现有的缺凭证分支（engine.rs:44-48）替换为：

```rust
    let token = cfg.token.as_deref().unwrap_or("");
    if token.trim().is_empty() && !adapter.accepts_empty_token() {
        notify_both(notifier, id, "缺少凭证：请登录产品（自动提取）或在设置中粘贴 token");
        store.record_run(id, "failed", "缺少凭证").ok();
        return RunResult::Failed("缺少凭证".into());
    }
```

（现有 `missing_token_fails_with_notification` 测试用 MockAdapter（默认 false）应继续通过，不改它。）

- [ ] **Step 2: commands.rs 工厂 + lib.rs 接线**

commands.rs 追加（imports 补 `use crate::adapter::workbuddy::WorkbuddyAdapter; use crate::adapter::ProductAdapter;`）：

```rust
/// 工厂：workbuddy 走真实适配器（本机登录态自动提取）；设 SIGNDOCK_MOCK_URL 时一切走 mock（开发/演示）。
pub fn adapter_for(product_id: &str) -> Box<dyn ProductAdapter> {
    let force_mock = std::env::var("SIGNDOCK_MOCK_URL").is_ok();
    if !force_mock && product_id == "workbuddy" {
        Box::new(WorkbuddyAdapter::new())
    } else {
        Box::new(MockAdapter::new(product_id, mock_base_url()))
    }
}
```

`sign_now` 中 `let adapter = MockAdapter::new(&product_id, mock_base_url());` 改为 `let adapter = adapter_for(&product_id);`，`run_once(&adapter, ...)` 改 `run_once(adapter.as_ref(), ...)`。

lib.rs 两处（约 :46 与 :70）`adapter::mock::MockAdapter::new(id, commands::mock_base_url())` 均改为 `commands::adapter_for(id)`，相应 `engine::run_once(&adapter, ...)` 改 `engine::run_once(adapter.as_ref(), ...)`。
删除不再需要的 `use crate::adapter::mock::MockAdapter;`（若 commands.rs 内仍用于工厂则保留）。

- [ ] **Step 3: 全量验证**

```bash
export PATH="$HOME/.cargo/bin:$PATH" && cd src-tauri && cargo test 2>&1 | tail -3 && cargo check --all-targets 2>&1 | tail -3
```
预期：全部测试 PASS（约 39 个）、零警告。

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/engine.rs src-tauri/src/commands.rs src-tauri/src/lib.rs
git -c user.name="xiangsir" -c user.email="xiangsir@local" commit -m "feat: workbuddy 适配器接线（工厂+引擎放行空 token）"
```

- [ ] **Step 5:（控制者执行，非实现者）真实冒烟**

`npm run tauri dev` → 托盘「立即签到（全部）」→ workbuddy 结果应为「今日已签到」（本机 streak=8，接口幂等安全）；trae/qoder 维持网络错误。历史表格出现 alreadySigned。

---

### Task 3: workbuddy 积分快照（余额 / 当日消耗 / 当日到期）

**Files:**
- Modify: `src-tauri/src/adapter/mod.rs`（新增 `CreditsSnapshot` + trait 默认方法 `fetch_credits`）
- Modify: `src-tauri/src/adapter/workbuddy.rs`（resource_base 拆分 + 积分实现 + 7 个新测试）

**Interfaces:**
- Consumes: Task 1 的 `resolve_cred`/`headers_for`；recon `docs/recon/workbuddy.md`「积分/资源接口」节（域名规则/请求体/到期规则，全部实测 200）
- Produces:
  - `CreditsSnapshot { balance: f64, today_used: f64, expiring_today: f64, fetched_at_ms: i64 }`（serde camelCase 导出，前端字段 `balance/todayUsed/expiringToday/fetchedAtMs`）
  - trait 新方法 `async fn fetch_credits(&self, token: &str) -> Result<CreditsSnapshot, AdapterError>`（默认返回 `SchemaChanged("该产品暂不支持积分查询")`；WorkbuddyAdapter 覆写）——Task 4 的 `get_credits` 命令消费它
  - `WorkbuddyAdapter::with_resource_base(self, &str) -> Self`（测试/分档注入）

- [ ] **Step 1: mod.rs 加 CreditsSnapshot 与 trait 默认方法**

```rust
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreditsSnapshot {
    pub balance: f64,
    pub today_used: f64,
    pub expiring_today: f64,
    pub fetched_at_ms: i64,
}
```

trait `ProductAdapter` 内追加：

```rust
    /// 积分快照；未支持的产品返回 SchemaChanged（前端仅展示错误文本）。
    async fn fetch_credits(&self, _token: &str) -> Result<CreditsSnapshot, AdapterError> {
        Err(AdapterError::SchemaChanged("该产品暂不支持积分查询".into()))
    }
```

- [ ] **Step 2: workbuddy.rs 追加常量、解析辅助与 7 个失败测试**

imports 改为：`use chrono::{Local, TimeZone, Utc};`；`use super::{AdapterError, CreditsSnapshot, ProductAdapter, SignOutcome, SignStatus};`；测试 mod 的 wiremock matchers 加 `body_partial_json`。

追加常量（数组/码表原样照抄 recon，勿改）：

```rust
pub const PROD_RESOURCE_BASE: &str = "https://www.workbuddy.cn";
const RESOURCE_SUMMARY_PATH: &str = "/billing/meter/get-user-resource-summary";
const RESOURCE_USAGE_PATH: &str = "/billing/meter/get-user-request-usage";
const RESOURCE_PAID_PATH: &str = "/billing/meter/get-user-resource-paid-packages";
const RESOURCE_FREE_PATH: &str = "/billing/meter/get-user-resource-free-packages";
/// DeductionEndTime 比 CycleEndTime 晚超 365 天 → 视为长期占位，改用 CycleEndTime（competitor credits.rs:194-215）
const EXPIRY_CYCLE_OVERRIDE_MS: i64 = 365 * 24 * 3600 * 1000;
/// 解析出的到期时间距今超 730 天 → 视为长期有效（None）
const FAR_FUTURE_EXPIRY_MS: i64 = 730 * 24 * 3600 * 1000;
const USAGE_PAGE_SIZE: u32 = 3000;
const USAGE_MAX_PAGES: u32 = 10;

const PAID_PACKAGE_CODES: &[&str] = &[
    "TCACA_code_002_AkiJS3ZHF5", "TCACA_code_023_4xbGhMrE6q",
    "TCACA_code_026_BaESVICNoi", "TCACA_code_027_0FCGVA6vSa",
    "TCACA_code_009_0XmEQc2xOf", "TCACA_code_038_OhvqZtiPKr",
    "TCACA_code_003_FAnt7lcmRT", "TCACA_code_036_lupO5WgNdG",
];
const FREE_PACKAGE_CODES: &[&str] = &[
    "TCACA_code_008_cfWoLwvjU4", "TCACA_code_007_nzdH5h4Nl0",
    "TCACA_code_028_NtpWi0jzXs", "TCACA_code_029_6wCGEWquYy",
    "TCACA_code_030_BjSt89qTvr", "TCACA_code_001_PqouKr6QWV",
    "TCACA_code_006_DbXS0lrypC", "TCACA_code_035_ArVxJcGDsm",
    "TCACA_code_037_WxOD3MpI2o", "TCACA_code_039_KRcQj7wUat",
    "TCACA_code_040_mi9rCYg46x",
];
```

追加辅助函数（移植自 competitor credits.rs:62-114,194-215，已按本仓库风格收紧）：

```rust
fn first_value<'a>(v: &'a Value, keys: &[&str]) -> Option<&'a Value> {
    keys.iter().find_map(|key| v.get(*key))
}

/// 数值字段既有 number 也有数字字符串（recon 实测），统一兜底
fn parse_number(v: Option<&Value>) -> Option<f64> {
    match v {
        Some(Value::Number(n)) => n.as_f64(),
        Some(Value::String(t)) => t.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn first_number(v: &Value, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| parse_number(v.get(*key)))
}

/// 毫秒数/秒数/RFC3339/"YYYY-MM-DD HH:MM:SS"（本地时区）全兼容
fn parse_timestamp_ms(v: Option<&Value>) -> Option<i64> {
    let v = v?;
    if let Some(n) = parse_number(Some(v)) {
        let millis = if n.abs() < 10_000_000_000.0 { n * 1000.0 } else { n };
        return Some(millis.round() as i64);
    }
    let text = v.as_str()?.trim();
    if text.is_empty() { return None; }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(text) {
        return Some(dt.timestamp_millis());
    }
    for fmt in ["%Y-%m-%d %H:%M:%S", "%Y-%m-%d %H:%M:%S%.f"] {
        if let Ok(nd) = chrono::NaiveDateTime::parse_from_str(text, fmt) {
            return Local.from_local_datetime(&nd).single().map(|d| d.timestamp_millis());
        }
    }
    chrono::NaiveDate::parse_from_str(text, "%Y-%m-%d").ok()
        .and_then(|d| d.and_hms_opt(23, 59, 59))
        .and_then(|nd| Local.from_local_datetime(&nd).single())
        .map(|d| d.timestamp_millis())
}

/// 到期时间：deduction−cycle>365 天视为长期占位改用 cycle；结果距今>730 天视为长期有效
fn resolve_expire_at(raw: &Value, now_ms: i64) -> Option<i64> {
    let deduction = parse_timestamp_ms(first_value(raw,
        &["DeductionEndTime", "deductionEndTime", "ExpiredTime", "expiredTime"]));
    let cycle = parse_timestamp_ms(first_value(raw, &["CycleEndTime", "cycleEndTime"]));
    let expire_at = match (deduction, cycle) {
        (Some(d), Some(c)) if d.saturating_sub(c) > EXPIRY_CYCLE_OVERRIDE_MS => Some(c),
        (Some(d), _) => Some(d),
        (None, c) => c,
    };
    expire_at.filter(|v| v.saturating_sub(now_ms) <= FAR_FUTURE_EXPIRY_MS)
}

fn arrays_at<'a>(v: &'a Value, paths: &[&[&str]]) -> Vec<&'a Value> {
    for path in paths {
        let mut cur = v;
        for key in *path {
            match cur.get(*key) { Some(n) => cur = n, None => { cur = &Value::Null; break } }
        }
        if let Some(items) = cur.as_array() { return items.iter().collect(); }
    }
    Vec::new()
}
fn accounts_of(v: &Value) -> Vec<&Value> { arrays_at(v, &[&["data", "Accounts"], &["data", "data", "Accounts"]]) }
fn packages_of(v: &Value) -> Vec<&Value> { arrays_at(v, &[&["data", "Packages"], &["data", "data", "Packages"]]) }

fn check_biz(v: &Value, api_path: &str) -> Result<(), AdapterError> {
    let code = v.get("code").and_then(Value::as_i64).unwrap_or(-1);
    if code == 0 || code == 200 { return Ok(()); }
    Err(AdapterError::SchemaChanged(format!("{api_path} code={code} msg={}", msg_of(v))))
}

fn is_local_today(ms: i64, now: &chrono::DateTime<Local>) -> bool {
    Local.timestamp_millis_opt(ms).single()
        .is_some_and(|dt| dt.date_naive() == now.date_naive())
}
```

新测试（7 个，放 workbuddy.rs tests；`adapter(&s, cred)` 复用现有 helper——`with_base_url` 会把两个 base 都指向 mock server，单 server 同时服务 meter 与 resource 路径）：

```rust
    async fn mount_credits_defaults(s: &MockServer) {
        Mock::given(method("POST")).and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"Packages":[]}})))
            .mount(s).await;
        Mock::given(method("POST")).and(path(RESOURCE_USAGE_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"total":0,"data":[]}})))
            .mount(s).await;
        Mock::given(method("POST")).and(path(RESOURCE_PAID_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"Accounts":[]}})))
            .mount(s).await;
        Mock::given(method("POST")).and(path(RESOURCE_FREE_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"Accounts":[]}})))
            .mount(s).await;
    }
    fn days_from_now(offset: i64) -> String {
        (Local::now() + chrono::Duration::days(offset)).format("%Y-%m-%d %H:%M:%S").to_string()
    }

    #[test]
    fn parse_timestamp_accepts_number_ms_sec_and_naive_string() {
        assert_eq!(parse_timestamp_ms(Some(&json!(1700000000000i64))), Some(1700000000000));
        assert_eq!(parse_timestamp_ms(Some(&json!(1700000000))), Some(1700000000000));
        let parsed = parse_timestamp_ms(Some(&json!("2026-09-25 10:00:00")));
        assert_eq!(parsed, Local.with_ymd_and_hms(2026, 9, 25, 10, 0, 0).unwrap().timestamp_millis());
    }

    #[test]
    fn resolve_expire_prefers_cycle_when_deduction_is_far_placeholder() {
        let raw = json!({"DeductionEndTime": 2493072000000i64, "CycleEndTime": days_from_now(2)});
        let ms = resolve_expire_at(&raw, Utc::now().timestamp_millis()).unwrap();
        let dt = Local.timestamp_millis_opt(ms).single().unwrap();
        assert_eq!(dt.date_naive(), (Local::now() + chrono::Duration::days(2)).date_naive());
        // 双长期 → None
        let far = json!({"DeductionEndTime": 2493072000000i64, "CycleEndTime": 2493072000000i64});
        assert_eq!(resolve_expire_at(&far, Utc::now().timestamp_millis()), None);
    }

    #[tokio::test]
    async fn credits_balance_sums_string_and_number_capacities() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"Packages":[
                {"PackageCode":"a","CycleRemainCapacity":"7231.57"},
                {"PackageCode":"b","CycleRemainCapacity":500}]}})))
            .mount(&s).await;
        mount_credits_defaults(&s).await;
        let cred = write_cred("cbal", "AT", "RT", future_ms());
        let snap = adapter(&s, cred.clone()).fetch_credits("").await.unwrap();
        std::fs::remove_file(&cred).ok();
        assert!((snap.balance - 7731.57).abs() < 1e-6);
        assert!((snap.today_used - 0.0).abs() < 1e-9);
    }

    #[tokio::test]
    async fn credits_today_used_paginates_until_total() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(RESOURCE_USAGE_PATH))
            .and(body_partial_json(json!({"pageNum":1})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(
                {"code":0,"data":{"total":3,"data":[{"credit":1.5},{"credit":"2.5"}]}})))
            .mount(&s).await;
        Mock::given(method("POST")).and(path(RESOURCE_USAGE_PATH))
            .and(body_partial_json(json!({"pageNum":2})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(
                {"code":0,"data":{"total":3,"data":[{"credit":"0.5"}]}})))
            .mount(&s).await;
        mount_credits_defaults(&s).await;
        let cred = write_cred("cpg", "AT", "RT", future_ms());
        let snap = adapter(&s, cred.clone()).fetch_credits("").await.unwrap();
        std::fs::remove_file(&cred).ok();
        assert!((snap.today_used - 4.5).abs() < 1e-6);
    }

    #[tokio::test]
    async fn credits_expiring_today_counts_only_accounts_maturing_today() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(RESOURCE_PAID_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"Accounts":[
                {"PackageName":"p1","CycleEndTime":days_from_now(0),"CycleCapacityRemainPrecise":"90.57"},
                {"PackageName":"p2","DeductionEndTime":2493072000000i64,
                 "CycleEndTime":"2049-01-01 00:00:00","CycleCapacityRemain":"500"}]}})))
            .mount(&s).await;
        Mock::given(method("POST")).and(path(RESOURCE_FREE_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"Accounts":[
                {"PackageName":"f1","CycleEndTime":days_from_now(1),"CycleCapacityRemain":"100"}]}})))
            .mount(&s).await;
        mount_credits_defaults(&s).await;
        let cred = write_cred("cexp", "AT", "RT", future_ms());
        let snap = adapter(&s, cred.clone()).fetch_credits("").await.unwrap();
        std::fs::remove_file(&cred).ok();
        assert!((snap.expiring_today - 90.57).abs() < 1e-6);
    }

    #[tokio::test]
    async fn credits_401_is_auth_expired() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(401)).mount(&s).await;
        let cred = write_cred("c401", "AT", "RT", future_ms());
        assert!(matches!(adapter(&s, cred.clone()).fetch_credits("").await,
            Err(AdapterError::AuthExpired)));
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn credits_business_error_code_is_schema_changed() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":10001,"msg":"PackageCodes required"})))
            .mount(&s).await;
        mount_credits_defaults(&s).await;
        let cred = write_cred("cbiz", "AT", "RT", future_ms());
        assert!(matches!(adapter(&s, cred.clone()).fetch_credits("").await,
            Err(AdapterError::SchemaChanged(_))));
        std::fs::remove_file(&cred).ok();
    }
```

- [ ] **Step 3: 实现 resource_base 拆分与 fetch_credits**

struct 加字段 `resource_base: String`；构造改造（`with_base_url` 同步设置两个 base，保证 wiremock 单 server 全覆盖）：

```rust
    pub fn new() -> Self {
        Self::with_base_url(PROD_BASE).with_resource_base(PROD_RESOURCE_BASE)
    }

    pub fn with_base_url(base_url: &str) -> Self {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(20))
            .user_agent(UA)
            .build().unwrap_or_default();
        let base = base_url.trim_end_matches('/').to_string();
        Self { base_url: base.clone(), resource_base: base, client, cred_path: None }
    }

    pub fn with_resource_base(mut self, base: &str) -> Self {
        self.resource_base = base.trim_end_matches('/').to_string();
        self
    }
```

`post_meter` 泛化为（鉴权/401|403/非 JSON 语义不变，签到路径零行为变化）：

```rust
    async fn post_meter(&self, api_path: &str, cred: &WorkbuddyCred) -> Result<Value, AdapterError> {
        self.post_json(&self.base_url.clone(), api_path, cred, json!({})).await
    }

    async fn post_json(&self, base: &str, api_path: &str, cred: &WorkbuddyCred, body: Value) -> Result<Value, AdapterError> {
        let resp = self.client
            .post(format!("{base}{api_path}"))
            .headers(self.headers_for(cred)).json(&body).send().await?;
        // workbuddy 无验证码机制：401/403 一律按鉴权失败（决策见计划 Global Constraints）
        if matches!(resp.status().as_u16(), 401 | 403) {
            return Err(AdapterError::AuthExpired);
        }
        let v: Value = resp.json().await
            .map_err(|_| AdapterError::SchemaChanged(format!("{api_path} 返回非 JSON")))?;
        Ok(v)
    }
```

`impl ProductAdapter for WorkbuddyAdapter` 内覆写：

```rust
    async fn fetch_credits(&self, token: &str) -> Result<CreditsSnapshot, AdapterError> {
        let cred = self.resolve_cred(token).await?;
        let now = Local::now();
        let now_ms = now.timestamp_millis();
        let day_start = now.format("%Y-%m-%d 00:00:00").to_string();
        let day_end = now.format("%Y-%m-%d 23:59:59").to_string();

        // 余额：summary 的 Σ CycleRemainCapacity（recon 实测 7731.57 = 7231.57+500）
        let summary = self.post_json(&self.resource_base, RESOURCE_SUMMARY_PATH, &cred, json!({})).await?;
        check_biz(&summary, RESOURCE_SUMMARY_PATH)?;
        let balance = packages_of(&summary).iter().fold(0.0f64, |acc, p| {
            acc + first_number(p, &["CycleRemainCapacity"]).unwrap_or(0.0)
        });

        // 当日消耗：usage 按 pageNum 翻页，直到采满 total / 空页 / 页数上限
        let mut today_used = 0.0f64;
        let mut page: u32 = 1;
        let mut collected: i64 = 0;
        loop {
            let body = json!({ "startTime": day_start,
                "endTime": now.format("%Y-%m-%d %H:%M:%S").to_string(),
                "pageNum": page, "pageSize": USAGE_PAGE_SIZE });
            let v = self.post_json(&self.resource_base, RESOURCE_USAGE_PATH, &cred, body).await?;
            check_biz(&v, RESOURCE_USAGE_PATH)?;
            let data = v.get("data").cloned().unwrap_or(Value::Null);
            let items = data.get("data").and_then(Value::as_array).cloned().unwrap_or_default();
            for it in &items {
                today_used += first_number(it, &["credit", "Credit"]).unwrap_or(0.0);
            }
            collected += items.len() as i64;
            let total = parse_number(data.get("total")).unwrap_or(0.0) as i64;
            if items.is_empty() || collected >= total || page >= USAGE_MAX_PAGES { break; }
            page += 1;
        }

        // 当日到期：paid+free Accounts，resolve 后到期日==今天 的剩余量求和
        let mut expiring_today = 0.0f64;
        let queries = [
            (RESOURCE_PAID_PATH, json!({"PageNumber":1,"PageSize":200,"Status":[0,3],
                                        "PackageCodes":PAID_PACKAGE_CODES})),
            (RESOURCE_FREE_PATH, json!({"PageNumber":1,"PageSize":200,"Status":[0,3],
                                        "PackageCodes":FREE_PACKAGE_CODES,
                                        "SlicePeriodStartTime":day_start,"SlicePeriodEndTime":day_end})),
        ];
        for (path, body) in queries {
            let v = self.post_json(&self.resource_base, path, &cred, body).await?;
            check_biz(&v, path)?;
            for acc in accounts_of(&v) {
                let Some(ms) = resolve_expire_at(acc, now_ms) else { continue };
                if !is_local_today(ms, &now) { continue; }
                expiring_today += first_number(acc,
                    &["CycleCapacityRemainPrecise", "CycleCapacityRemain", "CycleRemainCapacity"])
                    .unwrap_or(0.0);
            }
        }
        Ok(CreditsSnapshot { balance, today_used, expiring_today, fetched_at_ms: now_ms })
    }
```

- [ ] **Step 4: 运行测试**

`export PATH="$HOME/.cargo/bin:$PATH" && cd src-tauri && cargo test adapter::workbuddy 2>&1 | grep "test result:"`
预期：workbuddy 模块 21 个测试全 PASS（报告需含 RED→GREEN 证据）。
然后 `cargo check --all-targets` 零警告。

- [ ] **Step 5: 提交**

```bash
git add src-tauri/src/adapter/
git -c user.name="xiangsir" -c user.email="xiangsir@local" commit -m "feat: workbuddy 积分快照（余额/当日消耗/当日到期）"
```

---

### Task 4: get_credits 命令 + 前端积分行

**Files:**
- Modify: `src-tauri/src/commands.rs`（新增 `get_credits` tauri 命令）
- Modify: `src-tauri/src/lib.rs:88-90`（invoke_handler 注册）
- Modify: `src/main.ts`（卡片积分行 + 刷新按钮）

**Interfaces:**
- Consumes: Task 3 的 `CreditsSnapshot`、trait `fetch_credits`（MockAdapter 走默认实现 → 前端显示失败文本）
- Produces: tauri 命令 `get_credits(product_id) -> CreditsSnapshot | string(err)`

- [ ] **Step 1: commands.rs 追加命令**

import 改为 `use crate::adapter::{CreditsSnapshot, ProductAdapter};`，文件末尾追加：

```rust
#[tauri::command]
pub async fn get_credits(store: State<'_>, product_id: String) -> Result<CreditsSnapshot, String> {
    let cfg = store.config(&product_id).map_err(|e| e.0)?;
    let adapter = adapter_for(&product_id);
    adapter.fetch_credits(cfg.token.as_deref().unwrap_or("")).await.map_err(|e| e.to_string())
}
```

lib.rs `generate_handler![...]` 加 `commands::get_credits`。

- [ ] **Step 2: main.ts 积分 UI**

类型区加：

```ts
interface CreditsSnapshot { balance: number; todayUsed: number; expiringToday: number; fetchedAtMs: number; }
const fmtCredits = (n: number) => n.toLocaleString("zh-CN", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
```

卡片模板在现有 `<div class="row">…</div>`（保存/立即执行行）之后、`<table>` 之前插入：

```ts
      <div class="row">
        <button data-act="credits" data-id="${o.productId}">刷新积分</button>
        <span id="credits-${o.productId}" class="muted">积分：未查询</span>
      </div>
```

click 处理器追加分支：

```ts
  } else if (btn.dataset.act === "credits") {
    const span = document.getElementById(`credits-${id}`)!;
    const label = btn.textContent ?? "";
    btn.textContent = "查询中…";
    try {
      const c = await invoke<CreditsSnapshot>("get_credits", { productId: id });
      span.textContent = `余额 ${fmtCredits(c.balance)} · 今日消耗 ${fmtCredits(c.todayUsed)} · 今日到期 ${fmtCredits(c.expiringToday)}`;
    } catch (err) {
      span.textContent = `积分查询失败：${esc(String(err))}`;
    }
    btn.textContent = label;
  }
```

- [ ] **Step 3: 全量验证**

```bash
export PATH="$HOME/.cargo/bin:$PATH" && cd src-tauri && cargo test 2>&1 | grep "test result:" && cargo check --all-targets 2>&1 | tail -3
cd .. && npm run build 2>&1 | tail -5
```
预期：全部测试 PASS（约 46 个）、零警告；tsc+vite 构建成功。

- [ ] **Step 4: 提交**

```bash
git add src-tauri/src/commands.rs src-tauri/src/lib.rs src/main.ts
git -c user.name="xiangsir" -c user.email="xiangsir@local" commit -m "feat: get_credits 命令与积分面板（workbuddy 卡片刷新按钮）"
```

- [ ] **Step 5:（控制者执行，非实现者）真实冒烟**

`npm run tauri dev` → workbuddy 卡片点「刷新积分」→ 数值应与官网面板一致（本机实测基准：余额 7731.57、今日消耗/到期以当时面板为准）；trae/qoder 卡片显示「积分查询失败：该产品暂不支持积分查询」。

---

## Self-review 结论

- 覆盖：提取（T1）、刷新（T1）、接口映射（T1）、引擎放行（T2）、接线（T2）、积分快照三数（T3）、命令+UI（T4）——spec 方案 C 的 workbuddy 切片 + 用户新需求（当前积分/当日消耗/当日到期）闭环。
- 无占位符；类型一致：`CreditsSnapshot`/`fetch_credits` T3 定义、T4 消费；`with_base_url` 双 base 注入保证 wiremock 单 server 断言不变。
- trae/qoder 不在本计划内（等各自 recon）；其卡片积分按钮走 trait 默认错误分支，无需适配器。
