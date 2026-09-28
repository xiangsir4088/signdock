//! Trae 适配器：解密本机 TRAE 桌面登录态（AES-128-CBC 信封）+ 签到/积分接口。
//! 设计决策见 docs/recon/trae.md：绝不调用 ExchangeToken 主动刷新；
//! token 过期只提示「打开 TRAE 让它自己刷新」。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha256, Sha512};

use super::{
    transport_error, AdapterError, CreditsSnapshot, ProductAdapter, SignOutcome, SignStatus,
};

pub const PROD_BASE: &str = "https://api.trae.cn";
const STATUS_PATH: &str = "/trae/api/v2/ug/checkin_credits/status";
const CLAIM_PATH: &str = "/trae/api/v2/ug/checkin_credits/claim";
const USAGE_PATH: &str = "/trae/api/v2/pay/ide_user_ent_usage";

const STORAGE_KEY: &str = "iCubeAuthInfo://icube.cloudide";
const STORAGE_VARIANTS: [&str; 3] = ["TRAE SOLO CN", "Trae CN", "Trae"];

/// 用户决策（2026-09-23）：过期不刷新，仅提示
pub const REFRESH_HINT: &str = "token 已过期，请打开 TRAE 让它自己刷新后重试";

/// TRAE 信封头（6 字节）
const HEADER: [u8; 6] = [116, 99, 5, 16, 0, 0];
const LEFT_SECRET: [u8; 64] = [
    82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251, 124, 227, 57, 130,
    155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203, 84, 123, 148, 50, 166, 194, 35, 61,
    238, 76, 149, 11, 66, 250, 195, 78, 8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73, 109,
    139, 209, 37,
];
const RIGHT_SECRET: [u8; 64] = [
    31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95, 96, 81, 127, 169, 25,
    181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239, 160, 224, 59, 77, 174, 42, 245, 176, 200,
    235, 187, 60, 131, 83, 153, 97, 23, 43, 4, 126, 186, 119, 214, 38, 225, 105, 20, 99, 85, 33,
    12, 125,
];

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

fn sha512(data: &[u8]) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(data);
    let out = h.finalize();
    let mut arr = [0u8; 64];
    arr.copy_from_slice(out.as_slice());
    arr
}

/// 解密 storage.json 里 iCubeAuthInfo 的 base64 信封 → payload JSON。
/// 信封 = HEADER(6) + randomKey(32) + AES-128-CBC( SHA512(payload)(64) ‖ payload )。
pub fn decrypt_auth_info(encoded: &str) -> Result<Value, AdapterError> {
    let envelope = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| AdapterError::SchemaChanged("TRAE 凭据信封不是合法 base64".into()))?;
    if envelope.len() <= 38 || envelope.get(0..6) != Some(&HEADER[..]) {
        return Err(AdapterError::SchemaChanged(
            "TRAE 凭据信封格式不受支持（可能已更换加密方式）".into(),
        ));
    }
    let random_key = &envelope[6..38];
    let mut secret = [0u8; 64];
    for i in 0..64 {
        secret[i] = LEFT_SECRET[i] ^ RIGHT_SECRET[i];
    }
    let mut derived_input = Vec::with_capacity(128);
    derived_input.extend_from_slice(&sha512(random_key));
    derived_input.extend_from_slice(&secret);
    let derived = sha512(&derived_input);
    let key = &derived[0..16];
    let iv = &derived[16..32];
    let mut buf = envelope[38..].to_vec();
    let plaintext = Aes128CbcDec::new(key.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| AdapterError::SchemaChanged("TRAE 凭据信封解密失败".into()))?;
    if plaintext.len() < 64 {
        return Err(AdapterError::SchemaChanged("TRAE 凭据信封内容过短".into()));
    }
    let (expected, payload) = plaintext.split_at(64);
    if expected != sha512(payload).as_slice() {
        return Err(AdapterError::SchemaChanged("TRAE 凭据完整性校验失败".into()));
    }
    let json: Value = serde_json::from_slice(payload)
        .map_err(|_| AdapterError::SchemaChanged("TRAE 凭据 payload 不是 JSON".into()))?;
    Ok(json)
}

/// 本机 TRAE 登录态（jwt 为无前缀裸 token；user_id 用于设备派生 seed；username 供界面显示）
#[derive(Debug, Clone)]
pub struct TraeCred {
    pub jwt: String,
    pub user_id: String,
    pub username: String,
}

/// 信封 payload → 界面账号标识：优先 account.username，退化到 userId
pub fn account_label_of(info: &Value) -> String {
    let name = info
        .pointer("/account/username")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim();
    if !name.is_empty() {
        return name.to_string();
    }
    info.get("userId").and_then(|x| x.as_str()).unwrap_or("").trim().to_string()
}

/// 宽松时间解析（RFC3339 / 常见 ISO）→ 毫秒时间戳
pub fn parse_time_ms(v: Option<&Value>) -> Option<i64> {
    let s = v?.as_str()?;
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    for fmt in &["%Y-%m-%dT%H:%M:%SZ", "%Y-%m-%dT%H:%M:%S%.3fZ", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(ndt.and_utc().timestamp_millis());
        }
    }
    None
}

fn jwt_payload(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// JWT payload.exp → 毫秒（秒/毫秒/字符串宽松兼容）；解不出返回 None（不视为过期）
pub fn jwt_exp_ms(token: &str) -> Option<i64> {
    let raw = jwt_payload(token)?.get("exp").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    })?;
    Some(if raw < 100_000_000_000 { raw * 1000 } else { raw })
}

