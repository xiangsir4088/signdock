use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone, Utc};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use super::{AdapterError, CreditsSnapshot, ProductAdapter, SignOutcome, SignStatus};

pub const PROD_BASE: &str = "https://www.codebuddy.cn";
const STATUS_PATH: &str = "/v2/billing/meter/checkin-activity-status";
const SIGN_PATH: &str = "/v2/billing/meter/daily-checkin";
const REFRESH_PATH: &str = "/v2/plugin/auth/token/refresh";
/// 官方 OAuth 登录三接口（对齐 workbuddy-switch modules/oauth.rs）：无需用户粘贴 token
const OAUTH_STATE_PATH: &str = "/v2/plugin/auth/state";
const OAUTH_TOKEN_PATH: &str = "/v2/plugin/auth/token";
const OAUTH_ACCOUNT_PATH: &str = "/v2/plugin/login/account";
const OAUTH_PLATFORM: &str = "workbuddy";
pub const OAUTH_TIMEOUT_SECS: i64 = 600;
/// 与 tauri.conf.json 的 identifier 一致 → 与 signdock.db 同目录
const APP_DATA_DIR_NAME: &str = "com.signdock.app";
const UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/152.0.0.0 Safari/537.36";
/// 距过期不足 24h 即尝试刷新（对齐 workbuddy-switch 的惰性刷新阈值）
const REFRESH_AHEAD_MS: i64 = 24 * 3600 * 1000;
/// 读不到任何本机凭据时的统一文案（缺 token 与缺账号标识共用）
const WB_NOT_LOGGED_IN: &str = "未找到 WorkBuddy 本机登录凭证；请在设置中点「登录并获取 token」完成一次官方登录";

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

#[derive(Debug, Clone, PartialEq)]
pub struct WorkbuddyCred {
    pub access_token: String,
    pub refresh_token: String,
    pub uid: String,
    pub domain: String,
    /// 官方 OAuth 登录时服务端明文回传的昵称（产品文件里同名字段 5.6 起被加密，读不到）
    pub nickname: String,
    pub expires_at_ms: i64,
}

/// 发起登录的结果：auth_url 交给系统浏览器，login_id 用于轮询
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthSession {
    pub login_id: String,
    pub auth_url: String,
    pub expires_in: i64,
}

/// 登录完成：cred 写入 SignDock 自有文件，nickname 仅用于界面回执（绝不经接口回传 token）
#[derive(Debug, Clone)]
pub struct OAuthCompleted {
    pub cred: WorkbuddyCred,
    pub nickname: String,
}

#[derive(Debug, Clone)]
struct OAuthPending {
    state: String,
    expires_at_s: i64,
}

/// login_id → 进行中的登录会话（进程内；重启后前端重新发起）
static OAUTH_STATES: OnceLock<Mutex<HashMap<String, OAuthPending>>> = OnceLock::new();

fn oauth_states() -> &'static Mutex<HashMap<String, OAuthPending>> {
    OAUTH_STATES.get_or_init(|| Mutex::new(HashMap::new()))
}

fn now_secs() -> i64 { Utc::now().timestamp() }

fn new_login_id() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("wb_{nanos:x}{:x}", SEQ.fetch_add(1, Ordering::Relaxed))
}

/// SignDock 自有凭据（OAuth 登录产物）。与产品 auth 文件的区别：**这个可以回写**，
/// 因此刷新后的新 token 能落盘，下次启动继续自动续期（用户不必再粘贴）。
pub fn managed_cred_path() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(Path::new(&appdata).join(APP_DATA_DIR_NAME).join("workbuddy-cred.json"))
}

pub fn read_managed_cred(path: &Path) -> Result<WorkbuddyCred, AdapterError> {
    let raw = std::fs::read_to_string(path).map_err(|_| {
        AdapterError::AuthExpiredMsg("尚未登录 WorkBuddy；点「登录并获取 token」完成一次官方登录".into())
    })?;
    let v: Value = serde_json::from_str(&raw).map_err(|_| {
        AdapterError::AuthExpiredMsg("本地 WorkBuddy 登录凭据已损坏，请重新登录".into())
    })?;
    let access_token = str_field(&v, "accessToken");
    if access_token.is_empty() {
        return Err(AdapterError::AuthExpiredMsg("本地 WorkBuddy 登录凭据不完整，请重新登录".into()));
    }
    let now_ms = now_secs() * 1000;
    let expires_at_ms = parse_timestamp_ms(v.get("expiresAt"))
        .or_else(|| v.get("expiresIn").and_then(Value::as_i64).map(|s| now_ms + s * 1000))
        .unwrap_or(0);
    Ok(WorkbuddyCred {
        access_token,
        refresh_token: str_field(&v, "refreshToken"),
        uid: str_field(&v, "uid"),
        domain: str_field(&v, "domain"),
        nickname: str_field(&v, "nickname"),
        expires_at_ms,
    })
}

pub fn write_managed_cred(path: &Path, cred: &WorkbuddyCred) -> Result<(), AdapterError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|_| AdapterError::AuthExpired)?;
    }
    let v = json!({
        "accessToken": cred.access_token,
        "refreshToken": cred.refresh_token,
        "uid": cred.uid,
        "domain": cred.domain,
        "nickname": cred.nickname,
        "expiresAt": cred.expires_at_ms,
    });
    std::fs::write(path, v.to_string()).map_err(|_| AdapterError::AuthExpired)
}

pub fn default_auth_path() -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    Some(Path::new(&local)
        .join("CodeBuddyExtension")
        .join("Data").join("Public").join("auth")
        .join("workbuddy-desktop.info"))
}

/// 进程级凭证缓存：auth 文件路径 → (读取时的 mtime, 解析/刷新后的凭证)。
/// 解决“每次 resolve_cred 都重读文件并对过期 token 重复刷新”的问题
/// （status+sign+credits 一轮 = 3 次刷新，重复使用同一 stale refresh token 有轮换失效风险）。
static CRED_CACHE: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, WorkbuddyCred)>>> = OnceLock::new();

