# SignDock 阶段0+1（侦察 + 托盘骨架）实施计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** 搭建 SignDock（Tauri 2 托盘工具）完整骨架——调度器、SQLite 存储、通知、设置界面——并用 MockAdapter 跑通"查状态→签到→记录→通知"全链路；同时完成三家产品签到机制的抓包侦察文档。

**Architecture:** Rust 后端承载全部逻辑（ProductAdapter trait + tokio 定时轮询 + rusqlite），Web 前端仅做配置展示，经 Tauri command 通信。真实产品适配器（阶段 2/3）在侦察文档产出后另行规划，本计划以 MockAdapter 验证架构。

**Tech Stack:** Tauri 2 + vanilla-ts 前端、Rust（tokio / reqwest / rusqlite bundled / async-trait / chrono / thiserror）、wiremock（dev）、tempfile（dev）。

**Spec:** `docs/superpowers/specs/2026-09-22-signdock-design.md`

## Global Constraints

- 平台：仅 Windows 10+；不使用管理员权限
- 依赖：SQLite 必须用 `rusqlite` 的 `bundled` feature（免外部 DLL）
- UI 文案：简体中文
- 凭证：仅存本机（SQLite 明文列，本阶段不做加密；README 需注明）
- 风控/验证码：适配器只上报 `NeedManual`，绝不绕过
- 提交：若本仓库未配置 git 身份，先询问用户名字/邮箱，用 `git config user.name/user.email`（仅本仓库）设置
- 随机延迟：签到时刻 ±10 分钟，按（product_id, 日期）确定性哈希，保证可测试
- 环境检查失败（缺 cargo/rustc/node 或 Rust MSVC 工具链）→ 停下报告，不静默安装系统依赖

## 文件结构（本计划完成后）

```
D:/Projects/SignDock/
├── .gitignore
├── package.json                 # create-tauri-app 脚手架
├── index.html / src/main.ts / src/styles.css   # 设置窗口前端
├── docs/
│   ├── recon/{workbuddy,trae,qoder}.md         # 侦察文档（任务8-10）
│   └── superpowers/specs/2026-09-22-signdock-design.md
└── src-tauri/
    ├── Cargo.toml / tauri.conf.json / build.rs / icons/
    └── src/
        ├── main.rs          # 入口，调 signdock_lib::run()
        ├── lib.rs           # Tauri Builder、托盘、调度循环装配
        ├── types.rs         # Mode / SignStatus / SignOutcome / ProductConfig / RunRow
        ├── adapter/mod.rs   # ProductAdapter trait + AdapterError
        ├── adapter/mock.rs  # MockAdapter（指向可配 base_url 的 HTTP 接口）
        ├── store.rs         # Store（SQLite，Mutex<Connection>）
        ├── scheduler.rs     # 纯函数：jitter_minutes / is_due
        ├── engine.rs        # run_once / run_with_retry 编排
        ├── notify.rs        # Notifier trait + TauriNotifier + RecordingNotifier
        └── commands.rs      # Tauri IPC：get_overview/set_config/sign_now/get_runs
```

---

### Task 1: 环境检查 + Tauri 脚手架

**Files:**
- Create: 项目根下 `package.json`、`index.html`、`src/`、`src-tauri/`（脚手架生成）
- Modify: `src-tauri/Cargo.toml`（加依赖）、`.gitignore`

**Interfaces:**
- Consumes: 无
- Produces: 可编译的空 Tauri 2 项目；crate 名 `signdock_lib`（`src-tauri/Cargo.toml` 的 `[lib] name`，模板默认 `signdock_lib`，若实际不同以实际为准并在全计划中替换）

- [ ] **Step 1: 检查工具链**

```bash
node --version && cargo --version && rustc --version
```
Expected: 三者都输出版本号（cargo/rustc 需存在且为 MSVC toolchain，Windows 上 `rustc -vV` 的 host 为 `x86_64-pc-windows-msvc`）。**失败则停止并报告用户，不得静默安装。**

- [ ] **Step 2: 脚手架到临时目录并合入**

```bash
cd /d/Projects && npm create tauri-app@latest signdock-scaffold -- --template vanilla-ts --manager npm --identifier com.signdock.app --yes
```
将生成内容（除 `.git` 外）移动到 `D:/Projects/SignDock/`，删除临时目录：
```bash
cp -r signdock-scaffold/. SignDock/ && rm -rf signdock-scaffold
```

- [ ] **Step 3: 添加依赖**

`src-tauri/Cargo.toml` 的 `[dependencies]` 追加（保留模板已有条目）：

```toml
[dependencies]
tauri = { version = "2", features = ["tray-icon"] }
tauri-plugin-notification = "2"
tokio = { version = "1", features = ["rt-multi-thread", "macros", "time", "sync"] }
reqwest = { version = "0.12", features = ["json"] }
rusqlite = { version = "0.32", features = ["bundled"] }
async-trait = "0.1"
chrono = { version = "0.4", features = ["serde"] }
thiserror = "2"

[dev-dependencies]
wiremock = "0.6"
tempfile = "3"
```

`tauri.conf.json` 的 `plugins` 若无该键则加：`"plugins": {}`（notification 无需窗口权限配置）。根目录 `.gitignore` 追加：

```
node_modules/
src-tauri/target/
dist/
*.db
```

- [ ] **Step 4: 安装并编译验证**

```bash
cd /d/Projects/SignDock && npm install && cargo check --manifest-path src-tauri/Cargo.toml
```
Expected: `cargo check` 通过（首编含插件下载，耐心等）。

- [ ] **Step 5: 冒烟：应用窗口能打开**

```bash
npm run tauri dev
```
Expected: 弹出空白模板窗口。人工确认后 Ctrl+C 停止。

- [ ] **Step 6: Commit**

```bash
git add -A && git commit -m "chore: Tauri 2 脚手架（vanilla-ts）与依赖"
```

---

### Task 2: 领域类型 + ProductAdapter trait + AdapterError

**Files:**
- Create: `src-tauri/src/types.rs`、`src-tauri/src/adapter/mod.rs`
- Modify: `src-tauri/src/lib.rs`（顶部加 `mod types; pub mod adapter;`——若 main.rs 为 bin-only 则模块放 lib 侧；模板已生成 lib.rs+main.rs 结构，直接加到 lib.rs）