/// JWT payload 的 data.id（回退 auth_id、sub）→ user_id
pub fn jwt_user_id(token: &str) -> Option<String> {
    let payload = jwt_payload(token)?;
    payload
        .get("data")
        .and_then(|d| d.get("id"))
        .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_i64().map(|n| n.to_string())))
        .or_else(|| payload.get("auth_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .or_else(|| payload.get("sub").and_then(|v| v.as_str()).map(|s| s.to_string()))
}

/// "Cloud-IDE-JWT x"/"Bearer x"/裸 token → 裸 token
pub fn strip_prefix(token: &str) -> &str {
    let t = token.trim();
    t.strip_prefix("Cloud-IDE-JWT ")
        .or_else(|| t.strip_prefix("Bearer "))
        .unwrap_or(t)
        .trim()
}

/// authorization 头的完整值：缺前缀时补 `Cloud-IDE-JWT `
pub fn normalize_full(jwt: &str) -> String {
    let t = jwt.trim();
    if t.starts_with("Cloud-IDE-JWT ") || t.starts_with("Bearer ") {
        t.to_string()
    } else {
        format!("Cloud-IDE-JWT {t}")
    }
}

fn appdata_root() -> Option<std::path::PathBuf> {
    std::env::var("APPDATA").ok().map(std::path::PathBuf::from)
}

fn storage_paths_in(appdata: Option<&Path>, override_path: Option<&Path>) -> Vec<PathBuf> {
    if let Some(p) = override_path {
        return vec![p.to_path_buf()];
    }
    let Some(appdata) = appdata else { return vec![] };
    STORAGE_VARIANTS
        .iter()
        .map(|v| appdata.join(v).join("User").join("globalStorage").join("storage.json"))
        .collect()
}

/// 信封 payload → 凭据。到期时间解析不出来时按「未过期」处理（与 JWT 分支同口径）。
fn cred_from_info(info: &Value, now_ms: i64) -> Result<TraeCred, AdapterError> {
    let jwt = strip_prefix(info.get("token").and_then(|v| v.as_str()).unwrap_or("")).to_string();
    if jwt.is_empty() {
        // 解出即定（2026-09-23 裁决）：信封可解密但缺 token 属格式突变，立即失败不回退下一目录
        return Err(AdapterError::SchemaChanged("TRAE 凭据缺少 token".into()));
    }
    let user_id = info.get("userId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
    let env_exp = parse_time_ms(info.get("expiredAt")).unwrap_or(i64::MAX);
    let jwt_exp = jwt_exp_ms(&jwt).unwrap_or(i64::MAX);
    if env_exp <= now_ms || jwt_exp <= now_ms {
        return Err(AdapterError::AuthExpiredMsg(REFRESH_HINT.into()));
    }
    Ok(TraeCred { username: account_label_of(info), jwt, user_id })
}

/// 读当前活动 TRAE 账号：按变体顺序取第一个存在且可解密的目录；
/// 解出即定（过期**不**回退下一目录）。now_ms 外传以便测试稳定。
pub fn read_local_credential_at(
    override_path: Option<&Path>,
    now_ms: i64,
) -> Result<TraeCred, AdapterError> {
    read_local_credential_in(appdata_root().as_deref(), override_path, now_ms)
}

/// 同上，但 APPDATA 根目录由调用方给出。测试专用注入点：进程内 `set_var("APPDATA")`
/// 会波及其他并行读取登录态的测试，是 flaky 的来源。
pub fn read_local_credential_in(
    appdata: Option<&Path>,
    override_path: Option<&Path>,
    now_ms: i64,
) -> Result<TraeCred, AdapterError> {
    let mut last_err: Option<AdapterError> = None;
    for path in storage_paths_in(appdata, override_path) {
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let Ok(storage) = serde_json::from_str::<Value>(&raw) else { continue };
        let Some(encoded) = storage.get(STORAGE_KEY).and_then(|v| v.as_str()) else { continue };
        match decrypt_auth_info(encoded) {
            Ok(info) => return cred_from_info(&info, now_ms),
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        AdapterError::AuthExpiredMsg("未找到 TRAE 登录信息，请打开 TRAE 登录后重试".into())
    }))
}

/// 确定性伪设备身份（同 userId 永远同值 → 无需持久化，见 recon「设备身份派生」）
#[derive(Debug, Clone)]
struct DeviceIdentity {
    device_id: String,
    market_user_id: String,
    session_id: String,
}

fn seeded_stream(seed: &str, salt: &str, nbytes: usize) -> Vec<u8> {
    let data = format!("{salt}:{seed}");
    let mut out = Vec::with_capacity(nbytes);
    let mut i: u32 = 0;
    while out.len() < nbytes {
        let mut hasher = Sha256::new();
        hasher.update(data.as_bytes());
        hasher.update(i.to_be_bytes());
        out.extend_from_slice(&hasher.finalize());
        i += 1;
    }
    out.truncate(nbytes);
    out
}

fn rand_digits(n: usize, seed: &str) -> String {
    let bs = seeded_stream(seed, "devid", n + 1);
    bs[..n].iter().map(|b| (b % 10).to_string()).collect()
}

fn rand_hex(n: usize, seed: &str) -> String {
    let bs = seeded_stream(seed, "sess", n.div_ceil(2));
    let mut s = String::with_capacity(n);
    for b in &bs {
        s.push_str(&format!("{b:02x}"));
    }
    s.truncate(n);
    s
}

fn gen_market_uuid(seed: &str) -> String {
    let mut bs = seeded_stream(seed, "market", 16);
    bs[6] = (bs[6] & 0x0F) | 0x40;
    bs[8] = (bs[8] & 0x3F) | 0x80;
    let hex: String = bs.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8], &hex[8..12], &hex[12..16], &hex[16..20], &hex[20..32]
    )
}

fn derive_device(user_id: &str) -> DeviceIdentity {
    let seed = if user_id.is_empty() { "signdock-anon" } else { user_id };
    DeviceIdentity {
        device_id: rand_digits(15, seed),
        market_user_id: gen_market_uuid(seed),
        session_id: rand_hex(64, seed),
    }
}

