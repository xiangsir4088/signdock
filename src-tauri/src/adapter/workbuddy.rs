use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use chrono::{Local, TimeZone, Utc};
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use super::{
    transport_error, AdapterError, CreditsSnapshot, ProductAdapter, SignOutcome, SignStatus,
};

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
/// 签到后等资源汇总追上发放的固定等待（见 WorkbuddyAdapter::settle）
const SIGN_SETTLE: Duration = Duration::from_secs(5);

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
    let v = crate::secret::read_json(path).map_err(|e| match e {
        crate::secret::SecretError::NotFound => AdapterError::AuthExpiredMsg(
            "尚未登录 WorkBuddy；点「登录并获取 token」完成一次官方登录".into()),
        _ => AdapterError::AuthExpiredMsg("本地 WorkBuddy 登录凭据已损坏，请重新登录".into()),
    })?;
    let access_token = str_field(&v, "accessToken");
    if access_token.is_empty() {
        return Err(AdapterError::AuthExpiredMsg("本地 WorkBuddy 登录凭据不完整，请重新登录".into()));
    }
    let now_ms = now_secs() * 1000;
    let expires_at_ms = parse_timestamp_ms(v.get("expiresAt"))
        .or_else(|| v.get("expiresIn").and_then(Value::as_i64).map(|s| now_ms + s * 1000))
        .unwrap_or(0);
    let cred = WorkbuddyCred {
        access_token,
        refresh_token: str_field(&v, "refreshToken"),
        uid: str_field(&v, "uid"),
        domain: str_field(&v, "domain"),
        nickname: str_field(&v, "nickname"),
        expires_at_ms,
    };
    // 迁移前的老文件是明文：读到就当场换成封存件，而不是把用户踢回登录页重走一遍 OAuth
    if crate::secret::needs_upgrade(path) {
        let _ = write_managed_cred(path, &cred);
    }
    Ok(cred)
}