**Interfaces:**
- Produces:
```rust
// types.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode { Off, Remind, Auto }

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductConfig {
    pub product_id: String,
    pub mode: Mode,
    pub time_of_day: String,          // "HH:MM"
    pub token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRow {
    pub id: i64,
    pub product_id: String,
    pub at: String,                    // RFC3339
    pub outcome: String,              // "success" | "alreadySigned" | "reminded" | "needManual" | "failed"
    pub detail: String,
}

// adapter/mod.rs
#[derive(Debug, Clone, PartialEq)]
pub enum SignStatus { SignedToday, NotSigned, Unknown }

#[derive(Debug, Clone, PartialEq)]
pub enum SignOutcome {
    Success(String),    // 详情，如 "+10积分"
    AlreadySigned,
    NeedManual(String), // 验证码/风控提示，转人工
    Failed(String),   // 业务性失败，不重试
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
```

（`id` 返回 `String`：产品 id 存在适配器字段中，`'static str` 不适用。）

- [ ] **Step 1: 写 types.rs 与 adapter/mod.rs（上述完整代码）**

- [ ] **Step 2: lib.rs 注册模块** `pub mod types; pub mod adapter;` 且 `adapter/mod.rs` 内暂只有 trait（mock 在 Task 3）

- [ ] **Step 3: 编译验证**

```bash
cargo check --manifest-path src-tauri/Cargo.toml
```
Expected: 通过（可能有 dead_code 警告，忽略，Task 6 装配后消除）。

- [ ] **Step 4: Commit** `git add src-tauri/src && git commit -m "feat: 领域类型与 ProductAdapter trait"`

---

### Task 3: MockAdapter + wiremock 集成测试

**Files:**
- Create: `src-tauri/src/adapter/mock.rs`
- Modify: `src-tauri/src/adapter/mod.rs`（加 `pub mod mock;`）

**Interfaces:**
- Consumes: Task 2 的 `ProductAdapter`、`SignStatus`、`SignOutcome`、`AdapterError`
- Produces: `MockAdapter::new(product_id: &str, base_url: String) -> MockAdapter`

HTTP 契约（阶段 2 起每家真实适配器由侦察文档定义，Mock 先行示范）：
- `GET {base}/sign/status`，Header `Authorization: Bearer <token>` → 200 `{"signed": bool}`；401 → 未授权
- `POST {base}/sign` → 200 `{"ok":true,"points":N}`；409 `{"signed":true}`；403 `{"captcha":true}`；401 未授权

- [ ] **Step 1: 写失败的集成测试** `src-tauri/src/adapter/mock.rs` 底部 `#[cfg(test)]`：

```rust
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
        assert_eq!(adapter_with(&s).query_sign_status("t").await.unwrap(), SignStatus::NotSigned);
    }

    #[tokio::test]
    async fn status_signed_today() {
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"signed": true})))
            .mount(&s).await;
        assert_eq!(adapter_with(&s).query_sign_status("t").await.unwrap(), SignStatus::SignedToday);
    }

    #[tokio::test]
    async fn status_401_is_auth_expired() {
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(401)).mount(&s).await;
        assert!(matches!(adapter_with(&s).query_sign_status("t").await, Err(AdapterError::AuthExpired)));
    }

    #[tokio::test]
    async fn sign_in_success_with_points() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path("/sign"))
            .and(header("authorization", "Bearer t"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "points": 10})))
            .mount(&s).await;
        let out = adapter_with(&s).sign_in("t").await.unwrap();
        assert!(matches!(out, SignOutcome::Success(d) if d.contains("10")));
    }

    #[tokio::test]
    async fn sign_in_409_already_signed() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path("/sign"))
            .respond_with(ResponseTemplate::new(409).set_body_json(serde_json::json!({"signed": true})))
            .mount(&s).await;
        assert_eq!(adapter_with(&s).sign_in("t").await.unwrap(), SignOutcome::AlreadySigned);
    }

    #[tokio::test]
    async fn sign_in_403_need_manual() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path("/sign"))
            .respond_with(ResponseTemplate::new(403).set_body_json(serde_json::json!({"captcha": true})))
            .mount(&s).await;
        assert!(matches!(adapter_with(&s).sign_in("t").await, Ok(SignOutcome::NeedManual(_))));
    }

    #[tokio::test]
    async fn unknown_schema_maps_to_schema_changed() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path("/sign"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"weird": 1})))
            .mount(&s).await;
        assert!(matches!(adapter_with(&s).sign_in("t").await, Err(AdapterError::SchemaChanged(_))));
    }
}
```

- [ ] **Step 2: 运行确认失败** `cargo test --manifest-path src-tauri/Cargo.toml adapter::mock 2>&1 | tail -5` Expected: 编译错误 `cannot find MockAdapter`

- [ ] **Step 3: 实现 MockAdapter**（`mock.rs` 测试之前部分）：

```rust
use super::{AdapterError, ProductAdapter, SignOutcome, SignStatus};

pub struct MockAdapter {
    product_id: String,
    base_url: String,
    client: reqwest::Client,
}

impl MockAdapter {
    pub fn new(product_id: &str, base_url: String) -> Self {
        Self { product_id: product_id.into(), base_url, client: reqwest::Client::new() }
    }
}

#[async_trait::async_trait]
impl ProductAdapter for MockAdapter {
    fn id(&self) -> String { self.product_id.clone() }

    async fn query_sign_status(&self, token: &str) -> Result<SignStatus, AdapterError> {
        let resp = self.client.get(format!("{}/sign/status", self.base_url))
            .bearer_auth(token).send().await?;
        match resp.status().as_u16() {
            200 => {
                let v: serde_json::Value = resp.json().await?;
                match v.get("signed").and_then(|s| s.as_bool()) {
                    Some(true) => Ok(SignStatus::SignedToday),
                    Some(false) => Ok(SignStatus::NotSigned),
                    None => Err(AdapterError::SchemaChanged("status 缺少 signed".into())),
                }
            }
            401 => Err(AdapterError::AuthExpired),
            code => Err(AdapterError::SchemaChanged(format!("status 意外状态码 {code}"))),
        }
    }

    async fn sign_in(&self, token: &str) -> Result<SignOutcome, AdapterError> {
        let resp = self.client.post(format!("{}/sign", self.base_url))
            .bearer_auth(token).send().await?;
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
```