static REQ_SEQ: AtomicU64 = AtomicU64::new(0);

/// (x-tt-trace-id, x-request-id)：sha256(纳秒:序号) 手工造格式（无 uuid crate；仅需格式合理，无校验证据）
fn request_ids() -> (String, String) {
    let nanos = chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);
    let seq = REQ_SEQ.fetch_add(1, Ordering::Relaxed);
    let mut hasher = Sha256::new();
    hasher.update(format!("{nanos}:{seq}").as_bytes());
    let hex: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();
    (
        format!("00-{}-01", &hex[..16]),
        format!("{}-{}-{}-{}-{}", &hex[8..16], &hex[16..20], &hex[20..24], &hex[24..28], &hex[28..40]),
    )
}

/// code∈{0,200}（数字或字符串）/ success==true / status=="success"（trae-mate api_succeeded 同规则）
fn api_succeeded(v: &Value) -> bool {
    let code_ok = match v.get("code") {
        Some(Value::Number(n)) => n.as_i64().is_some_and(|c| c == 0 || c == 200),
        Some(Value::String(s)) => s == "0" || s == "200",
        _ => false,
    };
    code_ok
        || v.get("success").and_then(|x| x.as_bool()) == Some(true)
        || v.get("status").and_then(|x| x.as_str()) == Some("success")
}

fn bool_field(v: &Value, key: &str) -> Option<bool> {
    v.get(key).and_then(|x| x.as_bool())
}

fn msg_of(v: &Value) -> String {
    v.get("message")
        .and_then(|x| x.as_str())
        .unwrap_or("未知应答")
        .to_string()
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

/// 北京时间今日窗口 [start, end)（Unix 秒；固定 +08:00，某些环境系统时区不可信）
fn beijing_today_window(now: chrono::DateTime<chrono::Utc>) -> (i64, i64) {
    let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
    let start = now
        .with_timezone(&cst)
        .date_naive()
        .and_hms_opt(0, 0, 0)
        .unwrap()
        .and_local_timezone(cst)
        .unwrap()
        .timestamp();
    (start, start + 86_400)
}

pub struct TraeAdapter {
    client: reqwest::Client,
    base_url: String,
    storage_override: Option<PathBuf>,
}

impl Default for TraeAdapter {
    fn default() -> Self {
        Self { client: super::http_client(), base_url: PROD_BASE.into(), storage_override: None }
    }
}

impl TraeAdapter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }
    pub fn with_storage_path(mut self, path: PathBuf) -> Self {
        self.storage_override = Some(path);
        self
    }

    /// 凭证只有本机登录态一条来源：手工粘贴的 token 会压住产品自己刷新出来的新值
    fn resolve(&self) -> Result<TraeCred, AdapterError> {
        read_local_credential_at(self.storage_override.as_deref(), chrono::Utc::now().timestamp_millis())
    }

    fn headers(&self, cred: &TraeCred) -> Vec<(&'static str, String)> {
        let d = derive_device(&cred.user_id);
        let (trace, req_id) = request_ids();
        vec![
            ("authorization", normalize_full(&cred.jwt)),
            ("accept", "*/*".into()),
            ("accept-language", "zh-CN".into()),
            ("content-type", "application/json".into()),
            ("user-agent", "VSCode 1.107.1 (TRAE SOLO CN)".into()),
            ("x-market-client-id", "VSCode 1.107.1".into()),
            ("x-market-user-id", d.market_user_id),
            ("x-user-region", "CN".into()),
            ("x-device-id", d.device_id),
            ("x-lgw-req-sdk-type", "3".into()),
            ("package-type", "stable_cn".into()),
            ("x-lscbd-aid", "787976".into()),
            ("x-lscbd-platform", "windows".into()),
            ("app-version", "0.1.45".into()),
            ("x-tt-trace-id", trace),
            ("vscode-sessionid", d.session_id),
            ("x-request-id", req_id),
        ]
    }

    async fn post(&self, api_path: &str, cred: &TraeCred, body: Value) -> Result<Value, AdapterError> {
        let mut req = self.client.post(format!("{}{}", self.base_url, api_path));
        for (k, v) in self.headers(cred) {
            req = req.header(k, v);
        }
        let resp = req.json(&body).timeout(std::time::Duration::from_secs(30)).send().await?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(AdapterError::AuthExpiredMsg(REFRESH_HINT.into()));
        }
        if let Some(e) = transport_error(status) {
            return Err(e);
        }
        let v: Value = resp.json().await?;
        Ok(v)
    }
}

#[async_trait::async_trait]
impl ProductAdapter for TraeAdapter {
    fn id(&self) -> String { "trae".into() }

    async fn query_sign_status(&self) -> Result<SignStatus, AdapterError> {
        let cred = self.resolve()?;
        let v = self.post(STATUS_PATH, &cred, serde_json::json!({})).await?;
        if !api_succeeded(&v) {
            return Err(AdapterError::SchemaChanged(format!("status: {}", msg_of(&v))));
        }
        match bool_field(&v, "checked_in") {
            Some(true) => Ok(SignStatus::SignedToday),
            Some(false) => Ok(SignStatus::NotSigned),
            None => Err(AdapterError::SchemaChanged("status 响应缺少 checked_in".into())),
        }
    }

    async fn sign_in(&self) -> Result<SignOutcome, AdapterError> {
        let cred = self.resolve()?;
        let v = self.post(CLAIM_PATH, &cred, serde_json::json!({})).await?;
        if api_succeeded(&v) {
            let credits = v.get("credits").and_then(|x| x.as_f64()).unwrap_or(0.0);
            return Ok(SignOutcome::Success(if credits > 0.0 {
                format!("+{credits:.0}积分")
            } else {
                "+积分".into()
            }));
        }
        if bool_field(&v, "checked_in") == Some(true) || msg_of(&v).contains("已签到") {
            return Ok(SignOutcome::AlreadySigned);
        }
        Ok(SignOutcome::Failed(msg_of(&v)))
    }