pub fn write_managed_cred(path: &Path, cred: &WorkbuddyCred) -> Result<(), AdapterError> {
    let v = json!({
        "accessToken": cred.access_token,
        "refreshToken": cred.refresh_token,
        "uid": cred.uid,
        "domain": cred.domain,
        "nickname": cred.nickname,
        "expiresAt": cred.expires_at_ms,
    });
    // 明文只存在于内存：落盘走 DPAPI（当前用户）封存，本机其它进程读不走
    crate::secret::write_json(path, &v).map_err(|_| AdapterError::AuthExpired)
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

/// 自有凭据（OAuth 登录那份）续期结果的短期缓存。
/// 键不含 mtime 而用写入时刻：这里要防的是"同一轮里连发三次 refresh"，而回写失败时
/// 盘上永远是那份旧的，跟着 mtime 走等于没有防线。
static MANAGED_CACHE: OnceLock<Mutex<HashMap<PathBuf, (SystemTime, WorkbuddyCred)>>> = OnceLock::new();
const MANAGED_CACHE_TTL: Duration = Duration::from_secs(60);

fn managed_cache() -> &'static Mutex<HashMap<PathBuf, (SystemTime, WorkbuddyCred)>> {
    MANAGED_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn cached_managed(path: &Path, now_ms: i64) -> Option<WorkbuddyCred> {
    let map = managed_cache().lock().ok()?;
    let (written_at, cred) = map.get(path)?;
    let fresh = written_at.elapsed().ok().is_some_and(|e| e < MANAGED_CACHE_TTL);
    fresh.then(|| cred.clone()).filter(|c| c.expires_at_ms > now_ms)
}

fn put_cached_managed(path: &Path, cred: &WorkbuddyCred) {
    if let Ok(mut map) = managed_cache().lock() {
        map.insert(path.to_path_buf(), (SystemTime::now(), cred.clone()));
    }
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

/// 当前积分 = Σ CycleRemainCapacity（recon 实测 7731.57 = 7231.57+500）
fn balance_of(v: &Value) -> f64 {
    packages_of(v).iter().fold(0.0f64, |acc, p| {
        acc + first_number(p, &["CycleRemainCapacity"]).unwrap_or(0.0)
    })
}

/// 500.0 → "500"；457.29 → "457.29"
fn trim_amount(n: f64) -> String {
    if n.fract() == 0.0 { format!("{}", n as i64) } else { format!("{n:.2}") }
}

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

/// OAuth 轮询时服务端回的「进行中」信号：必须当「还没点完、继续轮询」，
/// 不能当失败。否则一次短暂的进行中（如 code=11217 / msg 含 "login ing"）就被误判成
/// 登录失败，前端 invoke 抛错、登录流直接中断，用户永远拿不到 token
/// （GitHub issue 复现：浏览器里登录明明成功，点完即报 11217）。
fn is_login_pending(code: i64, msg: &str) -> bool {
    const PENDING_CODES: &[i64] = &[11217];
    if PENDING_CODES.contains(&code) {
        return true;
    }
    let m = msg.to_lowercase();
    m.contains("login ing")
        || m.contains("login in progress")
        || m.contains("pending")
        || m.contains("处理中")
}

pub fn read_cred(path: &Path) -> Result<WorkbuddyCred, AdapterError> {
    let raw = std::fs::read_to_string(path).map_err(|_| AdapterError::AuthExpired)?;
    let v: Value = serde_json::from_str(&raw).map_err(|_| AdapterError::AuthExpired)?;
    let auth = v.get("auth").cloned().unwrap_or(Value::Null);
    let account = v.get("account").cloned().unwrap_or(Value::Null);
    let access = auth.get("accessToken").cloned().unwrap_or(Value::Null);
    if is_wb_encrypted(&access) {
        return Err(AdapterError::AuthExpiredMsg(
            "WorkBuddy 5.6+ 已加密本机登录凭据，SignDock 读不到你网页登录的 token；请在设置中点「登录并获取 token」，用 SignDock 自有的官方登录页授权一次（与网页登录是两套独立会话，互不相通）".into(),
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
    /// 签到后等资源汇总反映出新发放再读第二次余额。发放是跨服务异步写入的，签到接口
    /// 刚返回时汇总常常还是旧值。测试里置零，免得每个用例都白等。
    settle: Duration,
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
               cred_path: None, managed_path: None, settle: SIGN_SETTLE }
    }

    pub fn with_resource_base(mut self, base: &str) -> Self {
        self.resource_base = base.trim_end_matches('/').to_string();
        self
    }

    pub fn with_settle(mut self, settle: Duration) -> Self { self.settle = settle; self }

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
        // 盘上这份已到续期窗口，但本进程可能刚刚才续过一次（status / sign / credits
        // 各读一遍同一文件）。
        if let Some(cached) = cached_managed(path, now) {
            return Ok(cached);
        }
        match self.refresh_cred(&cred).await {
            Ok(fresh) => {
                // 先入内存再回写：回写失败时这一份新 token 至少在本进程里不会被
                // 拿旧的 refresh token 反复消耗
                put_cached_managed(path, &fresh);
                if let Err(e) = write_managed_cred(path, &fresh) {
                    eprintln!("[signdock] workbuddy 续期结果回写失败：{e}（本次运行仍可用，下次启动需重新登录）");
                }
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
            // 绝大多数非零 code 是服务端明确回的失败（如 state 失效），应当作硬失败交前端提示。
            // 但「登录进行中」(code=11217 / msg 含 "login ing") 是轮询中途的正常瞬态，
            // 必须当成「用户还没点完」继续轮询——否则一次短暂的进行中就被误判成登录失败，
            // 浏览器里明明登录成功了，SignDock 却在拿到 token 之前中断（GitHub issue 复现）。
            if is_login_pending(code, &msg_of(&v)) {
                return Ok(None);
            }
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

    /// 当前余额。读失败一律吞成 None —— 它只服务于「本次到账多少」这一行展示，
    /// 绝不能因为读不到数字就把一次已经成功的签到报成失败。
    async fn balance(&self, cred: &WorkbuddyCred) -> Option<f64> {
        let v = self.post_json(&self.resource_base, RESOURCE_SUMMARY_PATH, cred, json!({})).await.ok()?;
        check_biz(&v, RESOURCE_SUMMARY_PATH).ok()?;
        Some(balance_of(&v))
    }

    /// 签到响应里没有任何数额字段（对照 workbuddy-switch：它也只记 success/already），
    /// 所以数额只能自己量：等发放跨服务写进资源汇总，再取前后差。
    /// 量不出来就说人话，不编一个 "+0积分" 给用户看。
    async fn signed_detail(&self, cred: &WorkbuddyCred, before: Option<f64>) -> String {
        tokio::time::sleep(self.settle).await;
        match (before, self.balance(cred).await) {
            (Some(b), Some(a)) if a > b => format!("+{}积分", trim_amount(a - b)),
            _ => "已签到（本次到账以余额为准）".into(),
        }
    }

    async fn post_json(&self, base: &str, api_path: &str, cred: &WorkbuddyCred, body: Value) -> Result<Value, AdapterError> {
        let resp = self.client
            .post(format!("{base}{api_path}"))
            .headers(self.headers_for(cred)).json(&body).send().await?;
        // workbuddy 无验证码机制：401/403 一律按鉴权失败（决策见计划 Global Constraints）
        if matches!(resp.status().as_u16(), 401 | 403) {
            return Err(AdapterError::AuthExpired);
        }
        if let Some(e) = transport_error(resp.status()) {
            return Err(e);
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
            // 活动没开 ≠ 读不懂：交给 WindowPending，调度器会按间隔回头复查
            return Ok(SignStatus::WindowPending);
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
        let before = self.balance(&cred).await;
        let v = self.post_meter(SIGN_PATH, &cred).await?;
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(-1);
        if code == 0 || code == 200 {
            return Ok(SignOutcome::Success(self.signed_detail(&cred, before).await));
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
        let balance = balance_of(&summary);

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
            .with_settle(Duration::ZERO)
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

    /// recon：data.active == false 表示活动**此刻没开**（每天定时开放），
    /// 不是"读不懂"。必须是 WindowPending，才能按 retry_interval_min 回头复查；
    /// 落到 Unknown 会直接终态，09:00 撞上没开的窗口就等于整天错过福利。
    #[tokio::test]
    async fn status_inactive_is_window_pending() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":false}})))
            .mount(&s).await;
        let cred = write_cred("inact", "AT", "RT", future_ms());
        assert_eq!(adapter(&s, cred.clone()).query_sign_status().await.unwrap(), SignStatus::WindowPending);
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
    async fn gateway_503_with_json_body_is_retryable_not_a_success() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .respond_with(ResponseTemplate::new(503)
                .set_body_json(json!({"code":0,"data":{"active":true,"today_checked_in":true}})))
            .mount(&s).await;
        let cred = write_cred("503", "AT", "RT", future_ms());
        let err = adapter(&s, cred.clone()).query_sign_status().await.unwrap_err();
        assert!(matches!(err, AdapterError::Http(_)), "5xx 是可重试的网络错误: {err:?}");
        std::fs::remove_file(&cred).ok();
    }

    fn summary_body(remain: f64) -> Value {
        json!({"code":0,"data":{"Packages":[{"PackageCode":"PKG","CycleRemainCapacity":remain}]}})
    }

    /// 签到响应里**没有**数额字段（workbuddy-switch 也只记 success/already），
    /// 所以 "+N积分" 只能自己量：签到前后各读一次余额，差值就是本次到账。
    #[tokio::test]
    async fn sign_success_reports_balance_delta() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(summary_body(7000.0)))
            .up_to_n_times(1).with_priority(1).mount(&s).await;
        Mock::given(method("POST")).and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(summary_body(7500.0)))
            .with_priority(2).mount(&s).await;
        Mock::given(method("POST")).and(path(SIGN_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"today_checked_in":true}})))
            .mount(&s).await;
        let cred = write_cred("delta", "AT", "RT", future_ms());
        let out = adapter(&s, cred.clone()).sign_in().await.unwrap();
        assert!(matches!(&out, SignOutcome::Success(d) if d.contains("500")), "{out:?}");
        std::fs::remove_file(&cred).ok();
    }

    /// 余额没动（或前后读平）时不能编一个 "+0积分" 出来。
    #[tokio::test]
    async fn sign_success_without_balance_movement_omits_amount() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(summary_body(7000.0)))
            .mount(&s).await;
        Mock::given(method("POST")).and(path(SIGN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{}})))
            .mount(&s).await;
        let cred = write_cred("flat", "AT", "RT", future_ms());
        let out = adapter(&s, cred.clone()).sign_in().await.unwrap();
        let d = match &out { SignOutcome::Success(d) => d, other => panic!("{other:?}") };
        assert!(!d.contains('0'), "不该出现凭空的数额: {d}");
        std::fs::remove_file(&cred).ok();
    }

    /// 读余额只是"顺带算数额"，它失败绝不能把一次已经成功的签到报成失败。
    #[tokio::test]
    async fn sign_success_survives_unreadable_balance() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(RESOURCE_SUMMARY_PATH))
            .respond_with(ResponseTemplate::new(500)).mount(&s).await;
        Mock::given(method("POST")).and(path(SIGN_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"code":0,"data":{}})))
            .mount(&s).await;
        let cred = write_cred("nobal", "AT", "RT", future_ms());
        assert!(matches!(adapter(&s, cred.clone()).sign_in().await, Ok(SignOutcome::Success(_))));
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

    /// 自有凭据里的 token 不该以任何人能读的形式躺在盘上
    #[test]
    fn managed_cred_is_sealed_at_rest() {
        let p = managed("seal");
        let cred = WorkbuddyCred { access_token: "SECRET-NEW".into(), refresh_token: "RT-SECRET".into(),
            uid: "u".into(), domain: "d".into(), nickname: String::new(), expires_at_ms: future_ms() };
        write_managed_cred(&p, &cred).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("SECRET-NEW") && !raw.contains("RT-SECRET"), "落盘内容里读得出 token");
        assert_eq!(read_managed_cred(&p).unwrap(), cred);
        std::fs::remove_file(&p).ok();
    }

    /// 迁移前的老文件是明文 JSON：读要能读，读完必须就地换成封存件，不能把用户踢回登录页
    #[test]
    fn legacy_plaintext_managed_cred_upgrades_on_read() {
        let p = managed("legacy");
        std::fs::write(&p, json!({
            "accessToken": "PLAIN-OLD", "refreshToken": "RT-PLAIN",
            "uid": "u", "domain": "d", "nickname": "小明", "expiresAt": future_ms(),
        }).to_string()).unwrap();
        let cred = read_managed_cred(&p).unwrap();
        assert_eq!(cred.access_token, "PLAIN-OLD");
        assert_eq!(cred.nickname, "小明");
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(raw.starts_with(crate::secret::ENVELOPE_PREFIX), "读过一次之后没换成封存件");
        assert!(!raw.contains("PLAIN-OLD"), "读过一次之后盘上仍留着明文");
        assert_eq!(read_managed_cred(&p).unwrap(), cred);
        std::fs::remove_file(&p).ok();
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

    /// 自有凭据续期后必须回写盘；回写一旦失败（磁盘/权限/杀软占用），新 token 只活在
    /// 这一次调用里，下一次读盘仍是那个已经用过的 stale refresh token。反复拿同一个
    /// stale token 续期有轮换失效风险，最终逼用户重新登录 —— 所以续期结果也要进缓存。
    /// 只读位是本用例里唯一能造出"回写失败"的手段（动的是测试自己的临时文件）
    #[allow(clippy::permissions_set_readonly_false)]
    fn set_ro(path: &std::path::Path, ro: bool) {
        let mut perms = std::fs::metadata(path).unwrap().permissions();
        perms.set_readonly(ro);
        std::fs::set_permissions(path, perms).unwrap();
    }

    #[tokio::test]
    async fn managed_renew_caches_fresh_token_so_one_round_sends_one_refresh() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(REFRESH_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"accessToken":"NEW","refreshToken":"RT2","expiresIn":3600}})))
            .mount(&s).await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":true,"today_checked_in":false}})))
            .mount(&s).await;
        let m = managed("cache");
        write_managed_cred(&m, &WorkbuddyCred { access_token: "OLD".into(), refresh_token: "RT".into(),
            uid: "u".into(), domain: "d".into(), nickname: String::new(),
            expires_at_ms: Utc::now().timestamp_millis() + 3_600_000 }).unwrap();
        let a = WorkbuddyAdapter::with_base_url(&s.uri()).with_managed_path(Some(m.clone()));
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        let refreshes = s.received_requests().await.unwrap().iter()
            .filter(|r| r.url.path() == REFRESH_PATH).count();
        assert_eq!(refreshes, 1, "一轮续期就够，第二次该直接用续出来的 token");
        std::fs::remove_file(&m).ok();
    }

    /// 回写失败正是缓存存在的理由：盘上留着的仍是那份已经用过的 refresh token，
    /// 若只信盘，每轮都会拿它去续一次。
    #[tokio::test]
    async fn managed_renew_keeps_fresh_token_in_memory_when_write_back_fails() {
        let s = MockServer::start().await;
        Mock::given(method("POST")).and(path(REFRESH_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"accessToken":"NEW","refreshToken":"RT2","expiresIn":3600}})))
            .mount(&s).await;
        Mock::given(method("POST")).and(path(STATUS_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"code":0,"data":{"active":true,"today_checked_in":false}})))
            .mount(&s).await;
        let m = managed("ro");
        write_managed_cred(&m, &WorkbuddyCred { access_token: "OLD".into(), refresh_token: "RT".into(),
            uid: "u".into(), domain: "d".into(), nickname: String::new(),
            expires_at_ms: Utc::now().timestamp_millis() + 3_600_000 }).unwrap();
        set_ro(&m, true);
        let a = WorkbuddyAdapter::with_base_url(&s.uri()).with_managed_path(Some(m.clone()));
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        assert_eq!(a.query_sign_status().await.unwrap(), SignStatus::NotSigned);
        let refreshes = s.received_requests().await.unwrap().iter()
            .filter(|r| r.url.path() == REFRESH_PATH).count();
        set_ro(&m, false);
        assert_eq!(refreshes, 1, "回写不动也不该反复消耗同一个 refresh token");
        assert_eq!(read_managed_cred(&m).unwrap().access_token, "OLD");
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