`fn id()` 返回 `&'static str` 但 `product_id` 是字段时不能返回引用——将 trait 签名改为 `fn id(&self) -> String`，并同步修改 Task 2 定义与本实现；下文所有适配器同此。

- [ ] **Step 4: 运行确认通过** `cargo test --manifest-path src-tauri/Cargo.toml adapter::mock` Expected: 7 passed

- [ ] **Step 5: Commit** `git add src-tauri/src/adapter && git commit -m "feat: MockAdapter 与 wiremock 集成测试"`

---

### Task 4: Store（SQLite 配置 + 签到历史）

**Files:**
- Create: `src-tauri/src/store.rs`
- Modify: `src-tauri/src/lib.rs`（`pub mod store;`）

**Interfaces:**
- Consumes: Task 2 的 `ProductConfig`、`Mode`、`RunRow`
- Produces:

```rust
impl Store {
    pub fn open(path: &std::path::Path) -> Result<Store, StoreError>;
    pub fn in_memory() -> Result<Store, StoreError>;
    pub fn config(&self, product_id: &str) -> Result<ProductConfig, StoreError>; // 无记录时返回默认 Mode::Remind "09:00" token None 并落库
    pub fn set_config(&self, cfg: &ProductConfig) -> Result<(), StoreError>;
    pub fn record_run(&self, product_id: &str, outcome: &str, detail: &str) -> Result<(), StoreError>;
    pub fn last_success_at(&self, product_id: &str) -> Result<Option<i64>, StoreError>; // Unix 秒
    pub fn runs(&self, product_id: &str, limit: i64) -> Result<Vec<RunRow>, StoreError>;
}
pub struct StoreError(pub String); // impl Display + Error
```

- [ ] **Step 1: 写失败的单元测试**（`store.rs` 底部）：

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_is_remind_0900() {
        let s = Store::in_memory().unwrap();
        let cfg = s.config("workbuddy").unwrap();
        assert_eq!(cfg.mode, Mode::Remind);
        assert_eq!(cfg.time_of_day, "09:00");
        assert_eq!(cfg.token, None);
    }

    #[test]
    fn set_and_read_config_roundtrip() {
        let s = Store::in_memory().unwrap();
        s.set_config(&ProductConfig {
            product_id: "trae".into(), mode: Mode::Auto,
            time_of_day: "08:30".into(), token: Some("tok".into()),
        }).unwrap();
        let cfg = s.config("trae").unwrap();
        assert_eq!(cfg.mode, Mode::Auto);
        assert_eq!(cfg.time_of_day, "08:30");
        assert_eq!(cfg.token.as_deref(), Some("tok"));
    }

    #[test]
    fn last_success_none_then_some() {
        let s = Store::in_memory().unwrap();
        assert_eq!(s.last_success_at("qoder").unwrap(), None);
        s.record_run("qoder", "success", "+10积分").unwrap();
        assert!(s.last_success_at("qoder").unwrap().is_some());
    }

    #[test]
    fn failed_run_is_not_success() {
        let s = Store::in_memory().unwrap();
        s.record_run("qoder", "failed", "网络错误").unwrap();
        assert_eq!(s.last_success_at("qoder").unwrap(), None);
    }

    #[test]
    fn runs_desc_with_limit() {
        let s = Store::in_memory().unwrap();
        s.record_run("trae", "success", "a").unwrap();
        s.record_run("trae", "reminded", "b").unwrap();
        let rows = s.runs("trae", 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].outcome, "reminded"); // 新的在前
    }
}
```

- [ ] **Step 2: 运行确认失败** `cargo test --manifest-path src-tauri/Cargo.toml store:: 2>&1 | tail -3` Expected: 编译错误（Store 未定义）

- [ ] **Step 3: 实现 store.rs**：

```rust
use crate::types::{Mode, ProductConfig, RunRow};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug)]
pub struct StoreError(pub String);
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.0) }
}
impl std::error::Error for StoreError {}

pub struct Store { conn: Mutex<Connection> }

fn mode_to_str(m: Mode) -> &'static str {
    match m { Mode::Off => "off", Mode::Remind => "remind", Mode::Auto => "auto" }
}
fn mode_from_str(s: &str) -> Mode {
    match s { "off" => Mode::Off, "auto" => Mode::Auto, _ => Mode::Remind }
}