    async fn fetch_credits(&self) -> Result<CreditsSnapshot, AdapterError> {
        let cred = self.resolve()?;
        let v = self
            .post(USAGE_PATH, &cred, serde_json::json!({ "require_usage": true, "req_source": 2 }))
            .await?;
        let packs = v
            .get("user_entitlement_pack_list")
            .and_then(|x| x.as_array())
            .ok_or_else(|| AdapterError::SchemaChanged("usage 响应缺少 user_entitlement_pack_list".into()))?;
        let now = chrono::Utc::now();
        let (today_start, today_end) = beijing_today_window(now);
        let mut balance = 0.0;
        let mut expiring = 0.0;
        let mut expiring_tomorrow = 0.0;
        for p in packs {
            let Some(limit) = p
                .pointer("/entitlement_base_info/quota/credits_limit")
                .and_then(|x| x.as_f64())
            else {
                continue;
            };
            let used = p
                .pointer("/usage/credits_amount")
                .and_then(|x| x.as_f64())
                .unwrap_or(0.0);
            let remain = (limit - used).max(0.0);
            balance += remain;
            if let Some(exp) = p.get("expire_time").and_then(|x| x.as_i64()) {
                if exp >= today_start && exp < today_end {
                    expiring += remain;
                } else if exp >= today_end && exp < today_end + 86_400 {
                    expiring_tomorrow += remain;
                }
            }
        }
        Ok(CreditsSnapshot {
            balance: round2(balance),
            today_used: 0.0,
            expiring_today: round2(expiring),
            expiring_tomorrow: round2(expiring_tomorrow),
            fetched_at_ms: now.timestamp_millis(),
        })
    }