fn cred_cache() -> &'static Mutex<HashMap<PathBuf, (SystemTime, WorkbuddyCred)>> {
    CRED_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn str_field(obj: &Value, key: &str) -> String {
    obj.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn msg_of(v: &Value) -> String {
    v.get("msg").or_else(|| v.get("message"))
        .and_then(Value::as_str).unwrap_or("").to_string()
}

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
    const NULL: Value = Value::Null;
    for path in paths {
        let mut cur = v;
        for key in *path {
            match cur.get(*key) { Some(n) => cur = n, None => { cur = &NULL; break } }
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
    Err(AdapterError::Business(format!("{api_path} code={code} msg={}", msg_of(v))))
}

/// 到期时间相对今天的本地自然日偏移（0=今日，1=明日）；本地时区无法解析该毫秒时 None
fn local_day_offset(ms: i64, now: &chrono::DateTime<Local>) -> Option<i64> {
    Local.timestamp_millis_opt(ms).single()
        .map(|dt| dt.date_naive().signed_duration_since(now.date_naive()).num_days())
}

/// WorkBuddy 5.6+ 把 accessToken/refreshToken 换成 {$wbEncrypted, envelope} 信封（AES-GCM，
/// 密钥由产品原生模块提供）。SignDock 不逆向厂商凭据保护，遇到即转官方登录。
fn is_wb_encrypted(v: &Value) -> bool {
    v.get("$wbEncrypted").is_some()
}

pub fn read_cred(path: &Path) -> Result<WorkbuddyCred, AdapterError> {
    let raw = std::fs::read_to_string(path).map_err(|_| AdapterError::AuthExpired)?;
    let v: Value = serde_json::from_str(&raw).map_err(|_| AdapterError::AuthExpired)?;
    let auth = v.get("auth").cloned().unwrap_or(Value::Null);
    let account = v.get("account").cloned().unwrap_or(Value::Null);
    let access = auth.get("accessToken").cloned().unwrap_or(Value::Null);
    if is_wb_encrypted(&access) {
        return Err(AdapterError::AuthExpiredMsg(
            "WorkBuddy 5.6+ 已加密本机登录凭据，无法自动读取 accessToken；请在设置中点「登录并获取 token」完成一次官方登录".into(),
        ));
    }
    let access_token = access.as_str().unwrap_or_default().to_string();
    if access_token.is_empty() {
        return Err(AdapterError::AuthExpiredMsg(WB_NOT_LOGGED_IN.into()));
    }
    Ok(WorkbuddyCred {
        access_token,
        refresh_token: str_field(&auth, "refreshToken"),
        uid: str_field(&account, "uid"),
        domain: str_field(&auth, "domain"),
        nickname: str_field(&account, "nickname"),
        // 宽容解析：产品侧这个字段历史上给过秒/字符串，按 0 处理会把新鲜 token 误判成过期
        expires_at_ms: parse_timestamp_ms(auth.get("expiresAt")).unwrap_or(0),
    })
}

/// WorkBuddy 的 accessToken 本身就是 Keycloak JWT，payload 里带明文 nickname。
/// （产品文件里的 nickname 自 5.6 起是厂商加密信封，我们不逆向；解自己的 token 不需要逆向。）
fn nickname_from_jwt(token: &str) -> Option<String> {
    use base64::Engine;
    let payload = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(payload.trim_end_matches('='))
        .ok()?;
    let name = str_field(&serde_json::from_slice::<Value>(&bytes).ok()?, "nickname");
    let name = name.trim().to_string();
    (!name.is_empty()).then_some(name)
}

/// 界面账号标识：昵称优先（新登录会落盘昵称；老凭据没落盘则从 token 的 JWT 里解出），
/// 退化到完整 uid；两者都没有视为未登录。
fn account_label_of(cred: &WorkbuddyCred) -> Option<String> {
    let name = cred.nickname.trim();
    if !name.is_empty() {
        return Some(name.to_string());
    }
    if let Some(n) = nickname_from_jwt(&cred.access_token) {
        return Some(n);
    }
    let uid = cred.uid.trim();
    (!uid.is_empty()).then(|| uid.to_string())
}

/// workbuddy 的接口靠 UA 识别调用方，且请求自身不设超时——20s 总超时挂在 client 上。
/// 调度每 60s 重建一次适配器，客户端只该建这一次。
fn http_client() -> reqwest::Client {
    static HTTP: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    HTTP.get_or_init(|| reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .user_agent(UA)
        .build().unwrap_or_default()).clone()
}

pub struct WorkbuddyAdapter {
    base_url: String,
    /// 积分/资源接口域名（与签到 base_url 分开；recon：www.workbuddy.cn）
    resource_base: String,
    client: reqwest::Client,
    /// None=自动探测本机 auth 文件；测试用 Some 注入临时文件
    cred_path: Option<PathBuf>,
    /// SignDock 自有凭据（OAuth 登录结果）。None=不启用（测试构造器），Some=优先于产品 auth 文件
    managed_path: Option<PathBuf>,
}

impl Default for WorkbuddyAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl WorkbuddyAdapter {
    pub fn new() -> Self {
        Self::with_base_url(PROD_BASE).with_resource_base(PROD_RESOURCE_BASE)
            .with_managed_path(managed_cred_path())
    }

    pub fn with_base_url(base_url: &str) -> Self {
        let base = base_url.trim_end_matches('/').to_string();
        Self { base_url: base.clone(), resource_base: base, client: http_client(),
               cred_path: None, managed_path: None }
    }

    pub fn with_resource_base(mut self, base: &str) -> Self {
        self.resource_base = base.trim_end_matches('/').to_string();
        self
    }

    pub fn with_cred_path(mut self, path: PathBuf) -> Self { self.cred_path = Some(path); self }

    pub fn with_managed_path(mut self, path: Option<PathBuf>) -> Self {
        self.managed_path = path; self
    }

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

    /// 优先级：SignDock 自有凭据（OAuth 登录，可回写续期）→ 产品 auth 文件（只读）。
    /// 自有凭据刷新失败时**继续往下兜底**，全部失败才报错（报的是自有凭据那条，最可操作）。
    async fn resolve_cred(&self) -> Result<WorkbuddyCred, AdapterError> {
        let mut managed_err: Option<AdapterError> = None;
        if let Some(path) = self.managed_path.clone() {
            if let Ok(cred) = read_managed_cred(&path) {
                match self.renew_managed(&path, cred).await {
                    Ok(c) => return Ok(c),
                    Err(e) => managed_err = Some(e),
                }
            }
        }
        let path = match &self.cred_path {
            Some(p) => p.clone(),
            None => default_auth_path().ok_or(AdapterError::AuthExpired)?,
        };
        self.resolve_from_product_file(&path).await
            .map_err(|e| managed_err.unwrap_or(e))
    }

    async fn resolve_from_product_file(&self, path: &Path) -> Result<WorkbuddyCred, AdapterError> {
        // mtime 变化即视为缓存失效（缓存键含读取时的 mtime）；mtime 不可得按不匹配处理
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let now = Utc::now().timestamp_millis();
        if let Some(mt) = mtime {
            // 仅加锁取快照，绝不跨 await 持锁；两任务并发各刷新一次的小竞态可接受
            let hit = cred_cache().lock().ok().and_then(|map| {
                map.get(path).filter(|(cached_mt, cred)| {
                    *cached_mt == mt
                        && cred.expires_at_ms > now
                        && cred.expires_at_ms - now >= REFRESH_AHEAD_MS
                }).map(|(_, cred)| cred.clone())
            });
            if let Some(cred) = hit {
                return Ok(cred);
            }
        }
        let cred = read_cred(path)?;
        let expired = cred.expires_at_ms <= now;
        if expired || cred.expires_at_ms - now < REFRESH_AHEAD_MS {
            match self.refresh_cred(&cred).await {
                Ok(fresh) => {
                    if let (Some(mt), Ok(mut map)) = (mtime, cred_cache().lock()) {
                        map.insert(path.to_path_buf(), (mt, fresh.clone()));
                    }
                    return Ok(fresh);
                }
                Err(e) if expired => return Err(e),
                // 未过期但主动刷新失败：照旧使用现有 token（此兜底不缓存——缓存了也必然再刷）
                Err(_) => return Ok(cred),
            }
        }
        // 无刷新且宽裕新鲜（expires-now >= REFRESH_AHEAD_MS 蕴含 > now）→ 缓存
        if let (Some(mt), Ok(mut map)) = (mtime, cred_cache().lock()) {
            map.insert(path.to_path_buf(), (mt, cred.clone()));
        }
        Ok(cred)
    }

    /// 自有凭据需要续期时刷新并**回写自有文件**（下次启动继续自动续期）；
    /// 未过期但刷新失败时照旧使用现有 token。
    async fn renew_managed(&self, path: &Path, cred: WorkbuddyCred) -> Result<WorkbuddyCred, AdapterError> {
        let now = Utc::now().timestamp_millis();
        let expired = cred.expires_at_ms <= now;
        if !expired && cred.expires_at_ms - now >= REFRESH_AHEAD_MS {
            return Ok(cred);
        }
        match self.refresh_cred(&cred).await {
            Ok(fresh) => {
                write_managed_cred(path, &fresh).ok();
                Ok(fresh)
            }
            Err(e) if expired => Err(e),
            Err(_) => Ok(cred),
        }
    }

    /// 发起官方 OAuth 登录：向 codebuddy 申请 state，返回浏览器要打开的地址与轮询句柄。
    /// 用户只需在浏览器里登录一次，SignDock 由此拿到 token —— 不再要求手工粘贴。
    pub async fn oauth_start(&self) -> Result<OAuthSession, AdapterError> {
        self.oauth_start_at(now_secs()).await
    }

    async fn oauth_start_at(&self, now_s: i64) -> Result<OAuthSession, AdapterError> {
        let url = format!("{}{OAUTH_STATE_PATH}?platform={OAUTH_PLATFORM}", self.base_url);
        let resp = self.client.post(&url).json(&json!({})).send().await?;
        let v: Value = resp.json().await
            .map_err(|_| AdapterError::SchemaChanged("auth/state 返回非 JSON".into()))?;
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        let state = str_field(&data, "state");
        if state.is_empty() {
            return Err(AdapterError::SchemaChanged("auth/state 响应缺少 state".into()));
        }
        let auth_url = first_value(&data, &["authUrl", "auth_url", "url"])
            .and_then(Value::as_str).unwrap_or("").to_string();
        let auth_url = if auth_url.is_empty() {
            format!("{}/login?state={state}", self.base_url)
        } else { auth_url };
        let login_id = new_login_id();
        if let Ok(mut map) = oauth_states().lock() {
            map.retain(|_, info| info.expires_at_s > now_s);
            map.insert(login_id.clone(), OAuthPending {
                state, expires_at_s: now_s + OAUTH_TIMEOUT_SECS });
        }
        Ok(OAuthSession { login_id, auth_url, expires_in: OAUTH_TIMEOUT_SECS })
    }

    /// 轮询一次登录结果：Ok(None)=用户还没在浏览器完成，Ok(Some)=完成并已取到凭据。
    pub async fn oauth_poll(&self, login_id: &str) -> Result<Option<OAuthCompleted>, AdapterError> {
        self.oauth_poll_at(login_id, now_secs()).await
    }

    async fn oauth_poll_at(&self, login_id: &str, now_s: i64) -> Result<Option<OAuthCompleted>, AdapterError> {
        let state = {
            let Ok(mut map) = oauth_states().lock() else {
                return Err(AdapterError::AuthExpiredMsg("登录会话存储不可用，请重试".into()));
            };
            let Some(info) = map.get(login_id) else {
                return Err(AdapterError::AuthExpiredMsg("登录会话已失效，请重新点「登录并获取 token」".into()));
            };
            if now_s > info.expires_at_s {
                map.remove(login_id);
                return Err(AdapterError::AuthExpiredMsg("登录超时，请重新点「登录并获取 token」".into()));
            }
            info.state.clone()
        };
        let token_url = format!("{}{OAUTH_TOKEN_PATH}?state={state}", self.base_url);
        let resp = self.client.get(&token_url).send().await?;
        let v: Value = resp.json().await
            .map_err(|_| AdapterError::SchemaChanged("auth/token 返回非 JSON".into()))?;
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(-1);
        let data = v.get("data").cloned().unwrap_or(Value::Null);
        let access_token = str_field(&data, "accessToken");
        if code != 0 && code != 200 {
            // 非零 code 是服务端明确回了失败（如 state 失效），不是「用户还没点完」
            return Err(AdapterError::AuthExpiredMsg(format!("登录未完成：code={code} {}", msg_of(&v))));
        }
        if access_token.is_empty() {
            return Ok(None); // code=0 且 data 为空 = 用户尚未在浏览器完成登录
        }
        let now_ms = now_s * 1000;
        let domain = str_field(&data, "domain");
        let cred = WorkbuddyCred {
            access_token: access_token.clone(),
            refresh_token: str_field(&data, "refreshToken"),
            uid: String::new(),
            domain: domain.clone(),
            nickname: String::new(),
            expires_at_ms: parse_timestamp_ms(first_value(&data, &["expiresAt", "expires_at"]))
                .or_else(|| first_value(&data, &["expiresIn", "expires_in"])
                    .and_then(Value::as_i64).map(|s| now_ms + s * 1000))
                .unwrap_or(now_ms + 3600 * 1000),
        };
        // 账号信息仅用于确认登录成了哪个账号；拉取失败不影响登录成立
        let headers = self.headers_for(&cred);
        let account_url = format!("{}{OAUTH_ACCOUNT_PATH}?state={state}", self.base_url);
        let (mut uid, mut nickname) = (String::new(), String::new());
        if let Ok(resp) = self.client.get(&account_url).headers(headers).send().await {
            if let Ok(acc) = resp.json::<Value>().await {
                let ad = acc.get("data").cloned().unwrap_or(Value::Null);
                uid = str_field(&ad, "uid");
                nickname = str_field(&ad, "nickname");
            }
        }
        let cred = WorkbuddyCred { uid, nickname: nickname.clone(), ..cred };
        if let Ok(mut map) = oauth_states().lock() { map.remove(login_id); }
        Ok(Some(OAuthCompleted { cred, nickname }))
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
            // 服务端未给任何到期字段：保守按 1h 兜底，避免 0 值导致每次都刷新
            .unwrap_or(now + 3600 * 1000);
        let new_refresh = str_field(&d, "refreshToken");
        let new_domain = str_field(&d, "domain");
        Ok(WorkbuddyCred {
            access_token,
            refresh_token: if new_refresh.is_empty() { cred.refresh_token.clone() } else { new_refresh },
            uid: cred.uid.clone(),
            domain: if new_domain.is_empty() { cred.domain.clone() } else { new_domain },
            nickname: cred.nickname.clone(),
            expires_at_ms,
        })
    }

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
}

#[async_trait::async_trait]
impl ProductAdapter for WorkbuddyAdapter {
    fn id(&self) -> String { "workbuddy".into() }
    async fn query_sign_status(&self) -> Result<SignStatus, AdapterError> {
        let cred = self.resolve_cred().await?;
        let v = self.post_meter(STATUS_PATH, &cred).await?;
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code != 0 && code != 200 {
            return Err(AdapterError::Business(format!("status code={code} msg={}", msg_of(&v))));
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

    async fn sign_in(&self) -> Result<SignOutcome, AdapterError> {
        let cred = self.resolve_cred().await?;
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
        Err(AdapterError::Business(format!("sign code={code} msg={msg}")))
    }

    async fn fetch_credits(&self) -> Result<CreditsSnapshot, AdapterError> {
        let cred = self.resolve_cred().await?;
        let now = Local::now();
        let now_ms = now.timestamp_millis();
        let day_start = now.format("%Y-%m-%d 00:00:00").to_string();

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
            // total 缺席时不能当成 0：那会让「采满了」立即成立，当天消耗只剩第一页
            let total = parse_number(data.get("total")).map(|t| t as i64);
            if items.is_empty() || total.is_some_and(|t| collected >= t) || page >= USAGE_MAX_PAGES { break; }
            page += 1;
        }

        // 今/明日到期：paid+free Accounts，resolve 后按本地自然日归类求和。
        // free 的 SlicePeriod 查询窗口必须盖到明日，否则明日到期账号根本不会被返回。
        let mut expiring_today = 0.0f64;
        let mut expiring_tomorrow = 0.0f64;
        let slice_end = (now + chrono::Duration::days(1)).format("%Y-%m-%d 23:59:59").to_string();
        let queries = [
            (RESOURCE_PAID_PATH, json!({"PageNumber":1,"PageSize":200,"Status":[0,3],
                                        "PackageCodes":PAID_PACKAGE_CODES})),
            (RESOURCE_FREE_PATH, json!({"PageNumber":1,"PageSize":200,"Status":[0,3],
                                        "PackageCodes":FREE_PACKAGE_CODES,
                                        "SlicePeriodStartTime":day_start,"SlicePeriodEndTime":slice_end})),
        ];
        for (path, body) in queries {
            let v = self.post_json(&self.resource_base, path, &cred, body).await?;
            check_biz(&v, path)?;
            for acc in accounts_of(&v) {
                let Some(ms) = resolve_expire_at(acc, now_ms) else { continue };
                let Some(offset) = local_day_offset(ms, &now) else { continue };
                if offset != 0 && offset != 1 { continue; }
                let remain = first_number(acc,
                    &["CycleCapacityRemainPrecise", "CycleCapacityRemain", "CycleRemainCapacity"])
                    .unwrap_or(0.0);
                if offset == 0 { expiring_today += remain; } else { expiring_tomorrow += remain; }
            }
        }
        Ok(CreditsSnapshot { balance, today_used, expiring_today, expiring_tomorrow, fetched_at_ms: now_ms })
    }

    /// 账号标识只读本地凭据文件——不为此触发刷新，也不联网
    async fn account_label(&self) -> Result<String, AdapterError> {
        if let Some(path) = self.managed_path.as_ref() {
            if let Ok(cred) = read_managed_cred(path) {
                if let Some(label) = account_label_of(&cred) {
                    return Ok(label);
                }
            }
        }
        let path = self
            .cred_path
            .clone()
            .or_else(default_auth_path)
            .ok_or_else(|| AdapterError::AuthExpiredMsg(WB_NOT_LOGGED_IN.into()))?;
        let cred = read_cred(&path)?;
        account_label_of(&cred).ok_or_else(|| AdapterError::AuthExpiredMsg(WB_NOT_LOGGED_IN.into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{body_partial_json, header, method, path, query_param};
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
        // 秒级时间戳按秒理解（123s → 123_000ms）；产品实际给的是毫秒（>1e10 原样用）
        assert_eq!(c.expires_at_ms, 123_000);
    }

    #[test]
    fn read_cred_missing_file_is_auth_expired() {
        assert!(matches!(read_cred(Path::new("Z:\\definitely-not-here.info")), Err(AdapterError::AuthExpired)));
    }

    #[test]
    fn read_cred_empty_token_says_not_logged_in() {
        let p = write_cred("empty", "", "RT", future_ms());
        let err = read_cred(&p).unwrap_err();
        std::fs::remove_file(&p).ok();
        assert!(matches!(&err, AdapterError::AuthExpiredMsg(msg) if msg.contains("登录") && msg.contains("token")), "{err:?}");
    }

    #[test]
    fn read_cred_encrypted_envelope_points_to_oauth_login() {
        let mut p = std::env::temp_dir();
        p.push(format!("signdock-wb-enc-{}.info", std::process::id()));
        std::fs::write(&p, json!({
            "account": { "uid": "uid-1" },
            "auth": {
                "accessToken": { "$wbEncrypted": 1, "envelope": "eyJzdWl0ZSI6MX0=" },
                "refreshToken": { "$wbEncrypted": 1, "envelope": "eyJzdWl0ZSI6MX0=" },
                "domain": "www.workbuddy.cn", "expiresAt": future_ms()
            }
        }).to_string()).unwrap();
        let err = read_cred(&p).unwrap_err();
        std::fs::remove_file(&p).ok();
        assert!(matches!(&err, AdapterError::AuthExpiredMsg(msg) if msg.contains("加密") && msg.contains("accessToken")), "{err:?}");
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
        assert_eq!(adapter(&s, cred.clone()).query_sign_status().await.unwrap(), SignStatus::SignedToday);
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
        assert_eq!(adapter(&s, cred.clone()).query_sign_status().await.unwrap(), SignStatus::NotSigned);
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
        assert_eq!(adapter(&s, cred.clone()).query_sign_status().await.unwrap(), SignStatus::Unknown);
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn status_missing_field_is_schema_changed() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"weird":1}})))
            .mount(&s).await;
        let cred = write_cred("weird", "AT", "RT", future_ms());
        assert!(matches!(adapter(&s, cred.clone()).query_sign_status().await,
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
        let out = adapter(&s, cred.clone()).sign_in().await.unwrap();
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
        assert_eq!(adapter(&s, cred.clone()).sign_in().await.unwrap(), SignOutcome::AlreadySigned);
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
        assert!(matches!(adapter(&s, cred.clone()).sign_in().await, Ok(SignOutcome::NeedManual(_))));
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn sign_401_is_auth_expired() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(SIGN_PATH))
            .respond_with(ResponseTemplate::new(401)).mount(&s).await;
        let cred = write_cred("401", "AT", "RT", future_ms());
        assert!(matches!(adapter(&s, cred.clone()).sign_in().await, Err(AdapterError::AuthExpired)));
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
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        // 绝不回写：文件里的 token 必须原样是 OLD
        assert_eq!(read_cred(&cred).unwrap().access_token, "OLD");
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn refresh_once_then_cached() {
        let s = MockServer::start().await;
        // expect(1)：两次 resolve_cred（status x2）只允许发生一次刷新，结果由 MockServer drop 时校验
        Mock::given(method("POST")).and(path(REFRESH_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"accessToken":"NEW","expiresIn":2592000}})))
            .expect(1)
            .mount(&s).await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .and(header("authorization", "Bearer NEW"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":true,"today_checked_in":false}})))
            .mount(&s).await;
        let cred = write_cred("cache1", "OLD", "RT", past_ms());
        let a = adapter(&s, cred.clone());
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        // 绝不回写：文件仍是 OLD
        assert_eq!(read_cred(&cred).unwrap().access_token, "OLD");
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn cache_ignores_stale_entry_after_file_changes() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(REFRESH_PATH))
            .and(header("x-refresh-token", "RT"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"accessToken":"NEW","expiresIn":2592000}})))
            .expect(2)
            .mount(&s).await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .and(header("authorization", "Bearer NEW"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":true,"today_checked_in":false}})))
            .mount(&s).await;
        let cred = write_cred("cache2", "OLD", "RT", past_ms());
        let a = adapter(&s, cred.clone());
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        // 同一文件被外部更换（mtime 变化）→ 缓存条目失效，必须再次刷新
        std::thread::sleep(std::time::Duration::from_millis(60));
        let cred2 = write_cred("cache2", "OLD2", "RT", past_ms());
        assert_eq!(cred2, cred);
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn refresh_failure_when_expired_is_auth_expired() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(REFRESH_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":12153,"msg":"invalid_grant"})))
            .mount(&s).await;
        let cred = write_cred("rf", "OLD", "RT", past_ms());
        assert!(matches!(adapter(&s, cred.clone()).query_sign_status().await,
            Err(AdapterError::AuthExpired)));
        std::fs::remove_file(&cred).ok();
    }

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
        assert_eq!(parsed, Some(Local.with_ymd_and_hms(2026, 9, 25, 10, 0, 0).unwrap().timestamp_millis()));
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
        let snap = adapter(&s, cred.clone()).fetch_credits().await.unwrap();
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
        let snap = adapter(&s, cred.clone()).fetch_credits().await.unwrap();
        std::fs::remove_file(&cred).ok();
        assert!((snap.today_used - 4.5).abs() < 1e-6);
    }

    #[tokio::test]
    async fn credits_today_used_paginates_until_empty_page_when_total_missing() {
        let s = MockServer::start().await;
        // 服务端没回 total：只能一路翻到空页，把第一页当成全部会少算当天消耗
        Mock::given(method("POST")).and(path(RESOURCE_USAGE_PATH))
            .and(body_partial_json(json!({"pageNum":1})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(
                {"code":0,"data":{"data":[{"credit":1.5},{"credit":"2.5"}]}})))
            .mount(&s).await;
        Mock::given(method("POST")).and(path(RESOURCE_USAGE_PATH))
            .and(body_partial_json(json!({"pageNum":2})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(
                {"code":0,"data":{"data":[{"credit":"0.5"}]}})))
            .mount(&s).await;
        Mock::given(method("POST")).and(path(RESOURCE_USAGE_PATH))
            .and(body_partial_json(json!({"pageNum":3})))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(
                {"code":0,"data":{"data":[]}})))
            .mount(&s).await;
        mount_credits_defaults(&s).await;
        let cred = write_cred("cpgn", "AT", "RT", future_ms());
        let snap = adapter(&s, cred.clone()).fetch_credits().await.unwrap();
        std::fs::remove_file(&cred).ok();
        assert!((snap.today_used - 4.5).abs() < 1e-6);
    }

    #[tokio::test]
    async fn credits_expiring_windows_separate_today_and_tomorrow() {
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
        let snap = adapter(&s, cred.clone()).fetch_credits().await.unwrap();
        std::fs::remove_file(&cred).ok();
        assert!((snap.expiring_today - 90.57).abs() < 1e-6);
        assert!((snap.expiring_tomorrow - 100.0).abs() < 1e-6);
    }

    /// 造一个只用于测试的 JWT（payload 可控，签名是假串）
    fn fake_jwt(payload: &Value) -> String {
        use base64::Engine;
        let e = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        format!("{}.{}.sig", e.encode(b"{\"alg\":\"none\"}"), e.encode(payload.to_string().as_bytes()))
    }

    #[tokio::test]
    async fn account_label_prefers_nickname_then_full_uid() {
        let mut m = std::env::temp_dir();
        m.push(format!("signdock-wb-acct-{}.json", std::process::id()));
        let base = WorkbuddyCred {
            access_token: "AT".into(), refresh_token: "RT".into(),
            uid: "uid-0123456789".into(), domain: String::new(),
            nickname: "小明".into(), expires_at_ms: future_ms(),
        };
        let adapter = || WorkbuddyAdapter::with_base_url("http://127.0.0.1:9")
            .with_cred_path(m.clone()).with_managed_path(Some(m.clone()));
        write_managed_cred(&m, &base).unwrap();
        assert_eq!(adapter().account_label().await.unwrap(), "小明");
        // 老凭据文件没有昵称字段 → 用完整 uid，不截断（截断会让用户以为显示不全）
        write_managed_cred(&m, &WorkbuddyCred { nickname: String::new(), ..base.clone() }).unwrap();
        assert_eq!(adapter().account_label().await.unwrap(), "uid-0123456789");
        std::fs::remove_file(&m).ok();
    }

    /// 老凭据文件既没存昵称、又懒得重登录时：昵称就在我们自有 token 的 JWT payload 里
    #[tokio::test]
    async fn account_label_reads_nickname_from_own_jwt_when_cred_has_none() {
        let mut m = std::env::temp_dir();
        m.push(format!("signdock-wb-jwt-{}.json", std::process::id()));
        write_managed_cred(&m, &WorkbuddyCred {
            access_token: fake_jwt(&json!({"nickname": "小红", "exp": 9_999_999_999_i64})),
            refresh_token: "RT".into(), uid: "uid-0123456789".into(), domain: String::new(),
            nickname: String::new(), expires_at_ms: future_ms(),
        }).unwrap();
        let a = WorkbuddyAdapter::with_base_url("http://127.0.0.1:9")
            .with_cred_path(m.clone()).with_managed_path(Some(m.clone()));
        assert_eq!(a.account_label().await.unwrap(), "小红");
        std::fs::remove_file(&m).ok();
    }

    /// 自有凭据刷新失败**不能**永久挡住兜底：产品自己可能已经刷新出可用明文 token
    #[tokio::test]
    async fn managed_refresh_failure_falls_back_to_product_file() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(REFRESH_PATH))
            .respond_with(ResponseTemplate::new(401)).mount(&s).await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .and(header("authorization", "Bearer FROM-FILE"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":true,"today_checked_in":false}})))
            .mount(&s).await;
        let auth = write_cred("fallback", "FROM-FILE", "", future_ms());
        let m = managed("fallback");
        write_managed_cred(&m, &WorkbuddyCred { access_token: "DEAD".into(), refresh_token: "RT".into(),
            uid: "u".into(), domain: "d".into(), nickname: String::new(), expires_at_ms: past_ms() }).unwrap();
        let a = WorkbuddyAdapter::with_base_url(&s.uri())
            .with_cred_path(auth.clone()).with_managed_path(Some(m.clone()));
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        std::fs::remove_file(&auth).ok();
        std::fs::remove_file(&m).ok();
    }

    /// 全都失败时报**可操作**的那条（自有凭据到期→重新登录），而不是"没找到文件"
    #[tokio::test]
    async fn all_sources_failing_keeps_the_actionable_managed_error() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(REFRESH_PATH))
            .respond_with(ResponseTemplate::new(401)).mount(&s).await;
        let m = managed("allfail");
        write_managed_cred(&m, &WorkbuddyCred { access_token: "DEAD".into(), refresh_token: "RT".into(),
            uid: String::new(), domain: String::new(), nickname: String::new(), expires_at_ms: past_ms() }).unwrap();
        let a = WorkbuddyAdapter::with_base_url(&s.uri())
            .with_cred_path(PathBuf::from("Z:\\no-such.info")).with_managed_path(Some(m.clone()));
        let err = a.query_sign_status().await.unwrap_err().to_string();
        std::fs::remove_file(&m).ok();
        assert!(err.contains("重新登录"), "应提示重新登录，实得 {err}");
    }

    #[test]
    fn nickname_from_jwt_ignores_opaque_and_nameless_tokens() {
        assert_eq!(nickname_from_jwt("not-a-jwt"), None);
        assert_eq!(nickname_from_jwt(&fake_jwt(&json!({"exp": 1}))), None);
    }

    #[tokio::test]
    async fn managed_cred_round_trips_nickname() {
        let mut m = std::env::temp_dir();
        m.push(format!("signdock-wb-nick-{}.json", std::process::id()));
        write_managed_cred(&m, &WorkbuddyCred {
            access_token: "AT".into(), refresh_token: "RT".into(), uid: "u".into(),
            domain: String::new(), nickname: "小明".into(), expires_at_ms: future_ms(),
        }).unwrap();
        assert_eq!(read_managed_cred(&m).unwrap().nickname, "小明");
        std::fs::remove_file(&m).ok();
    }

    #[tokio::test]
    async fn credits_401_is_auth_expired() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(401)).mount(&s).await;
        let cred = write_cred("c401", "AT", "RT", future_ms());
        assert!(matches!(adapter(&s, cred.clone()).fetch_credits().await,
            Err(AdapterError::AuthExpired)));
        std::fs::remove_file(&cred).ok();
    }

    #[tokio::test]
    async fn credits_business_error_code_is_not_reported_as_schema_change() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":10001,"msg":"PackageCodes required"})))
            .mount(&s).await;
        mount_credits_defaults(&s).await;
        let cred = write_cred("cbiz", "AT", "RT", future_ms());
        // 服务端只是回了个业务码，把它说成「接口可能已变更」会把排障方向整个带偏
        let err = adapter(&s, cred.clone()).fetch_credits().await.unwrap_err();
        std::fs::remove_file(&cred).ok();
        assert!(matches!(&err, AdapterError::Business(_)), "{err:?}");
        let text = err.to_string();
        assert!(text.contains("PackageCodes required"), "{text}");
        assert!(!text.contains("接口可能已变更"), "{text}");
    }

    #[tokio::test]
    async fn status_business_code_is_business_error() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":500,"msg":"服务繁忙，请稍后再试"})))
            .mount(&s).await;
        let cred = write_cred("sbiz", "AT", "RT", future_ms());
        assert!(matches!(adapter(&s, cred.clone()).query_sign_status().await,
            Err(AdapterError::Business(m)) if m.contains("服务繁忙")));
        std::fs::remove_file(&cred).ok();
    }

    // ==================== 官方 OAuth 登录（零输入路径） ====================
    // 端点取自 workbuddy-switch oauth.rs：auth/state → auth/token → login/account

    async fn mount_state(s: &MockServer) {
        Mock::given(method("POST")).and(path(OAUTH_STATE_PATH))
            .and(query_param("platform", OAUTH_PLATFORM))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!(
                {"code":0,"data":{"state":"ST-1","authUrl":"https://www.codebuddy.cn/login?state=ST-1"}})))
            .mount(s).await;
    }

    #[tokio::test]
    async fn oauth_start_returns_login_id_and_auth_url() {
        let s = MockServer::start().await;
        mount_state(&s).await;
        let sess = WorkbuddyAdapter::with_base_url(&s.uri()).oauth_start_at(1_000).await.unwrap();
        assert!(sess.login_id.starts_with("wb_"), "{}", sess.login_id);
        assert_eq!(sess.auth_url, "https://www.codebuddy.cn/login?state=ST-1");
    }

    #[tokio::test]
    async fn oauth_start_without_auth_url_falls_back_to_login_endpoint() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(OAUTH_STATE_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{"state":"ST-2"}})))
            .mount(&s).await;
        let sess = WorkbuddyAdapter::with_base_url(&s.uri()).oauth_start_at(1_000).await.unwrap();
        assert_eq!(sess.auth_url, format!("{}/login?state=ST-2", s.uri()));
    }

    #[tokio::test]
    async fn oauth_start_missing_state_is_schema_changed() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(OAUTH_STATE_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{}})))
            .mount(&s).await;
        assert!(matches!(WorkbuddyAdapter::with_base_url(&s.uri()).oauth_start_at(1_000).await,
            Err(AdapterError::SchemaChanged(_))));
    }

    #[tokio::test]
    async fn oauth_poll_returns_none_until_token_exists() {
        let s = MockServer::start().await;
        mount_state(&s).await;
        Mock::given(method("GET")).and(path(OAUTH_TOKEN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{}})))
            .mount(&s).await;
        let a = WorkbuddyAdapter::with_base_url(&s.uri());
        let sess = a.oauth_start_at(1_000).await.unwrap();
        assert!(a.oauth_poll_at(&sess.login_id, 1_010).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oauth_poll_reports_real_failure_instead_of_polling_on() {
        // 「还没登录完」是 code=0 且 data 空（recon 实测）；非零 code 是真失败。
        // 把真失败也当 pending，前端会白轮询十分钟，而且永远看不到原因。
        let s = MockServer::start().await;
        mount_state(&s).await;
        Mock::given(method("GET")).and(path(OAUTH_TOKEN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":10001,"msg":"invalid state"})))
            .mount(&s).await;
        let a = WorkbuddyAdapter::with_base_url(&s.uri());
        let sess = a.oauth_start_at(1_000).await.unwrap();
        let err = a.oauth_poll_at(&sess.login_id, 1_010).await.unwrap_err();
        assert!(err.to_string().contains("invalid state"), "{err:?}");
    }

    #[tokio::test]
    async fn oauth_poll_unknown_login_id_is_not_retryable() {
        let s = MockServer::start().await;
        let a = WorkbuddyAdapter::with_base_url(&s.uri());
        let err = a.oauth_poll_at("wb_not-here", 1_000).await.unwrap_err();
        assert!(matches!(&err, AdapterError::AuthExpiredMsg(m) if m.contains("重新")), "{err:?}");
    }

    #[tokio::test]
    async fn oauth_poll_after_timeout_is_error() {
        let s = MockServer::start().await;
        mount_state(&s).await;
        let a = WorkbuddyAdapter::with_base_url(&s.uri());
        let sess = a.oauth_start_at(1_000).await.unwrap();
        let later = 1_000 + OAUTH_TIMEOUT_SECS + 1;
        let err = a.oauth_poll_at(&sess.login_id, later).await.unwrap_err();
        assert!(matches!(&err, AdapterError::AuthExpiredMsg(m) if m.contains("超时")), "{err:?}");
    }

    #[tokio::test]
    async fn oauth_poll_success_returns_fresh_cred_and_nickname() {
        let s = MockServer::start().await;
        mount_state(&s).await;
        Mock::given(method("GET")).and(path(OAUTH_TOKEN_PATH))
            .and(query_param("state", "ST-1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{
                "accessToken":"AT-O","refreshToken":"RT-O","expiresIn":2_592_000,"domain":"www.codebuddy.cn"}})))
            .mount(&s).await;
        Mock::given(method("GET")).and(path(OAUTH_ACCOUNT_PATH))
            .and(query_param("state", "ST-1"))
            .and(header("authorization", "Bearer AT-O"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{
                "uid":"uid-9","nickname":"小明"}})))
            .mount(&s).await;
        let a = WorkbuddyAdapter::with_base_url(&s.uri());
        let sess = a.oauth_start_at(1_000).await.unwrap();
        let done = a.oauth_poll_at(&sess.login_id, 1_010).await.unwrap().expect("应完成");
        assert_eq!(done.cred.access_token, "AT-O");
        assert_eq!(done.cred.refresh_token, "RT-O");
        assert_eq!(done.cred.uid, "uid-9");
        assert_eq!(done.cred.domain, "www.codebuddy.cn");
        assert_eq!(done.cred.expires_at_ms, 1_010_000 + 2_592_000_000);
        assert_eq!(done.nickname, "小明");
    }

    // ==================== SignDock 自有凭据文件（可回写；产品文件仍只读） ====================

    fn managed(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("signdock-wb-managed-{tag}-{}.json", std::process::id()));
        p
    }

    #[test]
    fn managed_cred_roundtrip_keeps_refresh_token() {
        let p = managed("rt");
        let cred = WorkbuddyCred { access_token: "AT".into(), refresh_token: "RT".into(),
            uid: "u".into(), domain: "d".into(), nickname: String::new(),
            expires_at_ms: 1_700_000_000_000 };
        write_managed_cred(&p, &cred).unwrap();
        let back = read_managed_cred(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(back, cred);
    }

    #[test]
    fn managed_cred_missing_file_is_not_auth_expired_msg_with_token() {
        let err = read_managed_cred(Path::new("Z:\\no-such-cred.json")).unwrap_err();
        match err {
            AdapterError::AuthExpiredMsg(m) => assert!(!m.contains("Bearer")),
            e => panic!("{e:?}"),
        }
    }

    #[tokio::test]
    async fn resolve_prefers_managed_cred_over_product_auth_file() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .and(header("authorization", "Bearer FROM-OAUTH"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":true,"today_checked_in":false}})))
            .mount(&s).await;
        let auth = write_cred("prio", "FROM-FILE", "RT", future_ms());
        let m = managed("prio");
        write_managed_cred(&m, &WorkbuddyCred { access_token: "FROM-OAUTH".into(),
            refresh_token: "RT".into(), uid: String::new(), domain: String::new(),
            nickname: String::new(), expires_at_ms: future_ms() }).unwrap();
        let a = WorkbuddyAdapter::with_base_url(&s.uri())
            .with_cred_path(auth.clone()).with_managed_path(Some(m.clone()));
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        std::fs::remove_file(&auth).ok();
        std::fs::remove_file(&m).ok();
    }

    #[tokio::test]
    async fn managed_cred_refresh_writes_back_and_never_touches_auth_file() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(REFRESH_PATH))
            .and(header("x-refresh-token", "RT"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"accessToken":"NEW","refreshToken":"RT2","expiresIn":2_592_000}})))
            .mount(&s).await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .and(header("authorization", "Bearer NEW"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":true,"today_checked_in":false}})))
            .mount(&s).await;
        let auth = write_cred("wbak", "FROM-FILE", "RT", future_ms());
        let m = managed("wbak");
        write_managed_cred(&m, &WorkbuddyCred { access_token: "OLD".into(), refresh_token: "RT".into(),
            uid: "u".into(), domain: "d".into(), nickname: String::new(),
            expires_at_ms: past_ms() }).unwrap();
        let a = WorkbuddyAdapter::with_base_url(&s.uri())
            .with_cred_path(auth.clone()).with_managed_path(Some(m.clone()));
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        // 刷新结果必须落到 SignDock 自有文件（下次启动仍能自动续期）
        let after = read_managed_cred(&m).unwrap();
        assert_eq!(after.access_token, "NEW");
        assert_eq!(after.refresh_token, "RT2");
        // 产品 auth 文件绝不回写
        assert_eq!(read_cred(&auth).unwrap().access_token, "FROM-FILE");
        std::fs::remove_file(&auth).ok();
        std::fs::remove_file(&m).ok();
    }

    #[tokio::test]
    async fn falls_back_to_auth_file_when_managed_cred_absent() {        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .and(header("authorization", "Bearer FROM-FILE"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":true,"today_checked_in":true}})))
            .mount(&s).await;
        let auth = write_cred("fb", "FROM-FILE", "RT", future_ms());
        let a = WorkbuddyAdapter::with_base_url(&s.uri())
            .with_cred_path(auth.clone()).with_managed_path(Some(managed("absent")));
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::SignedToday);
        std::fs::remove_file(&auth).ok();
    }

    /// 本机人工诊断：仅申请一次 state（不登录、不取凭据）。只打印域名与长度——
    /// authUrl 里的 state 是短时登录凭据句柄，绝不打印取值。
    #[tokio::test]
    #[ignore = "请求真实端点，仅供 cargo test -- --ignored 手工执行"]
    async fn diag_live_oauth_start() {
        match WorkbuddyAdapter::new().oauth_start().await {
            Ok(s) => println!("OAUTH START OK: auth_url_host={} auth_url_len={} login_id_len={} expires_in={}",
                s.auth_url.split('/').nth(2).unwrap_or("?"), s.auth_url.len(),
                s.login_id.len(), s.expires_in),
            Err(e) => println!("OAUTH START ERR: {e}"),
        }
    }
}