impl Store {
    fn from_conn(conn: Connection) -> Result<Store, StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS product_config(
                product_id TEXT PRIMARY KEY, mode TEXT NOT NULL,
                time_of_day TEXT NOT NULL, token TEXT);
             CREATE TABLE IF NOT EXISTS sign_run(
                id INTEGER PRIMARY KEY AUTOINCREMENT, product_id TEXT NOT NULL,
                at INTEGER NOT NULL, outcome TEXT NOT NULL, detail TEXT NOT NULL);"
        ).map_err(|e| StoreError(e.to_string()))?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(d) = path.parent() { std::fs::create_dir_all(d).map_err(|e| StoreError(e.to_string()))?; }
        Self::from_conn(Connection::open(path).map_err(|e| StoreError(e.to_string()))?)
    }

    pub fn in_memory() -> Result<Store, StoreError> {
        Self::from_conn(Connection::open_in_memory().map_err(|e| StoreError(e.to_string()))?)
    }

    pub fn config(&self, product_id: &str) -> Result<ProductConfig, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT mode, time_of_day, token FROM product_config WHERE product_id=?1")
            .map_err(|e| StoreError(e.to_string()))?;
        if let Some(row) = stmt.query_row(params![product_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
        }).ok() {
            return Ok(ProductConfig { product_id: product_id.into(), mode: mode_from_str(&row.0), time_of_day: row.1, token: row.2 });
        }
        let def = ProductConfig { product_id: product_id.into(), mode: Mode::Remind, time_of_day: "09:00".into(), token: None };
        conn.execute("INSERT INTO product_config(product_id, mode, time_of_day, token) VALUES(?1,?2,?3,?4)",
            params![def.product_id, mode_to_str(def.mode), def.time_of_day, def.token])
            .map_err(|e| StoreError(e.to_string()))?;
        Ok(def)
    }

    pub fn set_config(&self, cfg: &ProductConfig) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO product_config(product_id, mode, time_of_day, token) VALUES(?1,?2,?3,?4)
             ON CONFLICT(product_id) DO UPDATE SET mode=?2, time_of_day=?3, token=?4",
            params![cfg.product_id, mode_to_str(cfg.mode), cfg.time_of_day, cfg.token])
            .map_err(|e| StoreError(e.to_string()))?;
        Ok(())
    }

    pub fn record_run(&self, product_id: &str, outcome: &str, detail: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("INSERT INTO sign_run(product_id, at, outcome, detail) VALUES(?1,?2,?3,?4)",
            params![product_id, chrono::Utc::now().timestamp(), outcome, detail])
            .map_err(|e| StoreError(e.to_string()))?;
        Ok(())
    }

    pub fn last_success_at(&self, product_id: &str) -> Result<Option<i64>, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT MAX(at) FROM sign_run WHERE product_id=?1 AND outcome='success'",
            params![product_id], |r| r.get::<_, Option<i64>>(0))
            .map_err(|e| StoreError(e.to_string()))
    }

    pub fn runs(&self, product_id: &str, limit: i64) -> Result<Vec<RunRow>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, product_id, at, outcome, detail FROM sign_run WHERE product_id=?1 ORDER BY at DESC LIMIT ?2")
            .map_err(|e| StoreError(e.to_string()))?;
        let rows = stmt.query_map(params![product_id, limit], |r| {
            Ok(RunRow {
                id: r.get(0)?, product_id: r.get(1)?,
                at: chrono::DateTime::<chrono::Utc>::from_timestamp(r.get::<_, i64>(2)?, 0)
                    .map(|d| d.to_rfc3339()).unwrap_or_default(),
                outcome: r.get(3)?, detail: r.get(4)?,
            })
        }).map_err(|e| StoreError(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| StoreError(e.to_string()))
    }
}
```

注意 `chrono` 在本 crate 已有（Task 1 依赖）。

- [ ] **Step 4: 运行确认通过** `cargo test --manifest-path src-tauri/Cargo.toml store::` Expected: 5 passed

- [ ] **Step 5: Commit** `git add src-tauri/src && git commit -m "feat: SQLite Store（配置与签到历史）"`

---

### Task 5: 调度器纯逻辑（jitter + is_due）

**Files:**
- Create: `src-tauri/src/scheduler.rs`
- Modify: `src-tauri/src/lib.rs`（`pub mod scheduler;`）

**Interfaces:**
- Consumes: `ProductConfig`、`Mode`
- Produces:

```rust
pub fn jitter_minutes(product_id: &str, day: chrono::NaiveDate) -> i64;        // -10..=10，确定性
pub fn is_due(now: chrono::DateTime<chrono::Local>, product_id: &str,
              cfg: &ProductConfig, last_success_at: Option<i64>) -> bool;
```

- [ ] **Step 1: 写失败的单元测试**：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Mode, ProductConfig};
    use chrono::{Local, TimeZone};

    fn cfg(mode: Mode, time: &str) -> ProductConfig {
        ProductConfig { product_id: "workbuddy".into(), mode, time_of_day: time.into(), token: None }
    }
    fn at(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> chrono::DateTime<Local> {
        Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap()
    }

    #[test]
    fn jitter_is_deterministic_in_range() {
        let day = chrono::NaiveDate::from_ymd_opt(2026, 9, 22).unwrap();
        let a = jitter_minutes("workbuddy", day);
        let b = jitter_minutes("workbuddy", day);
        assert_eq!(a, b);
        assert!((-10..=10).contains(&a));
    }

    #[test]
    fn off_mode_never_due() {
        assert!(!is_due(at(2026, 9, 22, 9, 0), "workbuddy", &cfg(Mode::Off, "09:00"), None));
    }

    #[test]
    fn not_due_before_schedule() {
        // jitter 最大 +10：08:40 时任何 jitter 都未到期
        assert!(!is_due(at(2026, 9, 22, 8, 40), "workbuddy", &cfg(Mode::Auto, "09:00"), None));
    }

    #[test]
    fn due_after_schedule_when_never_signed() {
        // jitter 最小 -10：09:15 时任何 jitter 都已到期
        assert!(is_due(at(2026, 9, 22, 9, 15), "workbuddy", &cfg(Mode::Auto, "09:00"), None));
    }

    #[test]
    fn not_due_if_success_today() {
        let today_ts = at(2026, 9, 22, 9, 20).timestamp();
        assert!(!is_due(at(2026, 9, 22, 9, 30), "workbuddy", &cfg(Mode::Auto, "09:00"), Some(today_ts)));
    }

    #[test]
    fn due_again_next_day_even_if_signed_yesterday() {
        let yesterday = at(2026, 9, 21, 9, 20).timestamp();
        assert!(is_due(at(2026, 9, 22, 9, 15), "workbuddy", &cfg(Mode::Remind, "09:00"), Some(yesterday)));
    }
}
```

- [ ] **Step 2: 运行确认失败** Expected: 编译错误（模块不存在）

- [ ] **Step 3: 实现 scheduler.rs**：

```rust
use crate::types::{Mode, ProductConfig};
use chrono::{DateTime, Local, NaiveDate, Timelike};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

pub fn jitter_minutes(product_id: &str, day: NaiveDate) -> i64 {
    let mut h = DefaultHasher::new();
    product_id.hash(&mut h);
    day.hash(&mut h);
    (h.finish() % 21) as i64 - 10
}

pub fn is_due(now: DateTime<Local>, product_id: &str, cfg: &ProductConfig, last_success_at: Option<i64>) -> bool {
    if cfg.mode == Mode::Off { return false; }
    let mut parts = cfg.time_of_day.split(':');
    let (Ok(h), Ok(m)) = (parts.next().and_then(|s| s.parse::<u32>().ok()), parts.next().and_then(|s| s.parse::<u32>().ok())) else {
        return false;
    };
    let today = now.date_naive();
    if let Some(ts) = last_success_at {
        if DateTime::<Local>::from_timestamp(ts, 0)
            .map(|d| d.date_naive() == today).unwrap_or(false) { return false; }
    }
    let Some(scheduled) = today.and_hms_opt(h, m, 0) else { return false };
    let scheduled = scheduled + chrono::Duration::minutes(jitter_minutes(product_id, today));
    let Some(now_naive) = now.to_local().naive_local().and_then(|_| Some(now.naive_local())) else { return false };
    now_naive >= scheduled
}
```

（`DateTime::from_timestamp` 为 `chrono::DateTime::from_timestamp`，需 `use chrono::TimeZone;`）