    async fn account_label(&self) -> Result<String, AdapterError> {
        let cred = self.resolve()?;
        if !cred.username.is_empty() {
            return Ok(cred.username);
        }
        let label = cred.user_id.trim();
        if label.is_empty() {
            return Err(AdapterError::SchemaChanged("TRAE 登录信息里没有账号标识".into()));
        }
        Ok(label.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 由 scripts/gen_trae_test_envelopes.mjs 以固定 embedded_key 加密的假凭证（不含真实 token）
    pub const ENVELOPE_VALID: &str = "dGMFEAAABwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwflMiwPUbmm/55iGLIVK+MVhwxJ6h+/yYAtI46lXl358O6PnsYIoS44h6mjpVUzuDDnc6QmqJygB6x3uX+jcwfaFNIFAJ2R26Z1iQyGPefD6AUxDsutUWn9LEWPlSAPFlTMkkr39Cx9gkx01BRoMJDjCudtbXG7AcNGeXPon62apJT9SnA5iIkGuWfYAwyZbhYWfGf3vNe3ERkb26MFdQ+nx/C+Yr7QC6TMaWDVxMxdJflKrCdrKRMZi5LfpZLt3dBtj37azklyIYjpDucs3KeDknmHfKy2dSTAHKezIk35Znl6Ofstjtn6W1kDYcw2dlRlS5brDgFsNeBQXiJoHZhGj/BBWb9cO+/JnDRKJ1pnn6YNRqoy7yvl9IkNs4lAetcgRpA3gQoJBx5KdOpcKSPe1G18TRl+9G6nawtnaXCx/g==";
    pub const ENVELOPE_EXPIRED: &str = "dGMFEAAACQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQl/9cOgu58jZV+HL8+M9HidVON3hupmB62m1LpDoVn+M/bUZL9iI6Lrc6hhrn9d8ivHZ7WOkMAPpcUaXtHooai2lxYB7/fvTzPrw5Ra1ABDOBkV9fmNgAzPngJ06t8PNr5CQiFEMtSWUhCH74aCeaLlBdgppKxUwYEV0HEHdKZOottbSz4oWhZcUVqBzKsb9gPHZ+2SrzX7FHIUiB1gVQrasLTLVRmCdGDRQcj2c6I6BUZF7E2L354obfDQzvuvlD8ikvjuf+X2GOQArzARXCwW6jEsvWRRKjrwQ80qSMzJqynnBPH9UpQ2NLNi+9NHon/8QEbQRiFIKzMmzX5dRQTJgsU6QbvQSqfYbTVPvdJwx/MsY0uNJ67/USuiZNH6Pwfzx+y9fLtOlt2NlfZeQtIxhwBnKL9AVe7R1wCrgaVgbA==";
    /// payload 与 ENVELOPE_VALID 同构但缺 token 字段（embedded key 0x0B×32，expiredAt=2100 年）
    pub const ENVELOPE_NO_TOKEN: &str = "dGMFEAAACwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwsLCwvajck2Cx1F3mIK+mbXoBNqX6PEWzxn0+oXnFf+PJh1OIZ0VnyYu5uiAhRxjL+Eiwq3gzr+xdpWBjSgvYD8jPZmx7RgLKFe9D9jmvTqHtpYSrO99KIBszkMaxawpUzN+Po2QnYQ85LJcNjrqb/zEwHhKDp3x8uaINJUm1c9ynJZQUyhZL/HUjgtw48/CbRjy8ngJdTzd4ba8N/echc+dRgAkiCCz8xq+WgsfhxEQbUdA37Vud6H3NxUux98U9zQRmQiFHvPX2+vx/i5Y6k2fvFPgR4Nin0WYwKpYRaqXaRoJT+fwC1y9GE/PTjZfRhRjyMlyCLMR5FCW7aSqlaDs6C/wGyFgrLzpqIQeADY1yPyCkyEPae+L0RIGfm6pRDaJm7+ciGT4G1HnuhLT2Uqy9xN";

    /// 固定"现在"=2026-09-23T00:00:00Z（ENVELOPE_VALID 到期于 2026-10-23，过期信封到期于 2026-09-22）
    pub fn now_ms() -> i64 {
        chrono::DateTime::parse_from_rfc3339("2026-09-23T00:00:00Z").unwrap().timestamp_millis()
    }

    pub fn write_storage(dir: &Path, envelope: &str) -> PathBuf {
        let p = dir.join("storage.json");
        std::fs::write(&p, format!("{{\"{STORAGE_KEY}\":\"{envelope}\"}}")).unwrap();
        p
    }

    #[test]
    fn decrypt_returns_payload_fields() {
        let info = decrypt_auth_info(ENVELOPE_VALID).unwrap();
        assert_eq!(info["token"], "TK-VALID");
        assert_eq!(info["userId"], "1234567890123456");
        assert_eq!(info["host"], "https://api.trae.cn");
    }

    #[test]
    fn decrypt_rejects_bad_header() {
        let envelope = base64::engine::general_purpose::STANDARD.encode([0u8; 80]);
        assert!(matches!(decrypt_auth_info(&envelope), Err(AdapterError::SchemaChanged(_))));
    }

    #[test]
    fn decrypt_rejects_non_base64() {
        assert!(matches!(decrypt_auth_info("!!!not base64!!!"), Err(AdapterError::SchemaChanged(_))));
    }

    #[test]
    fn read_local_accepts_valid_envelope_with_fixed_now() {
        let dir = tempfile::tempdir().unwrap();
        let storage = write_storage(dir.path(), ENVELOPE_VALID);
        let cred = read_local_credential_at(Some(&storage), now_ms()).unwrap();
        assert_eq!(cred.jwt, "TK-VALID");
        assert_eq!(cred.user_id, "1234567890123456");
    }

    #[test]
    fn read_local_expired_envelope_is_auth_expired_msg() {
        let dir = tempfile::tempdir().unwrap();
        let storage = write_storage(dir.path(), ENVELOPE_EXPIRED);
        let err = read_local_credential_at(Some(&storage), now_ms()).unwrap_err();
        assert!(matches!(&err, AdapterError::AuthExpiredMsg(m) if m.contains("请打开 TRAE")));
    }

    #[test]
    fn read_local_missing_file_is_auth_expired_with_hint() {
        let err = read_local_credential_at(Some(Path::new("X:/definitely/not/here/storage.json")), now_ms()).unwrap_err();
        assert!(matches!(&err, AdapterError::AuthExpiredMsg(m) if m.contains("未找到")));
    }

    #[test]
    fn read_local_unsupported_header_surfaces_schema_changed() {
        // 模拟 "Trae CN" 新版信封（头不匹配）：解密错误记为 last_err，最终上浮而非误报"未找到"
        let dir = tempfile::tempdir().unwrap();
        let bogus = base64::engine::general_purpose::STANDARD.encode([7u8; 80]);
        let storage = write_storage(dir.path(), &bogus);
        let err = read_local_credential_at(Some(&storage), now_ms()).unwrap_err();
        assert!(matches!(err, AdapterError::SchemaChanged(_)));
    }

    #[test]
    fn read_local_missing_token_is_schema_changed() {
        // 解出即定：payload 可解密但缺 token → 直接 SchemaChanged，不继续探测/不误报"未找到"
        let dir = tempfile::tempdir().unwrap();
        let storage = write_storage(dir.path(), ENVELOPE_NO_TOKEN);
        let err = read_local_credential_at(Some(&storage), now_ms()).unwrap_err();
        assert!(matches!(&err, AdapterError::SchemaChanged(m) if m.contains("缺少 token")), "实得 {err:?}");
    }

    /// 到期字段解析不出来 ≠ 已过期：与 JWT 分支、qoder 的 i64::MAX 口径一致，
    /// 否则厂商一次格式变更（换日期格式/改字段名）就会把有效 token 判成过期。
    #[test]
    fn missing_expiry_field_is_not_treated_as_expired() {
        let info = serde_json::json!({
            "token": "Cloud-IDE-JWT abc.def.ghi", "userId": "u1", "account": { "username": "tester" }
        });
        assert!(matches!(cred_from_info(&info, now_ms()), Ok(c) if c.username == "tester"));
    }

    #[test]
    fn account_label_prefers_username_then_full_user_id() {
        assert_eq!(
            account_label_of(&serde_json::json!({ "account": { "username": "tester" }, "userId": "1234567890123456" })),
            "tester"
        );
        assert_eq!(account_label_of(&serde_json::json!({ "userId": "1234567890123456" })), "1234567890123456");
        assert_eq!(account_label_of(&serde_json::json!({})), "");
    }

    #[test]
    fn adapter_account_label_comes_from_local_envelope() {
        let dir = tempfile::tempdir().unwrap();
        let storage = write_storage(dir.path(), ENVELOPE_VALID);
        let a = TraeAdapter::new().with_storage_path(storage);
        assert_eq!(rt().block_on(a.account_label()).unwrap(), "tester");
    }

    #[test]
    fn read_local_missing_token_does_not_fall_through_to_next_variant() {
        // 解出即定：首个可解密目录缺 token 即失败，不得回退到后续变体目录（旧 continue 逻辑会误回退）
        let root = tempfile::tempdir().unwrap();
        let mk = |variant: &str, envelope: &str| {
            let d = root.path().join(variant).join("User").join("globalStorage");
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(
                d.join("storage.json"),
                format!("{{\"{STORAGE_KEY}\":\"{envelope}\"}}"),
            ).unwrap();
        };
        mk(STORAGE_VARIANTS[0], ENVELOPE_NO_TOKEN); // 先命中：可解密但缺 token
        mk(STORAGE_VARIANTS[1], ENVELOPE_VALID); // 若错误地 continue，会在这里"成功"
        let err = read_local_credential_in(Some(root.path()), None, now_ms()).unwrap_err();
        assert!(matches!(&err, AdapterError::SchemaChanged(m) if m.contains("缺少 token")), "实得 {err:?}");
    }

    #[test]
    fn parse_time_accepts_rfc3339_and_naive() {
        assert!(parse_time_ms(Some(&Value::String("2026-10-23T06:53:44.733Z".into()))).unwrap() > 0);
        assert!(parse_time_ms(Some(&Value::String("not-a-date".into()))).is_none());
        assert!(parse_time_ms(None).is_none());
    }

    #[test]
    fn jwt_exp_accepts_seconds_millis_and_none_for_bare_token() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"data":{"id":"7"},"exp":2000000000}"#);
        assert_eq!(jwt_exp_ms(&format!("aa.{payload}.bb")), Some(2000000000 * 1000));
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"exp":2000000000000}"#);
        assert_eq!(jwt_exp_ms(&format!("aa.{payload}.bb")), Some(2000000000000));
        assert_eq!(jwt_exp_ms("TK-VALID"), None);
    }

    #[test]
    fn jwt_user_id_reads_data_id_with_fallbacks() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"data":{"id":"1234567890123456"}}"#);
        assert_eq!(jwt_user_id(&format!("aa.{payload}.bb")).as_deref(), Some("1234567890123456"));
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"sub":"u9"}"#);
        assert_eq!(jwt_user_id(&format!("aa.{payload}.bb")).as_deref(), Some("u9"));
        assert_eq!(jwt_user_id("bare"), None);
    }

    #[test]
    fn strip_and_normalize_prefixes() {
        assert_eq!(strip_prefix("Cloud-IDE-JWT abc"), "abc");
        assert_eq!(strip_prefix("  Bearer abc "), "abc");
        assert_eq!(strip_prefix("abc"), "abc");
        assert_eq!(normalize_full("abc"), "Cloud-IDE-JWT abc");
        assert_eq!(normalize_full("Cloud-IDE-JWT abc"), "Cloud-IDE-JWT abc");
    }

    #[test]
    fn device_derivation_matches_reference_vectors() {
        assert_eq!(rand_digits(15, "1234567890123456"), "413174708280782");
        assert_eq!(
            rand_hex(64, "1234567890123456"),
            "fbabf7aa1e173b90385c623e1ec49157860cc07a4b01f53f7ca141b17d876eae"
        );
        assert_eq!(gen_market_uuid("1234567890123456"), "746608f9-7f37-4960-b4c7-8553cec6d366");
        assert_eq!(rand_digits(15, "9876543210"), "630512735296035");
        assert_eq!(
            rand_hex(64, "9876543210"),
            "78e8868abb83fdf82f3852e9563b07184ef8deeb462568d852affd8d59a4bbd9"
        );
        assert_eq!(gen_market_uuid("9876543210"), "c2aec55c-c9bb-4f68-b068-b8fa183b958b");
    }

    #[test]
    fn market_uuid_is_v4_shaped_and_stable() {
        let u = gen_market_uuid("1234567890123456");
        assert_eq!(&u[14..15], "4");
        assert!(matches!(&u[19..20], "8" | "9" | "a" | "b"));
        assert_eq!(u, gen_market_uuid("1234567890123456"));
    }

    #[test]
    fn request_ids_shape_and_uniqueness() {
        let (t1, r1) = request_ids();
        let (t2, r2) = request_ids();
        // recon：`00-<16hex>-01` 恒为 22 字符（计划笔误写成 23，按实测格式修正断言）
        assert!(t1.starts_with("00-") && t1.ends_with("-01") && t1.len() == 22, "trace 格式: {t1}");
        assert_eq!(r1.matches('-').count(), 4, "request id 应为 8-4-4-4-12: {r1}");
        assert_ne!((t1, r1), (t2, r2));
    }

    use tokio::runtime::Runtime;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn rt() -> Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    async fn mount_json(s: &MockServer, api_path: &str, body: Value) {
        Mock::given(method("POST"))
            .and(path(api_path))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(s)
            .await;
    }

    fn trae_adapter(s: &MockServer, storage: PathBuf) -> TraeAdapter {
        TraeAdapter::new().with_base_url(s.uri()).with_storage_path(storage)
    }

    /// 本机已登录 TRAE 的夹具：storage.json 里放一份有效信封。
    /// TempDir 必须由调用方持有到断言结束（信封是从那个目录里读的）。
    fn trae_logged_in(s: &MockServer) -> (tempfile::TempDir, TraeAdapter) {
        let dir = tempfile::tempdir().unwrap();
        let adapter = trae_adapter(s, write_storage(dir.path(), ENVELOPE_VALID));
        (dir, adapter)
    }

    #[test]
    fn status_signed_today_sends_full_headers() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_json(&s, STATUS_PATH, serde_json::json!({"checked_in": true, "code": 0})));
        let dir = tempfile::tempdir().unwrap();
        let storage = write_storage(dir.path(), ENVELOPE_VALID);
        let adapter = trae_adapter(&s, storage);
        let status = rt().block_on(adapter.query_sign_status()).unwrap();
        assert_eq!(status, SignStatus::SignedToday);
        let reqs = rt().block_on(s.received_requests()).unwrap();
        let h = &reqs[0].headers;
        assert_eq!(h["authorization"].to_str().unwrap(), "Cloud-IDE-JWT TK-VALID");
        assert_eq!(h["x-device-id"].to_str().unwrap(), "413174708280782"); // uid 1234567890123456 金标
        assert_eq!(h["x-market-user-id"].to_str().unwrap(), "746608f9-7f37-4960-b4c7-8553cec6d366");
        assert_eq!(h["vscode-sessionid"].to_str().unwrap(), "fbabf7aa1e173b90385c623e1ec49157860cc07a4b01f53f7ca141b17d876eae");
        assert_eq!(h["x-user-region"].to_str().unwrap(), "CN");
        assert_eq!(h["package-type"].to_str().unwrap(), "stable_cn");
        assert!(h["x-tt-trace-id"].to_str().unwrap().starts_with("00-"));
    }

    #[test]
    fn status_not_signed_maps_to_not_signed() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_json(&s, STATUS_PATH, serde_json::json!({"checked_in": false, "code": 0})));
        let (_login, adapter) = trae_logged_in(&s);
        let status = rt().block_on(adapter.query_sign_status()).unwrap();
        assert_eq!(status, SignStatus::NotSigned);
    }

    #[test]
    fn status_business_code_is_schema_changed() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_json(&s, STATUS_PATH, serde_json::json!({"code": 500, "message": "internal"})));
        let (_login, adapter) = trae_logged_in(&s);
        let err = rt().block_on(adapter.query_sign_status()).unwrap_err();
        assert!(matches!(err, AdapterError::SchemaChanged(_)));
    }

    #[test]
    fn status_missing_checked_in_is_schema_changed() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_json(&s, STATUS_PATH, serde_json::json!({"code": 0})));
        let (_login, adapter) = trae_logged_in(&s);
        let err = rt().block_on(adapter.query_sign_status()).unwrap_err();
        assert!(matches!(err, AdapterError::SchemaChanged(_)));
    }

    #[test]
    fn claim_success_reports_credits() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_json(&s, CLAIM_PATH, serde_json::json!({"code": 0, "credits": 150})));
        let (_login, adapter) = trae_logged_in(&s);
        let out = rt().block_on(adapter.sign_in()).unwrap();
        assert_eq!(out, SignOutcome::Success("+150积分".into()));
    }

    #[test]
    fn claim_without_credits_field_falls_back_to_plain_success() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_json(&s, CLAIM_PATH, serde_json::json!({"code": "0"})));
        let (_login, adapter) = trae_logged_in(&s);
        let out = rt().block_on(adapter.sign_in()).unwrap();
        assert_eq!(out, SignOutcome::Success("+积分".into()));
    }

    #[test]
    fn claim_already_signed_maps_to_already_signed() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_json(&s, CLAIM_PATH, serde_json::json!({"code": 1001, "checked_in": true, "message": "今日已签到"})));
        let (_login, adapter) = trae_logged_in(&s);
        let out = rt().block_on(adapter.sign_in()).unwrap();
        assert_eq!(out, SignOutcome::AlreadySigned);
    }

    #[test]
    fn claim_business_fail_maps_to_failed() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_json(&s, CLAIM_PATH, serde_json::json!({"code": 1005, "message": "活动未开始"})));
        let (_login, adapter) = trae_logged_in(&s);
        let out = rt().block_on(adapter.sign_in()).unwrap();
        assert!(matches!(out, SignOutcome::Failed(m) if m == "活动未开始"));
    }

    #[test]
    fn claim_activity_ended_maps_to_failed_not_already_signed() {
        // 「活动已结束」含"已"但不含"已签到"：旧 contains('已') 判定会误判为 AlreadySigned
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_json(&s, CLAIM_PATH, serde_json::json!({"code": 1001, "message": "活动已结束"})));
        let (_login, adapter) = trae_logged_in(&s);
        let out = rt().block_on(adapter.sign_in()).unwrap();
        assert!(matches!(&out, SignOutcome::Failed(m) if m == "活动已结束"), "期望 Failed，实得 {out:?}");
    }

    #[test]
    fn http_401_maps_to_auth_expired_msg() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(
            Mock::given(method("POST"))
                .and(path(STATUS_PATH))
                .respond_with(ResponseTemplate::new(401))
                .mount(&s),
        );
        let (_login, adapter) = trae_logged_in(&s);
        let err = rt().block_on(adapter.query_sign_status()).unwrap_err();
        assert!(matches!(&err, AdapterError::AuthExpiredMsg(m) if m.contains("请打开 TRAE")));
    }

    #[test]
    fn gateway_503_with_json_body_is_retryable_not_a_success() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(
            Mock::given(method("POST"))
                .and(path(STATUS_PATH))
                .respond_with(ResponseTemplate::new(503).set_body_json(serde_json::json!({"code": 0, "checked_in": true})))
                .mount(&s),
        );
        let (_login, adapter) = trae_logged_in(&s);
        let err = rt().block_on(adapter.query_sign_status()).unwrap_err();
        assert!(matches!(err, AdapterError::Http(_)), "5xx 是可重试的网络错误: {err:?}");
    }

    #[test]
    fn local_expired_storage_maps_to_refresh_hint() {
        // ENVELOPE_EXPIRED（expiredAt=2026-09-22）在真实时钟下必然已过期
        let s = rt().block_on(MockServer::start());
        let dir = tempfile::tempdir().unwrap();
        let storage = write_storage(dir.path(), ENVELOPE_EXPIRED);
        let adapter = trae_adapter(&s, storage);
        let err = rt().block_on(adapter.query_sign_status()).unwrap_err();
        assert!(matches!(&err, AdapterError::AuthExpiredMsg(m) if m == REFRESH_HINT));
    }

    use wiremock::matchers::body_json;

    /// 北京时间今日 23:00（固定落于今日窗口内，任何时刻跑测试都成立）
    fn beijing_today_23() -> i64 {
        let cst = chrono::FixedOffset::east_opt(8 * 3600).unwrap();
        let start = chrono::Utc::now()
            .with_timezone(&cst)
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_local_timezone(cst)
            .unwrap()
            .timestamp();
        start + 23 * 3600
    }

    fn usage_body() -> Value {
        serde_json::json!({
            "user_entitlement_pack_list": [
                { "expire_time": beijing_today_23(),
                  "usage": { "credits_amount": 400.0 },
                  "entitlement_base_info": { "quota": { "credits_limit": 1000.0 } } },
                { "expire_time": chrono::Utc::now().timestamp() + 86_400,
                  "usage": { "credits_amount": 0.0 },
                  "entitlement_base_info": { "quota": { "credits_limit": 500.5 } } },
                { "expire_time": chrono::Utc::now().timestamp() + 7200,
                  "entitlement_base_info": {} },
                { "expire_time": chrono::Utc::now().timestamp() + 3600,
                  "usage": { "credits_amount": 200.0 },
                  "entitlement_base_info": { "quota": { "credits_limit": 100.0 } } }
            ]
        })
    }

    #[test]
    fn credits_balance_and_expiring_today() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_json(&s, USAGE_PATH, usage_body()));
        let (_login, adapter) = trae_logged_in(&s);
        let snap = rt().block_on(adapter.fetch_credits()).unwrap();
        assert_eq!(snap.balance, 1100.5); // 600 + 500.5 + 0（第4包超额归零），第3包无 quota 忽略
        assert_eq!(snap.expiring_today, 600.0); // 仅北京今日 23:00 到期的包
        assert_eq!(snap.expiring_tomorrow, 500.5); // 现在+24h 必落在北京明日窗口
        assert_eq!(snap.today_used, 0.0); // 无 API 字段，恒为 0（决策）
        assert!(snap.fetched_at_ms > 0);
    }

    #[test]
    fn credits_sends_require_usage_body() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(
            Mock::given(method("POST"))
                .and(path(USAGE_PATH))
                .and(body_json(serde_json::json!({"require_usage": true, "req_source": 2})))
                .respond_with(ResponseTemplate::new(200).set_body_json(usage_body()))
                .mount(&s),
        );
        let (_login, adapter) = trae_logged_in(&s);
        rt().block_on(adapter.fetch_credits()).unwrap();
    }

    #[test]
    fn credits_missing_pack_list_is_schema_changed() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_json(&s, USAGE_PATH, serde_json::json!({"code": 0})));
        let (_login, adapter) = trae_logged_in(&s);
        let err = rt().block_on(adapter.fetch_credits()).unwrap_err();
        assert!(matches!(err, AdapterError::SchemaChanged(_)));
    }

    #[test]
    fn credits_401_is_auth_expired_msg() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(
            Mock::given(method("POST"))
                .and(path(USAGE_PATH))
                .respond_with(ResponseTemplate::new(401))
                .mount(&s),
        );
        let (_login, adapter) = trae_logged_in(&s);
        let err = rt().block_on(adapter.fetch_credits()).unwrap_err();
        assert!(matches!(&err, AdapterError::AuthExpiredMsg(m) if m.contains("请打开 TRAE")));
    }

    /// 本机人工诊断：只读磁盘、绝不联网、只打印长度与时间戳，绝不输出凭据取值。
    #[test]
    #[ignore = "读本机真实 TRAE 存储，仅供 cargo test -- --ignored 手工执行"]
    fn diag_local_trae_state() {
        use chrono::{Local, TimeZone, Utc};
        let now = Utc::now().timestamp_millis();
        let f = |ms: i64| Local.timestamp_millis_opt(ms).single()
            .map(|d| d.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| format!("(非法 {ms})"));
        for path in storage_paths_in(appdata_root().as_deref(), None) {
            println!("[{}] exists={}", path.display(), path.exists());
            let Ok(raw) = std::fs::read_to_string(&path) else { continue };
            let Ok(storage) = serde_json::from_str::<Value>(&raw) else {
                println!("  storage.json 解析失败"); continue;
            };
            match storage.get(STORAGE_KEY) {
                Some(Value::String(enc)) => match decrypt_auth_info(enc) {
                    Ok(info) => {
                        let jwt = strip_prefix(info.get("token").and_then(|v| v.as_str()).unwrap_or("")).to_string();
                        let env_exp = parse_time_ms(info.get("expiredAt")).unwrap_or(0);
                        let jwt_exp = jwt_exp_ms(&jwt).unwrap_or(i64::MAX);
                        println!("  token_len={} 有refreshToken={} expiredAt={} jwt_exp={} 信封已过期={} JWT已过期={}",
                            jwt.len(),
                            info.get("refreshToken").and_then(|v| v.as_str()).map(|s| !s.is_empty()).unwrap_or(false),
                            f(env_exp), f(jwt_exp), env_exp <= now, jwt_exp <= now);
                    }
                    Err(e) => println!("  信封解密失败: {e}"),
                },
                Some(Value::Object(o)) => println!("  字段是对象（格式可能已变）keys={:?}", o.keys().collect::<Vec<_>>()),
                _ => println!("  无该键（未登录过该变体？）"),
            }
        }
    }

    /// 真实主机**只读**探测：仅查签到状态（幂等，不领积分），输出只有状态与服务器文案，不含凭据。
    #[test]
    #[ignore = "会打真实 status 接口，仅供 cargo test -- --ignored 手工执行"]
    fn diag_live_status_real_host() {
        match rt().block_on(TraeAdapter::new().query_sign_status()) {
            Ok(s) => println!("STATUS OK: {s:?}"),
            Err(e) => println!("STATUS ERR: {e}"),
        }
    }

    /// 真实主机 claim 探测（每日一次的正常领积分，服务器幂等）：只打印判定结果与服务器文案。
    #[test]
    #[ignore = "会真实领一次当日积分（幂等），仅供 cargo test -- --ignored 手工执行"]
    fn diag_live_claim_real_host() {
        match rt().block_on(TraeAdapter::new().sign_in()) {
            Ok(o) => println!("CLAIM OK: {o:?}"),
            Err(e) => println!("CLAIM ERR: {e}"),
        }
    }
}