- [ ] **Step 4: 运行确认通过** `cargo test --manifest-path src-tauri/Cargo.toml scheduler::` Expected: 6 passed

- [ ] **Step 5: Commit** `git add src-tauri/src && git commit -m "feat: 调度纯逻辑（确定性 jitter 与到期判断）"`

---

### Task 6: 引擎编排 run_once / run_with_retry

**Files:**
- Create: `src-tauri/src/engine.rs`、`src-tauri/src/notify.rs`
- Modify: `src-tauri/src/lib.rs`（`pub mod engine; pub mod notify;`）

**Interfaces:**
- Consumes: `ProductAdapter`（Task 2/3）、`Store`（Task 4）、`is_due`（Task 5）
- Produces:

```rust
// notify.rs
pub trait Notifier: Send + Sync { fn notify(&self, title: &str, body: &str); }
pub struct RecordingNotifier;              // 测试用，记录 Vec<(String,String)>，构造时给定 Arc<Mutex<Vec<..>>> 内部共享
impl RecordingNotifier { pub fn new() -> (Self, std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>); }

// engine.rs
#[derive(Debug, PartialEq)]
pub enum RunResult { Skipped, AlreadySigned, Success(String), Reminded, NeedManual(String), Failed(String) }

pub async fn run_once(
    adapter: &dyn ProductAdapter, cfg: &ProductConfig,
    store: &Store, notifier: &dyn Notifier, now_epoch: i64,
) -> RunResult;

pub fn should_retry(err: &AdapterError, attempt: u32, max_retries: u32) -> bool; // AuthExpired/SchemaChanged 不重试
```

`run_once` 流程（对应 spec §4）：无 token → `Failed` 且通知"缺少凭证"；`query_sign_status` 得 `SignedToday` → 记录 success 语义的 `AlreadySigned`（outcome 记 `"alreadySigned"`）返回；`Mode::Remind` → 通知"该签到了"、记 `"reminded"`；`Mode::Auto` → `sign_in` 按映射处理并 `record_run`。

- [ ] **Step 1: 写失败的单元测试**（engine.rs 底部，用 MockAdapter + wiremock 真发请求）：

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::mock::MockAdapter;
    use crate::store::Store;
    use crate::types::{Mode, ProductConfig};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(mode: Mode) -> ProductConfig {
        ProductConfig { product_id: "workbuddy".into(), mode, time_of_day: "09:00".into(), token: Some("tok".into()) }
    }
    fn store() -> Store { Store::in_memory().unwrap() }

    #[tokio::test]
    async fn remind_mode_notifies_without_signing() {
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"signed": false})))
            .mount(&s).await;
        let (n, rec) = RecordingNotifier::new();
        let a = MockAdapter::new("workbuddy", s.uri());
        let store = store();
        let r = run_once(&a, &cfg(Mode::Remind), &store, &n, 0).await;
        assert_eq!(r, RunResult::Reminded);
        assert_eq!(store.runs("workbuddy", 10).unwrap().len(), 1);
        assert_eq!(store.runs("workbuddy", 10).unwrap()[0].outcome, "reminded");
        assert_eq!(rec.lock().unwrap().len(), 1);
    }
}
```

补充以下用例（与上面同一 `mod tests`，写法一致）：

```rust
    #[tokio::test]
    async fn auto_mode_signs_in_and_records_success() {
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"signed": false})))
            .mount(&s).await;
        Mock::given(method("POST")).and(path("/sign"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "points": 5})))
            .mount(&s).await;
        let (n, rec) = RecordingNotifier::new();
        let a = MockAdapter::new("workbuddy", s.uri());
        let store = store();
        let r = run_once(&a, &cfg(Mode::Auto), &store, &n, 0).await;
        assert!(matches!(&r, RunResult::Success(d) if d.contains('5')));
        assert_eq!(store.runs("workbuddy", 10).unwrap()[0].outcome, "success");
        assert_eq!(rec.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn signed_today_short_circuits() {
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"signed": true})))
            .mount(&s).await;
        let (n, _rec) = RecordingNotifier::new();
        let a = MockAdapter::new("workbuddy", s.uri());
        let store = store();
        let r = run_once(&a, &cfg(Mode::Auto), &store, &n, 0).await;
        assert_eq!(r, RunResult::AlreadySigned);
        assert_eq!(store.last_success_at("workbuddy").unwrap(), None); // 已签≠本次成功记录
    }

    #[tokio::test]
    async fn missing_token_fails_with_notification() {
        let (n, rec) = RecordingNotifier::new();
        let s = MockServer::start().await;
        let a = MockAdapter::new("workbuddy", s.uri());
        let cfg = ProductConfig { product_id: "workbuddy".into(), mode: Mode::Auto, time_of_day: "09:00".into(), token: None };
        let store = store();
        let r = run_once(&a, &cfg, &store, &n, 0).await;
        assert!(matches!(r, RunResult::Failed(_)));
        assert_eq!(rec.lock().unwrap().len(), 1);
    }

    #[test]
    fn should_retry_policy() {
        use crate::adapter::AdapterError;
        assert!(!should_retry(&AdapterError::AuthExpired, 0, 2));
        assert!(!should_retry(&AdapterError::SchemaChanged("x".into()), 0, 2));
        let http_err: reqwest::Error = {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async { reqwest::Client::new().get("http://127.0.0.1:1/x").send().await.unwrap_err() })
        };
        assert!(should_retry(&AdapterError::Http(http_err), 0, 2));
        assert!(!should_retry(&AdapterError::Http(http_err), 2, 2));
    }
```

- [ ] **Step 2: 运行确认失败**（未定义符号）。Expected: 编译错误

- [ ] **Step 3: 实现 notify.rs**：

```rust
pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str);
}

pub struct RecordingNotifier {
    sink: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

impl RecordingNotifier {
    pub fn new() -> (Self, std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>) {
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (Self { sink: sink.clone() }, sink)
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, title: &str, body: &str) {
        self.sink.lock().unwrap().push((title.into(), body.into()));
    }
}
```

（TauriNotifier 在 Task 7 加，避免测试依赖 AppHandle。）

- [ ] **Step 4: 实现 engine.rs**（测试之前部分）：

```rust
use crate::adapter::{AdapterError, ProductAdapter, SignOutcome, SignStatus};
use crate::notify::Notifier;
use crate::store::Store;
use crate::types::{Mode, ProductConfig};

#[derive(Debug, PartialEq)]
pub enum RunResult {
    Skipped,
    AlreadySigned,
    Success(String),
    Reminded,
    NeedManual(String),
    Failed(String),
}

pub fn should_retry(err: &AdapterError, attempt: u32, max_retries: u32) -> bool {
    match err {
        AdapterError::Http(_) => attempt < max_retries,
        AdapterError::AuthExpired | AdapterError::SchemaChanged(_) => false,
    }
}

fn notify_both(n: &dyn Notifier, product_id: &str, body: &str) {
    n.notify(&format!("SignDock · {product_id}"), body);
}

pub async fn run_once(
    adapter: &dyn ProductAdapter,
    cfg: &ProductConfig,
    store: &Store,
    notifier: &dyn Notifier,
    _now_epoch: i64,
) -> RunResult {
    let id = cfg.product_id.as_str();
    if cfg.mode == Mode::Off { return RunResult::Skipped; }
    let Some(token) = cfg.token.as_deref() else {
        notify_both(notifier, id, "缺少凭证：请在设置中粘贴 token 或等待本机提取支持");
        store.record_run(id, "failed", "缺少凭证").ok();
        return RunResult::Failed("缺少凭证".into());
    };
    let status = match adapter.query_sign_status(token).await {
        Ok(s) => s,
        Err(e) => {
            let msg = e.to_string();
            notify_both(notifier, id, &format!("查询签到状态失败：{msg}"));
            store.record_run(id, "failed", &msg).ok();
            return RunResult::Failed(msg);
        }
    };
    if status == SignStatus::SignedToday {
        store.record_run(id, "alreadySigned", "今日已签到").ok();
        return RunResult::AlreadySigned;
    }
    if cfg.mode == Mode::Remind {
        notify_both(notifier, id, "该签到了（今日未签到）");
        store.record_run(id, "reminded", "提醒已发出").ok();
        return RunResult::Reminded;
    }
    match adapter.sign_in(token).await {
        Ok(SignOutcome::Success(detail)) => {
            notify_both(notifier, id, &format!("签到成功 {detail}"));
            store.record_run(id, "success", &detail).ok();
            RunResult::Success(detail)
        }
        Ok(SignOutcome::AlreadySigned) => {
            store.record_run(id, "alreadySigned", "今日已签到").ok();
            RunResult::AlreadySigned
        }
        Ok(SignOutcome::NeedManual(msg)) => {
            notify_both(notifier, id, &format!("{msg}"));
            store.record_run(id, "needManual", &msg).ok();
            RunResult::NeedManual(msg)
        }
        Ok(SignOutcome::Failed(msg)) => {
            notify_both(notifier, id, &format!("签到失败：{msg}"));
            store.record_run(id, "failed", &msg).ok();
            RunResult::Failed(msg)
        }
        Err(e) => {
            let msg = e.to_string();
            notify_both(notifier, id, &format!("签到失败：{msg}"));
            store.record_run(id, "failed", &msg).ok();
            RunResult::Failed(msg)
        }
    }
}
```

- [ ] **Step 5: 运行确认通过** `cargo test --manifest-path src-tauri/Cargo.toml engine::` Expected: 5 passed

- [ ] **Step 6: Commit** `git add src-tauri/src && git commit -m "feat: 签到引擎（状态查询→模式分流→记录通知）"`

---

### Task 7: Tauri 装配——托盘、通知、调度循环、IPC

**Files:**
- Modify: `src-tauri/src/lib.rs`、Create: `src-tauri/src/commands.rs`（或并入 lib.rs，保持 ≤200 行为宜）
- Modify: `src-tauri/capabilities/default.json`（notification 权限）

**Interfaces:**
- Consumes: Task 2-6 全部
- Produces: 可运行托盘应用；IPC 命令 `get_overview` / `set_config` / `sign_now` / `get_runs`，product 列表常量 `PRODUCT_IDS: [&str; 3] = ["workbuddy", "trae", "qoder"]`

- [ ] **Step 1: capabilities 加通知权限** `src-tauri/capabilities/default.json` 的 `permissions` 数组追加 `"notification:default"`。

- [ ] **Step 2: notify.rs 追加 TauriNotifier**：

```rust
pub struct TauriNotifier(pub tauri::AppHandle);
impl Notifier for TauriNotifier {
    fn notify(&self, title: &str, body: &str) {
        use tauri_plugin_notification::NotificationExt;
        let _ = self.0.notification()
            .show_title_body(title, body);
    }
}
```

- [ ] **Step 3: commands.rs**：

```rust
use crate::adapter::mock::MockAdapter;
use crate::engine::{run_once, RunResult};
use crate::notify::TauriNotifier;
use crate::store::Store;
use crate::types::{ProductConfig, RunRow};
use std::sync::Arc;

pub const PRODUCT_IDS: [&str; 3] = ["workbuddy", "trae", "qoder"];

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    pub product_id: String,
    pub config: ProductConfig,
    pub runs: Vec<RunRow>,
}

type State = tauri::State<'_, Arc<Store>>;

#[tauri::command]
pub fn get_overview(store: State) -> Vec<Overview> {
    PRODUCT_IDS.iter().map(|id| Overview {
        product_id: id.to_string(),
        config: store.config(id).unwrap_or_else(|_| ProductConfig {
            product_id: id.to_string(), mode: crate::types::Mode::Remind,
            time_of_day: "09:00".into(), token: None,
        }),
        runs: store.runs(id, 20).unwrap_or_default(),
    }).collect()
}

#[tauri::command]
pub fn set_config(store: State, cfg: ProductConfig) -> Result<(), String> {
    store.set_config(&cfg).map_err(|e| e.0)
}

#[tauri::command]
pub fn get_runs(store: State, product_id: String) -> Vec<RunRow> {
    store.runs(&product_id, 50).unwrap_or_default()
}

#[tauri::command]
pub async fn sign_now(app: tauri::AppHandle, store: tauri::State<'_, Arc<Store>>, product_id: String) -> Result<RunResult, String> {
    let cfg = store.config(&product_id).map_err(|e| e.0)?;
    // base_url 留空：阶段 1 真实适配器未注册，MockAdapter 走本地不可达地址 → 网络错误路径，验证通知链路
    let adapter = MockAdapter::new(&product_id, std::env::var("SIGNDOCK_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:9".into()));
    let notifier = TauriNotifier(app);
    Ok(run_once(adapter.as_ref(), &cfg, &store, &notifier, chrono::Utc::now().timestamp()).await)
}
```

（`run_once` 接收 `&dyn ProductAdapter` + `&Store`：注意 `store` 是 `Arc`，解引用传 `&*store`。）

- [ ] **Step 4: lib.rs 装配**（替换模板 `run()`）：

```rust
pub mod adapter;
pub mod commands;
pub mod engine;
pub mod notify;
pub mod scheduler;
pub mod store;
pub mod types;

use std::sync::Arc;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::TrayIconBuilder;
            use tauri::{Manager, WebviewWindowBuilder, WebviewUrl};

            let data_dir = app.path().app_data_dir()?;
            let store = Arc::new(store::Store::open(&data_dir.join("signdock.db")).map_err(|e| e.0)?);
            app.manage(store.clone());

            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let sign_all = MenuItem::with_id(app, "sign_all", "立即签到（全部）", true, None::<&str>)?;
            let open_settings = MenuItem::with_id(app, "open", "打开设置", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_settings, &sign_all, &quit])?;
            TrayIconBuilder::with_id("main-tray")
                .menu(&menu)
                .tooltip("SignDock")
                .icon(app.default_window_icon().unwrap().clone())
                .on_menu_event(move |app, ev| match ev.id.as_ref() {
                    "quit" => app.exit(0),
                    "open" => {
                        if let Some(w) = app.get_webview_window("main") { let _ = w.show(); let _ = w.set_focus(); }
                        else { let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::default()).build(); }
                    }
                    "sign_all" => {
                        let app2 = app.clone();
                        let store2 = app.state::<Arc<store::Store>>().inner().clone();
                        tauri::async_runtime::spawn(async move {
                            for id in commands::PRODUCT_IDS {
                                let cfg = match store2.config(id) { Ok(c) => c, Err(_) => continue };
                                if cfg.mode == types::Mode::Off { continue; }
                                let adapter = adapter::mock::MockAdapter::new(id,
                                    std::env::var("SIGNDOCK_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:9".into()));
                                let _ = engine::run_once(adapter.as_ref(), &cfg, &store2,
                                    &notify::TauriNotifier(app2.clone()), chrono::Utc::now().timestamp()).await;
                            }
                        });
                    }
                    _ => {}
                })
                .build(app)?;

            // 调度循环：每 60s tick，对到期产品执行引擎
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                loop {
                    ticker.tick().await;
                    let store = app_handle.state::<Arc<store::Store>>().inner().clone();
                    let now = chrono::Local::now();
                    for id in commands::PRODUCT_IDS {
                        let Ok(cfg) = store.config(id) else { continue };
                        let last = store.last_success_at(id).ok().flatten();
                        if scheduler::is_due(now, id, &cfg, last) {
                            let adapter = adapter::mock::MockAdapter::new(id,
                                std::env::var("SIGNDOCK_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:9".into()));
                            let max_retries: u32 = 2;
                            let mut attempt: u32 = 0;
                            loop {
                                let outcome = engine::run_once(adapter.as_ref(), &cfg, &store,
                                    &notify::TauriNotifier(app_handle.clone()), chrono::Utc::now().timestamp()).await;
                                let retriable_net_err = matches!(&outcome, engine::RunResult::Failed(m) if m.starts_with("网络错误"));
                                if !retriable_net_err || attempt >= max_retries { break; }
                                attempt += 1;
                                tokio::time::sleep(std::time::Duration::from_secs(300)).await;
                            }
                        }
                    }
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_overview, commands::set_config, commands::sign_now, commands::get_runs
        ])
        .run(tauri::generate_context!())
        .expect("error while running SignDock");
}
```

注：模板 `main.rs` 保持调 `signdock_lib::run()`；删除模板 lib.rs 中的 `greet` 示例命令及其 capability 引用。

- [ ] **Step 5: 编译 + 全量测试** `cargo check --manifest-path src-tauri/Cargo.toml && cargo test --manifest-path src-tauri/Cargo.toml` Expected: check 通过、此前所有测试 passed

- [ ] **Step 6: 人工冒烟**（托盘链路无法自动化）`npm run tauri dev`，验证清单：① 托盘图标出现，右键中文菜单 ② "立即签到（全部）"→ 未配 token 时弹 toast"缺少凭证" ③ 在设置里配 `SIGNDOCK_MOCK_URL` 起 wiremock 不在人工范围，跳过 ④ 退出菜单可退出。

- [ ] **Step 7: Commit** `git add src-tauri && git commit -m "feat: 托盘、通知、调度循环与 IPC 装配"`

---

### Task 8: 设置窗口前端

**Files:**
- Modify: `index.html`、`src/main.ts`、`src/styles.css`（模板文件重写）

**Interfaces:**
- Consumes: IPC `get_overview`/`set_config`/`sign_now`（Task 7，camelCase JSON）
- Produces: 设置窗口 UI

- [ ] **Step 1: 重写 `index.html`**：

```html
<!doctype html>
<html lang="zh-CN">
  <head>
    <meta charset="UTF-8" />
    <link rel="stylesheet" href="/src/styles.css" />
    <title>SignDock 设置</title>
  </head>
  <body>
    <h1>SignDock</h1>
    <div id="cards"></div>
    <script type="module" src="/src/main.ts"></script>
  </body>
</html>
```

- [ ] **Step 2: 重写 `src/main.ts`**：

```ts
import { invoke } from "@tauri-apps/api/core";

interface ProductConfig { productId: string; mode: "off" | "remind" | "auto"; timeOfDay: string; token: string | null; }
interface RunRow { id: number; productId: string; at: string; outcome: string; detail: string; }
interface Overview { productId: string; config: ProductConfig; runs: RunRow[]; }

const OUTCOME_LABEL: Record<string, string> = {
  success: "成功", alreadySigned: "已签", reminded: "提醒", needManual: "需人工", failed: "失败",
};

async function refresh() {
  const list = await invoke<Overview[]>("get_overview");
  const root = document.getElementById("cards")!;
  root.innerHTML = "";
  for (const o of list) {
    const card = document.createElement("section");
    card.className = "card";
    card.innerHTML = `<h2>${o.productId}</h2>
      <label>模式
        <select id="mode-${o.productId}">
          <option value="off">关闭</option><option value="remind">仅提醒</option><option value="auto">自动签到</option>
        </select></label>
      <label>时间 <input type="time" id="time-${o.productId}" value="${o.config.timeOfDay}" /></label>
      <label>Token <input type="text" id="token-${o.productId}" placeholder="手动粘贴 token" value="${o.config.token ?? ""}" /></label>
      <div class="row">
        <button data-act="save" data-id="${o.productId}">保存</button>
        <button data-act="now" data-id="${o.productId}">立即执行一次</button>
        <span id="status-${o.productId}" class="muted">今日：${todayStatus(o.runs)}</span>
      </div>
      <table><thead><tr><th>时间</th><th>结果</th><th>详情</th></tr></thead>
      <tbody>${o.runs.slice(0, 10).map(r =>
        `<tr><td>${new Date(r.at).toLocaleString()}</td><td>${OUTCOME_LABEL[r.outcome] ?? r.outcome}</td><td>${r.detail}</td></tr>`
      ).join("")}</tbody></table>`;
    (card.querySelector(`#mode-${o.productId}`) as HTMLSelectElement).value = o.config.mode;
    root.appendChild(card);
  }
}

function todayStatus(runs: RunRow[]): string {
  const d = new Date().toDateString();
  const today = runs.find(r => new Date(r.at).toDateString() === d);
  return today ? `${OUTCOME_LABEL[today.outcome] ?? today.outcome} · ${today.detail}` : "未执行";
}

document.addEventListener("click", async (e) => {
  const btn = (e.target as HTMLElement).closest("button");
  if (!btn) return;
  const id = btn.dataset.id!;
  if (btn.dataset.act === "save") {
    await invoke("set_config", { cfg: {
      productId: id,
      mode: (document.getElementById(`mode-${id}`) as HTMLSelectElement).value,
      timeOfDay: (document.getElementById(`time-${id}`) as HTMLInputElement).value || "09:00",
      token: (document.getElementById(`token-${id}`) as HTMLInputElement).value || null,
    }});
    await refresh();
  } else if (btn.dataset.act === "now") {
    btn.textContent = "执行中…";
    const result = await invoke<Record<string, unknown>>("sign_now", { productId: id });
    btn.textContent = "立即执行一次";
    await refresh();
    void result;
  }
});

refresh();
```

- [ ] **Step 3: `src/styles.css`**（模板重写）：

```css
:root { color-scheme: dark; font-family: system-ui, sans-serif; }
body { margin: 24px; background: #17181c; color: #e6e6e6; }
h1 { font-size: 20px; }
#cards { display: grid; gap: 16px; }
.card { background: #212228; border: 1px solid #303138; border-radius: 10px; padding: 16px; }
.card h2 { margin: 0 0 12px; font-size: 16px; }
label { display: block; margin: 6px 0; font-size: 13px; }
input, select { background: #17181c; color: inherit; border: 1px solid #3a3b42; border-radius: 6px; padding: 4px 8px; margin-left: 8px; }
.row { display: flex; align-items: center; gap: 10px; margin: 10px 0; }
button { background: #2f6fed; color: #fff; border: 0; border-radius: 6px; padding: 6px 14px; cursor: pointer; }
.muted { color: #9a9aa5; font-size: 13px; }
table { width: 100%; border-collapse: collapse; font-size: 12px; }
th, td { text-align: left; padding: 4px 6px; border-bottom: 1px solid #303138; }
```

- [ ] **Step 4: 冒烟** `npm run tauri dev` 验证：三张卡片渲染、保存模式/时间/token 后重启进程仍生效（持久化）、"立即执行一次"出现 toast、表格出现新历史记录。

- [ ] **Step 5: Commit** `git add index.html src && git commit -m "feat: 设置窗口（产品卡片/历史/手动触发）"`

---

### Task 9-11: 抓包侦察（workbuddy / trae / qoder）

三个任务同构，各产出一份 `docs/recon/<product>.md`。这是人工操作为主的任务（代理抓包 + DevTools），执行者逐步确认。

**Files:**
- Create: `docs/recon/workbuddy.md` / `docs/recon/trae.md` / `docs/recon/qoder.md`

**Interfaces:**
- Produces: 阶段 2/3 适配器实现的输入。模板如下（每家填写）：

```markdown
# <product> 签到侦察
- 日期 / 客户端版本：
## 签到接口
- [ ] Method + URL：
- [ ] 必需 Headers（含鉴权头格式）：
- [ ] 请求 Body 与签名参数（如有，记录算法线索）：
- [ ] 成功响应样例（脱敏）：
## 状态查询接口
- [ ] 存在？Method + URL + 响应样例：
## 本机登录态
- [ ] token/session 存储路径：
- [ ] 加密方式（明文 / DPAPI / Chromium Local State AES 等）：
- [ ] 提取步骤（可复述）：
## 风控观察
- [ ] 是否有验证码/设备绑定？触发条件？
## 结论
- [ ] 可行模式：自动签到 / 仅提醒
- [ ] 未决问题：
```

- [ ] **Step 1（每家）: 准备抓包环境** — 推荐 mitmproxy 或 Fiddler Classic（HTTP/HTTPS 代理）。若产品为 Electron 且证书固定，可用 `--remote-debugging-port=9222` 启动后在 Chrome DevTools Network 面板直接观察（仅观察自己账号流量）。
- [ ] **Step 2: 冷启动抓 token** — 打开 App 登录状态下的积分/签到页，找到 token/凭证在请求头中的形态。
- [ ] **Step 3: 抓一次手动签到全流程** — 记录签到接口与状态接口的完整请求响应，脱敏存入文档。
- [ ] **Step 4: 定位本机凭证文件** — 常见位置：`%APPDATA%/<product>/`、`%APPDATA%/<product>/Local Storage/`、`Local State`（Chromium AES+DPAPI）。记录路径与加密方式。
- [ ] **Step 5: 填模板得出结论并提交** `git add docs/recon && git commit -m "docs: <product> 签到侦察结论"`

**每家侦察失败的降级**：无法拿到可重放接口（强制验证码/设备绑定/无 token 形态）→ 结论写"仅提醒"，阶段 2/3 计划中该产品不实现 sign_in。

---

## 计划边界（明确不做）

- workbuddy/trae/qoder 真实适配器与 DPAPI 提取（阶段 2/3，待侦察文档后另出计划）
- token 加密存储、开机自启、多账号
- 开源化整理（LICENSE、README、CI）
