//! Qoder CN 适配器：本机 DPAPI+os_crypt(v10) 登录态 + 活动签到/积分接口。
//! 实测依据见 docs/recon/qoder.md。

use std::path::{Path, PathBuf};

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use serde_json::Value;

use super::trae::parse_time_ms;
use super::{
    transport_error, AdapterError, CreditsSnapshot, ProductAdapter, SignOutcome, SignStatus,
};

pub const PROD_BASE: &str = "https://openapi.qoder.com.cn";
const CAMPAIGNS_PATH: &str = "/sash/api/v1/me/campaigns";
const USAGE_PATH: &str = "/sash/api/v2/me/usage";

const DATA_DIR_NAME: &str = "com.qodercn.app.stable";
const LOCAL_STATE_FILE: &str = "Local State";
const AUTH_FILE: &str = "auth.v1.dat";
const OS_CRYPT_TAG: &[u8] = b"v10";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const DPAPI_PREFIX: &[u8] = b"DPAPI";

/// 用户决策（同 trae）：不调 deviceToken/refresh，过期只提示打开产品
pub const REFRESH_HINT: &str = "Qoder 登录态已过期，请打开 Qoder 让它自己刷新后重试";
/// 领取窗口外（福利每天北京时间 10:00 刷新）时的可自处理文案
pub const NO_WINDOW_HINT: &str =
    "Qoder 今日福利还没开放（每天北京时间 10:00 刷新），请把签到时间设在 10:00 之后";
pub const NOT_LOGGED_IN_HINT: &str =
    "未读到 Qoder 本机登录态，请先打开 Qoder 登录后重试";

#[derive(Debug, Clone, PartialEq)]
pub struct QoderCred {
    pub token: String,
    pub expires_at_ms: i64,
    /// 界面显示的账号标识（user.name，退化到 user.id）
    pub label: String,
}

pub fn qoder_data_dir() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(Path::new(&appdata).join(DATA_DIR_NAME))
}

/// Chromium os_crypt v10：`v10` + nonce(12) + AES-256-GCM(明文 ‖ tag(16))，无 AAD
pub fn decrypt_os_crypt(key: &[u8], blob: &[u8]) -> Result<Vec<u8>, AdapterError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| AdapterError::SchemaChanged("Qoder 数据密钥长度不受支持".into()))?;
    if blob.len() < OS_CRYPT_TAG.len() + NONCE_LEN + TAG_LEN || &blob[..3] != OS_CRYPT_TAG {
        return Err(AdapterError::SchemaChanged(
            "Qoder 凭据格式不受支持（可能已更换加密方式）".into(),
        ));
    }
    let (nonce, ct) = blob[3..].split_at(NONCE_LEN);
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| AdapterError::SchemaChanged("Qoder 凭据解密失败".into()))
}

/// auth.v1.dat 明文 → 凭证（时钟由调用方给，便于测试）
pub fn parse_auth_json_at(bytes: &[u8], now_ms: i64) -> Result<QoderCred, AdapterError> {
    let v: Value = serde_json::from_slice(bytes)
        .map_err(|_| AdapterError::SchemaChanged("Qoder 凭据不是 JSON".into()))?;
    let token = v
        .get("token")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if token.is_empty() {
        return Err(AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into()));
    }
    // expiresAt 缺失时按「不过期」处理：Qoder 的 token 不透明，只能信文件里的字段
    let expires_at_ms = parse_time_ms(v.get("expiresAt")).unwrap_or(i64::MAX);
    if expires_at_ms <= now_ms {
        return Err(AdapterError::AuthExpiredMsg(REFRESH_HINT.into()));
    }
    Ok(QoderCred { token, expires_at_ms, label: label_of(&v) })
}

/// 本机 auth JSON → 界面账号标识：优先 user.name（实测可能是邮箱），退化到 user.id
fn label_of(v: &Value) -> String {
    let user = v.get("user");
    let name = user
        .and_then(|u| u.get("name"))
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .trim();
    if !name.is_empty() {
        return name.to_string();
    }
    user.and_then(|u| u.get("id")).and_then(|x| x.as_str()).unwrap_or("").trim().to_string()
}

/// DPAPI（当前用户）解包 Local State 里的 os_crypt 数据密钥
#[cfg(windows)]
fn dpapi_unwrap(blob: &[u8]) -> Result<Vec<u8>, AdapterError> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    let input =
        CRYPT_INTEGER_BLOB { cbData: blob.len() as u32, pbData: blob.as_ptr() as *mut u8 };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let unwrapped = unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            Some(std::ptr::null()),
            None,
            0,
            &mut output,
        )
    };
    if unwrapped.is_err() {
        return Err(AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into()));
    }
    let data =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut c_void)));
    }
    Ok(data)
}

#[cfg(not(windows))]
fn dpapi_unwrap(_blob: &[u8]) -> Result<Vec<u8>, AdapterError> {
    Err(AdapterError::SchemaChanged("仅 Windows 支持读取 Qoder 本机登录态".into()))
}

fn auth_key(dir: &Path) -> Result<Vec<u8>, AdapterError> {
    let text = std::fs::read_to_string(dir.join(LOCAL_STATE_FILE))
        .map_err(|_| AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into()))?;
    let json: Value = serde_json::from_str(&text)
        .map_err(|_| AdapterError::SchemaChanged("Qoder Local State 不是 JSON".into()))?;
    let encoded = json
        .get("os_crypt")
        .and_then(|o| o.get("encrypted_key"))
        .and_then(|x| x.as_str())
        .ok_or_else(|| AdapterError::SchemaChanged("Qoder Local State 缺少 os_crypt 密钥".into()))?;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| AdapterError::SchemaChanged("Qoder 数据密钥不是 base64".into()))?;
    if blob.get(..DPAPI_PREFIX.len()) != Some(DPAPI_PREFIX) {
        return Err(AdapterError::SchemaChanged("Qoder 数据密钥头不受支持".into()));
    }
    dpapi_unwrap(&blob[DPAPI_PREFIX.len()..])
}

/// 读本机 Qoder 登录态（严格只读，绝不回写产品目录）
pub fn read_local_cred(dir: &Path, now_ms: i64) -> Result<QoderCred, AdapterError> {
    let key = auth_key(dir)?;
    let blob = std::fs::read(dir.join(AUTH_FILE))
        .map_err(|_| AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into()))?;
    parse_auth_json_at(&decrypt_os_crypt(&key, &blob)?, now_ms)
}

/// 取当日福利活动：优先窗口内的一条，否则退化为窗口结束最晚的一条（便于报告「已领取」）
pub fn pick_campaign(campaigns: &[Value], now_s: i64) -> Option<Value> {
    let benefits: Vec<Value> = campaigns
        .iter()
        .filter(|c| c.get("actionType").and_then(|x| x.as_str()) == Some("CLAIM_BENEFIT"))
        .cloned()
        .collect();
    let in_window = benefits.iter().find(|c| {
        c.get("startAt").and_then(|x| x.as_i64()).unwrap_or(i64::MAX) <= now_s
            && now_s < c.get("endAt").and_then(|x| x.as_i64()).unwrap_or(i64::MIN)
    });
    in_window.cloned().or_else(|| {
        benefits
            .into_iter()
            .max_by_key(|c| c.get("endAt").and_then(|x| x.as_i64()).unwrap_or(i64::MIN))
    })
}

fn claim_status(item: &Value) -> &str {
    item.get("claimStatus").and_then(|x| x.as_str()).unwrap_or("")
}

fn in_window(item: &Value, now_s: i64) -> bool {
    item.get("startAt").and_then(|x| x.as_i64()).unwrap_or(i64::MAX) <= now_s
        && now_s < item.get("endAt").and_then(|x| x.as_i64()).unwrap_or(i64::MIN)
}

/// 今日窗口判定用本地日期，与 scheduler::day_start 的「今天」同一口径
fn local_date(ts_s: i64) -> Option<chrono::NaiveDate> {
    chrono::DateTime::from_timestamp(ts_s, 0)
        .map(|dt| dt.with_timezone(&chrono::Local).date_naive())
}

/// 滚动窗口的下一轮开启时刻（窗口是 [startAt, endAt)，所以下一轮从 endAt 的下一秒开始）。
/// 它落在本地今天 → 今天还会再开一轮，现在看到的已领属于上一轮。
/// 活动没有 endAt 或下一轮不在今天（一次性活动、周报式活动）→  false，按已领处理，别全天追问。
fn next_round_opens_today(item: &Value, now_s: i64) -> bool {
    let Some(end) = item.get("endAt").and_then(|x| x.as_i64()) else { return false };
    local_date(now_s).is_some_and(|today| local_date(end + 1) == Some(today))
}

pub fn status_of(item: Option<&Value>, now_s: i64) -> SignStatus {
    let Some(item) = item else { return SignStatus::Unknown };
    match claim_status(item) {
        "CLAIMED" if next_round_opens_today(item, now_s) => SignStatus::WindowPending,
        "CLAIMED" => SignStatus::SignedToday,
        "CLAIMABLE" if in_window(item, now_s) => SignStatus::NotSigned,
        _ => SignStatus::Unknown,
    }
}

/// claim 应答 → 结果。幂等标记是 `replayed`（实测：重复 POST 返回 200 + replayed:true）
pub fn outcome_of(body: &Value) -> SignOutcome {
    // 注意：claim 响应里状态字段叫 `status`，campaigns 里叫 `claimStatus`
    if body.get("status").and_then(|x| x.as_str()) != Some("CLAIMED") {
        return SignOutcome::Failed("未确认领取成功".into());
    }
    if body.get("replayed").and_then(|x| x.as_bool()) == Some(true) {
        return SignOutcome::AlreadySigned;
    }
    let amount = body
        .get("benefit")
        .and_then(|b| b.get("amount"))
        .and_then(|x| x.as_f64())
        .unwrap_or(0.0);
    SignOutcome::Success(format!("+{amount:.0} 积分"))
}

/// 北京时间今日 [0点, 明日0点)（固定 +08:00，系统时区不可信）
fn beijing_today(now_ms: i64) -> (i64, i64) {
    let cst = chrono::FixedOffset::east_opt(8 * 3_600).unwrap();
    let start = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms)
        .map(|d| d.with_timezone(&cst).date_naive())
        .unwrap_or_default()
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(cst).single())
        .map(|d| d.timestamp_millis())
        .unwrap_or(now_ms);
    (start, start + 86_400_000)
}

/// 余额 = 套餐剩余 + 加购剩余；当日消耗无 API 字段（同 trae 决策，返回 0）
pub fn credits_of(body: &Value, now_ms: i64) -> Result<CreditsSnapshot, AdapterError> {
    let usage = body
        .get("qoderUsage")
        .ok_or_else(|| AdapterError::SchemaChanged("usage 响应缺少 qoderUsage".into()))?;
    let remaining = |key: &str| -> f64 {
        usage
            .get(key)
            .and_then(|q| q.get("remaining"))
            .and_then(|x| x.as_f64())
            .unwrap_or(0.0)
    };
    let balance = round2(remaining("userQuota") + remaining("addOnQuota"));
    let (today_start, tomorrow_start) = beijing_today(now_ms);
    let day_after_start = tomorrow_start + 86_400_000;
    let exp = usage.get("expiresAt").and_then(|x| x.as_i64());
    let expiring_today = matches!(exp, Some(e) if (today_start..tomorrow_start).contains(&e))
        .then(|| balance).unwrap_or(0.0);
    let expiring_tomorrow = matches!(exp, Some(e) if (tomorrow_start..day_after_start).contains(&e))
        .then(|| balance).unwrap_or(0.0);
    Ok(CreditsSnapshot { balance, today_used: 0.0, expiring_today, expiring_tomorrow, fetched_at_ms: now_ms })
}

fn round2(v: f64) -> f64 {
    (v * 100.0).round() / 100.0
}

pub struct QoderAdapter {
    client: reqwest::Client,
    base_url: String,
    data_dir: Option<PathBuf>,
}

impl Default for QoderAdapter {
    fn default() -> Self {
        Self { client: super::http_client(), base_url: PROD_BASE.into(), data_dir: qoder_data_dir() }
    }
}

impl QoderAdapter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }
    pub fn with_data_dir(mut self, dir: PathBuf) -> Self {
        self.data_dir = Some(dir);
        self
    }

    /// 凭证只有本机登录态一条来源（DPAPI 读 auth.v1.dat）：不透明 token 一旦手工传进来就
    /// 没有任何过期信号能纠正它，会永久压住产品自己刷新的新凭据。
    fn resolve(&self) -> Result<QoderCred, AdapterError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        match self.data_dir.as_deref() {
            Some(dir) => read_local_cred(dir, now_ms),
            None => Err(AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into())),
        }
    }

    async fn request(
        &self,
        method: reqwest::Method,
        api_path: &str,
        cred: &QoderCred,
        body: Option<Value>,
    ) -> Result<Value, AdapterError> {
        let url = format!("{}{}", self.base_url, api_path);
        let mut req = self
            .client
            .request(method, url)
            .json(&body.unwrap_or_else(|| serde_json::json!({})));
        // Cosy-ClientType 缺失时接口不报错、只返回空活动列表（实测），必须常带
        req = req
            .header("authorization", format!("Bearer {}", cred.token))
            .header("cosy-clienttype", "10")
            .header("user-agent", "Qoder")
            .header("accept", "application/json")
            .timeout(std::time::Duration::from_secs(30));
        let resp = req.send().await?;
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

    async fn campaigns(&self, cred: &QoderCred) -> Result<Vec<Value>, AdapterError> {
        let v = self.request(reqwest::Method::GET, CAMPAIGNS_PATH, cred, None).await?;
        let list = v
            .get("campaigns")
            .and_then(|x| x.as_array())
            .ok_or_else(|| AdapterError::SchemaChanged("campaigns 响应缺少 campaigns 数组".into()))?;
        Ok(list.clone())
    }
}

#[async_trait::async_trait]
impl ProductAdapter for QoderAdapter {
    fn id(&self) -> String {
        "qoder".into()
    }

    async fn query_sign_status(&self) -> Result<SignStatus, AdapterError> {
        let cred = self.resolve()?;
        let now_s = chrono::Utc::now().timestamp();
        let item = pick_campaign(&self.campaigns(&cred).await?, now_s);
        Ok(status_of(item.as_ref(), now_s))
    }

    async fn sign_in(&self) -> Result<SignOutcome, AdapterError> {
        let cred = self.resolve()?;
        let now_s = chrono::Utc::now().timestamp();
        let Some(item) = pick_campaign(&self.campaigns(&cred).await?, now_s) else {
            return Ok(SignOutcome::Failed("当前无可领取的 Qoder 活动".into()));
        };
        match status_of(Some(&item), now_s) {
            SignStatus::SignedToday => return Ok(SignOutcome::AlreadySigned),
            SignStatus::NotSigned => {}
            // 退化分支挑出来的可能是「窗口外但还没领」或「已领的是上一轮」的一条：对它发 claim
            // 只会被服务端拒/重放，而且用户看不到原因。状态不明就别动手，把窗口时间讲清楚。
            SignStatus::Unknown | SignStatus::WindowPending => {
                return Ok(SignOutcome::Failed(NO_WINDOW_HINT.into()));
            }
        }
        let Some(cid) = item.get("campaignId").and_then(|x| x.as_str()) else {
            return Err(AdapterError::SchemaChanged("活动缺少 campaignId".into()));
        };
        let body = self
            .request(
                reqwest::Method::POST,
                &format!("{CAMPAIGNS_PATH}/{cid}/claim"),
                &cred,
                Some(serde_json::json!({})),
            )
            .await?;
        Ok(outcome_of(&body))
    }

    async fn fetch_credits(&self) -> Result<CreditsSnapshot, AdapterError> {
        let cred = self.resolve()?;
        let v = self.request(reqwest::Method::GET, USAGE_PATH, &cred, None).await?;
        credits_of(&v, chrono::Utc::now().timestamp_millis())
    }

    /// 账号标识只取自本机登录态文件（token 本身不透明、无从反解身份）
    async fn account_label(&self) -> Result<String, AdapterError> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let dir = self
            .data_dir
            .as_deref()
            .ok_or_else(|| AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into()))?;
        let cred = read_local_cred(dir, now_ms)?;
        if cred.label.is_empty() {
            return Err(AdapterError::SchemaChanged("Qoder 登录信息里没有账号标识".into()));
        }
        Ok(cred.label)
    }
}

#[cfg(test)]
mod tests {

    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    use serde_json::{json, Value};
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        credits_of, decrypt_os_crypt, outcome_of, parse_auth_json_at, pick_campaign,
        read_local_cred, status_of, AUTH_FILE, DPAPI_PREFIX, LOCAL_STATE_FILE, NONCE_LEN,
        OS_CRYPT_TAG, QoderAdapter,
    };
    use crate::adapter::{AdapterError, ProductAdapter, SignOutcome, SignStatus};
    use base64::Engine as _;
    use chrono::TimeZone;

    fn campaign(id: &str, action: &str, claim: &str, start: i64, end: i64, amount: f64) -> Value {
        let mut c = json!({
            "campaignId": id,
            "campaignKey": format!("act-{id}"),
            "actionType": action,
            "claimStatus": claim,
            "startAt": start,
            "endAt": end,
        });
        if amount > 0.0 {
            c["benefit"] = json!({"kind": "CREDITS", "amount": amount});
        }
        c
    }

    #[test]
    fn pick_campaign_prefers_claim_benefit_over_static_promo() {
        let daily = campaign("d1", "CLAIM_BENEFIT", "CLAIMABLE", 100, 200, 100.0);
        let promo = campaign("p1", "VIEW_DETAILS", "CLAIMED", 0, 10_000, 0.0);
        assert_eq!(pick_campaign(&[promo, daily.clone()], 150).unwrap()["campaignId"], "d1");
    }

    #[test]
    fn pick_campaign_falls_back_to_latest_window_when_none_active() {
        let yesterday = campaign("d1", "CLAIM_BENEFIT", "CLAIMED", 0, 100, 100.0);
        let older = campaign("d2", "CLAIM_BENEFIT", "CLAIMED", 0, 50, 100.0);
        assert_eq!(pick_campaign(&[older, yesterday], 500).unwrap()["campaignId"], "d1");
    }

    #[test]
    fn pick_campaign_returns_none_when_no_benefit_campaign_exists() {
        let promo = campaign("p1", "VIEW_DETAILS", "CLAIMED", 0, 10_000, 0.0);
        assert!(pick_campaign(&[promo], 150).is_none());
    }

    /// 本地时刻 → Unix 秒。测试全部用本地时间构造，判定也在本地时间做，换时区不会假失败
    fn ts(y: i32, mo: u32, d: u32, h: u32, mi: u32) -> i64 {
        chrono::Local.with_ymd_and_hms(y, mo, d, h, mi, 0).unwrap().timestamp()
    }

    #[test]
    fn status_maps_claim_state_and_window() {
        let day = 86_400;
        let claimable = campaign("d", "CLAIM_BENEFIT", "CLAIMABLE", 1_000, 1_000 + day, 100.0);
        let claimed = campaign("d", "CLAIM_BENEFIT", "CLAIMED", 1_000, 1_000 + day, 100.0);
        let future = campaign("d", "CLAIM_BENEFIT", "CLAIMABLE", 5_000, 9_000, 100.0);
        assert_eq!(status_of(Some(&claimable), 1_500), SignStatus::NotSigned);
        assert_eq!(status_of(Some(&claimed), 1_500), SignStatus::SignedToday);
        assert_eq!(status_of(Some(&future), 1_500), SignStatus::Unknown);
        assert_eq!(status_of(None, 1_500), SignStatus::Unknown);
        // 窗口早就结束（明天再看也不会有一天新的）→ 仍按已领处理
        let over = campaign("d", "CLAIM_BENEFIT", "CLAIMED", 1_000, 5_000, 100.0);
        assert_eq!(status_of(Some(&over), 1_500 + day), SignStatus::SignedToday);
    }

    /// 回归：Qoder 的领取窗口按北京时间 10:00 滚动（昨日 10:00 → 今日 09:59，实测见
    /// docs/recon/qoder.md）。10:00 前看到的「已领取」是上一轮的战果，今天的福利还没到手 ——
    /// 记成 SignedToday 会落一条 alreadySigned，把当天 10:05 的调度整天堵死。
    #[test]
    fn claimed_previous_window_means_todays_window_not_open_yet() {
        let last = campaign("d", "CLAIM_BENEFIT", "CLAIMED",
            ts(2026, 9, 23, 10, 0), ts(2026, 9, 24, 9, 59), 100.0);
        assert_eq!(status_of(Some(&last), ts(2026, 9, 24, 9, 0)), SignStatus::WindowPending);
        // 0:30 同理：命中的还是昨天那一轮
        assert_eq!(status_of(Some(&last), ts(2026, 9, 24, 0, 30)), SignStatus::WindowPending);
        // 10:00 之后列出来的是今天这一轮，领掉它才算今天处理完
        let today = campaign("d", "CLAIM_BENEFIT", "CLAIMED",
            ts(2026, 9, 24, 10, 0), ts(2026, 9, 25, 9, 59), 100.0);
        assert_eq!(status_of(Some(&today), ts(2026, 9, 24, 10, 30)), SignStatus::SignedToday);
    }

    /// 一次性活动结束多日：今天不会再开新一轮，别全天反复追问接口
    #[test]
    fn claimed_long_ago_campaign_stays_signed_today() {
        let stale = campaign("d", "CLAIM_BENEFIT", "CLAIMED",
            ts(2026, 9, 20, 10, 0), ts(2026, 9, 21, 9, 59), 100.0);
        assert_eq!(status_of(Some(&stale), ts(2026, 9, 24, 9, 0)), SignStatus::SignedToday);
    }

    #[test]
    fn outcome_reports_amount_and_replay() {
        assert_eq!(
            outcome_of(&json!({"status": "CLAIMED", "replayed": false, "benefit": {"amount": 100}})),
            SignOutcome::Success("+100 积分".into())
        );
        assert_eq!(
            outcome_of(&json!({"status": "CLAIMED", "replayed": true})),
            SignOutcome::AlreadySigned
        );
        assert_eq!(outcome_of(&json!({"status": "GRANT_FAILED"})), SignOutcome::Failed("未确认领取成功".into()));
    }

    fn usage_body(user_left: f64, addon_left: f64) -> Value {
        json!({
            "displayMode": "qoder",
            "qoderUsage": {
                "usageType": "credits",
                "expiresAt": 253_402_214_400_000i64,
                "userQuota": {"total": 100.0, "used": 100.0 - user_left, "remaining": user_left},
                "addOnQuota": {"total": 400.0, "used": 0.0, "remaining": addon_left},
            }
        })
    }

    #[test]
    fn credits_sum_plan_and_addon_remaining() {
        let snap = credits_of(&usage_body(10.0, 400.0), 1_700_000_000_000).unwrap();
        assert_eq!(snap.balance, 410.0);
        assert_eq!(snap.today_used, 0.0);
        assert_eq!(snap.expiring_today, 0.0);
        assert_eq!(snap.fetched_at_ms, 1_700_000_000_000);
    }

    #[test]
    fn credits_flag_balance_expiring_today() {
        let cst = chrono::FixedOffset::east_opt(8 * 3_600).unwrap();
        let today_noon = chrono::Utc::now()
            .with_timezone(&cst)
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(cst)
            .unwrap()
            .timestamp_millis();
        let mut body = usage_body(0.0, 30.0);
        body["qoderUsage"]["expiresAt"] = json!(today_noon);
        let snap = credits_of(&body, today_noon).unwrap();
        assert_eq!(snap.balance, 30.0);
        assert_eq!(snap.expiring_today, 30.0);
    }

    #[test]
    fn credits_flag_balance_expiring_tomorrow() {
        let cst = chrono::FixedOffset::east_opt(8 * 3_600).unwrap();
        let tomorrow_noon = chrono::Utc::now()
            .with_timezone(&cst)
            .date_naive()
            .and_hms_opt(12, 0, 0)
            .unwrap()
            .and_local_timezone(cst)
            .unwrap()
            .timestamp_millis()
            + 86_400_000;
        let now_ms = tomorrow_noon - 86_400_000;
        let mut body = usage_body(0.0, 30.0);
        body["qoderUsage"]["expiresAt"] = json!(tomorrow_noon);
        let snap = credits_of(&body, now_ms).unwrap();
        assert_eq!(snap.expiring_today, 0.0);
        assert_eq!(snap.expiring_tomorrow, 30.0);
    }

    #[test]
    fn parse_auth_json_exposes_account_label() {
        let cred = parse_auth_json_at(
            br#"{"token":"TK","user":{"name":"tester@qoder.cn","id":"01ab23456789"}}"#,
            1_700_000_000_000,
        ).unwrap();
        assert_eq!(cred.label, "tester@qoder.cn");
        let cred = parse_auth_json_at(
            br#"{"token":"TK","user":{"id":"01ab23456789"}}"#,
            1_700_000_000_000,
        ).unwrap();
        assert_eq!(cred.label, "01ab23456789");
        let cred = parse_auth_json_at(br#"{"token":"TK"}"#, 1_700_000_000_000).unwrap();
        assert_eq!(cred.label, "");
    }

    #[test]
    fn credits_rejects_payload_without_quota() {
        let err = credits_of(&json!({"displayMode": "qoder"}), 1_700_000_000_000).unwrap_err();
        assert!(matches!(err, AdapterError::SchemaChanged(_)), "缺 qoderUsage 应报接口变更: {err:?}");
    }

    fn gcm_encrypt(key: &[u8; 32], nonce: &[u8; 12], plain: &[u8]) -> Vec<u8> {
        Aes256Gcm::new_from_slice(key).unwrap().encrypt(Nonce::from_slice(nonce), plain).unwrap()
    }

    #[test]
    fn decrypt_os_crypt_reverses_v10_framing() {
        let key = [7u8; 32];
        let nonce = [3u8; 12];
        let mut blob = b"v10".to_vec();
        blob.extend_from_slice(&nonce);
        blob.extend(gcm_encrypt(&key, &nonce, b"{\"token\":\"t\"}"));
        assert_eq!(decrypt_os_crypt(&key, &blob).unwrap(), b"{\"token\":\"t\"}".to_vec());
    }

    #[test]
    fn decrypt_os_crypt_rejects_foreign_framing() {
        let key = [7u8; 32];
        assert!(decrypt_os_crypt(&key, b"v11abcdefghijklmnop").is_err());
        assert!(decrypt_os_crypt(&key, b"v10short").is_err());
        let mut tampered = b"v10".to_vec();
        let nonce = [9u8; 12];
        tampered.extend_from_slice(&nonce);
        let mut ct = gcm_encrypt(&key, &nonce, b"payload");
        let last = ct.len() - 1;
        ct[last] ^= 0xFF;
        tampered.extend(&ct);
        assert!(decrypt_os_crypt(&key, &tampered).is_err());
    }

    #[test]
    fn parse_auth_json_extracts_token_and_expiry() {
        let bytes = json!({
            "token": " opaque-token ",
            "expiresAt": "2026-10-22T01:53:36Z",
            "refreshTokenExpiresAt": "2027-09-17T01:53:36Z",
        })
        .to_string()
        .into_bytes();
        let cred = parse_auth_json_at(&bytes, 1_700_000_000_000).unwrap();
        assert_eq!(cred.token, "opaque-token");
        assert_eq!(cred.expires_at_ms, 1_792_634_016_000);
    }

    #[test]
    fn parse_auth_json_rejects_missing_or_empty_token() {
        let later = 1_700_000_000_000;
        assert!(parse_auth_json_at(br#"{"expiresAt":"2099-01-01T00:00:00Z"}"#, later).is_err());
        assert!(
            parse_auth_json_at(br#"{"token":"","expiresAt":"2099-01-01T00:00:00Z"}"#, later).is_err()
        );
        assert!(parse_auth_json_at(b"not json", later).is_err());
    }

    #[test]
    fn read_local_cred_without_files_points_at_qoder_login() {
        let dir = tempfile::tempdir().unwrap();
        let err = read_local_cred(dir.path(), chrono::Utc::now().timestamp_millis())
            .expect_err("空目录不应解出凭证");
        assert!(err.to_string().contains("Qoder"), "文案应指向 Qoder: {err}");
    }

    #[test]
    fn expired_local_cred_asks_to_reopen_qoder() {
        let bytes = json!({"token": "t", "expiresAt": "2020-01-01T00:00:00Z"}).to_string().into_bytes();
        let err = parse_auth_json_at(&bytes, 1_700_000_000_000).expect_err("过期凭证应报错");
        assert!(err.to_string().contains("Qoder"), "文案应指向 Qoder: {err}");
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    /// 「本机装过并登录了 Qoder」的测试夹具：Local State 放 DPAPI 包好的数据密钥，
    /// auth.v1.dat 放 v10 密文。DPAPI 是当前用户的密钥，只有 Windows 造得出来，
    /// 所以依赖它的用例都挂在 #[cfg(windows)] 上。
    #[cfg(windows)]
    struct LocalLogin(tempfile::TempDir);

    #[cfg(windows)]
    impl LocalLogin {
        fn with_token(token: &str) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let key = [7u8; 32];
            let encoded = base64::engine::general_purpose::STANDARD
                .encode([DPAPI_PREFIX, dpapi_protect(&key).as_slice()].concat());
            std::fs::write(dir.path().join(LOCAL_STATE_FILE),
                json!({"os_crypt": {"encrypted_key": encoded}}).to_string()).unwrap();
            let nonce = [3u8; NONCE_LEN];
            let plain = json!({"token": token, "expiresAt": "2099-01-01T00:00:00Z",
                              "user": {"name": "tester"}}).to_string();
            let mut blob = OS_CRYPT_TAG.to_vec();
            blob.extend_from_slice(&nonce);
            blob.extend(gcm_encrypt(&key, &nonce, plain.as_bytes()));
            std::fs::write(dir.path().join(AUTH_FILE), blob).unwrap();
            Self(dir)
        }
        fn adapter(&self, s: &MockServer) -> QoderAdapter {
            QoderAdapter::new().with_base_url(s.uri())
                .with_data_dir(self.0.path().to_path_buf())
        }
    }

    #[cfg(windows)]
    fn dpapi_protect(data: &[u8]) -> Vec<u8> {
        use std::ffi::c_void;
        use windows::Win32::Foundation::{HLOCAL, LocalFree};
        use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
        let input =
            CRYPT_INTEGER_BLOB { cbData: data.len() as u32, pbData: data.as_ptr() as *mut u8 };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(&input, None, None, Some(std::ptr::null()), None, 0, &mut out)
                .expect("DPAPI 加密失败");
        }
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe { let _ = LocalFree(Some(HLOCAL(out.pbData as *mut c_void))); }
        v
    }

    async fn mount_campaigns(s: &MockServer, campaigns: Vec<Value>) {
        Mock::given(method("GET"))
            .and(path("/sash/api/v1/me/campaigns"))
            .and(header("cosy-clienttype", "10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "uid": "u1", "showCampaign": true,
                "claimable": campaigns.iter().any(|c| c["claimStatus"] == "CLAIMABLE"),
                "campaignUrl": "https://openapi.qoder.com.cn/growth-page/activity-iframe",
                "campaigns": campaigns,
            })))
            .mount(s)
            .await;
    }

    /// 手动 token 走鉴权头，wiremock 断言 header 是否真的带上
    async fn mount_campaigns_for(s: &MockServer, campaigns: Vec<Value>, bearer: &str) {
        Mock::given(method("GET"))
            .and(path("/sash/api/v1/me/campaigns"))
            .and(header("authorization", format!("Bearer {bearer}")))
            .and(header("cosy-clienttype", "10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "showCampaign": true, "campaigns": campaigns,
            })))
            .mount(s)
            .await;
    }

    #[cfg(windows)]
    #[test]
    fn query_status_sends_local_token_and_cosy_header() {
        let s = rt().block_on(MockServer::start());
        let now = chrono::Utc::now().timestamp();
        rt().block_on(mount_campaigns_for(
            &s,
            vec![campaign("d1", "CLAIM_BENEFIT", "CLAIMABLE", now - 60, now + 86_400, 100.0)],
            "opaque-token",
        ));
        let login = LocalLogin::with_token("opaque-token");
        let status = rt().block_on(login.adapter(&s).query_sign_status()).unwrap();
        assert_eq!(status, SignStatus::NotSigned);
    }

    #[cfg(windows)]
    #[test]
    fn query_status_with_no_campaigns_is_unknown() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_campaigns(&s, vec![]));
        let login = LocalLogin::with_token("opaque-token");
        let status = rt().block_on(login.adapter(&s).query_sign_status()).unwrap();
        assert_eq!(status, SignStatus::Unknown);
    }

    async fn mount_status(s: &MockServer, api_path: &str, status: u16) {
        Mock::given(method("GET"))
            .and(path(api_path))
            .respond_with(ResponseTemplate::new(status))
            .mount(s)
            .await;
    }

    #[cfg(windows)]
    #[test]
    fn query_status_on_401_is_auth_expired() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_status(&s, "/sash/api/v1/me/campaigns", 401));
        let login = LocalLogin::with_token("opaque-token");
        let err = rt().block_on(login.adapter(&s).query_sign_status()).unwrap_err();
        assert!(matches!(err, AdapterError::AuthExpiredMsg(_)), "401 应提示登录失效: {err:?}");
    }

    #[cfg(windows)]
    #[test]
    fn gateway_503_with_json_body_is_retryable_not_a_success() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(
            Mock::given(method("GET"))
                .and(path("/sash/api/v1/me/campaigns"))
                .respond_with(ResponseTemplate::new(503).set_body_json(json!({"campaigns": []})))
                .mount(&s),
        );
        let login = LocalLogin::with_token("opaque-token");
        let err = rt().block_on(login.adapter(&s).query_sign_status()).unwrap_err();
        assert!(matches!(err, AdapterError::Http(_)), "5xx 是可重试的网络错误: {err:?}");
    }

    async fn mount_claim(s: &MockServer, cid: &str, body: Value) {
        Mock::given(method("POST"))
            .and(path(format!("/sash/api/v1/me/campaigns/{cid}/claim")))
            .and(header("cosy-clienttype", "10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(s)
            .await;
    }

    #[cfg(windows)]
    #[test]
    fn sign_in_posts_claim_on_the_active_campaign() {
        let s = rt().block_on(MockServer::start());
        let now = chrono::Utc::now().timestamp();
        let cid = "01a0cd40-ea93-75c3-be36-23c22a84e619";
        rt().block_on(mount_campaigns(
            &s,
            vec![
                campaign("promo", "VIEW_DETAILS", "CLAIMED", now - 86_400, now + 86_400, 0.0),
                campaign(cid, "CLAIM_BENEFIT", "CLAIMABLE", now - 60, now + 86_400, 100.0),
            ],
        ));
        rt().block_on(mount_claim(
            &s,
            cid,
            json!({
                "grantId": "g1", "status": "CLAIMED", "replayed": false,
                "benefit": {"kind": "CREDITS", "amount": 100},
                "campaignId": cid, "campaignKey": "act-20260923-159",
            }),
        ));
        let login = LocalLogin::with_token("opaque-token");
        let outcome = rt().block_on(login.adapter(&s).sign_in()).unwrap();
        assert_eq!(outcome, SignOutcome::Success("+100 积分".into()));
    }

    #[cfg(windows)]
    #[test]
    fn sign_in_refuses_to_claim_outside_the_window() {
        let s = rt().block_on(MockServer::start());
        let now = chrono::Utc::now().timestamp();
        // 昨天那条窗口已关且没领：退化分支会把它挑出来，对它发 claim 只会被服务端拒
        rt().block_on(mount_campaigns(
            &s,
            vec![campaign("d1", "CLAIM_BENEFIT", "CLAIMABLE", now - 2 * 86_400, now - 86_400, 100.0)],
        ));
        let login = LocalLogin::with_token("opaque-token");
        let outcome = rt().block_on(login.adapter(&s).sign_in()).unwrap();
        let text = match &outcome {
            SignOutcome::Failed(m) => m.clone(),
            other => panic!("窗口外不该给出成功/已签结果: {other:?}"),
        };
        assert!(text.contains("10:00"), "文案要告诉用户窗口何时开: {text}");
        let posts = rt().block_on(s.received_requests()).unwrap()
            .iter().filter(|r| r.method.as_str() == "POST").count();
        assert_eq!(posts, 0, "窗口外绝不能发 claim");
    }

    #[cfg(windows)]
    #[test]
    fn sign_in_returns_already_signed_without_posting_claim() {
        let s = rt().block_on(MockServer::start());
        let now = chrono::Utc::now().timestamp();
        rt().block_on(mount_campaigns(
            &s,
            vec![campaign("d1", "CLAIM_BENEFIT", "CLAIMED", now - 60, now + 86_400, 100.0)],
        ));
        let login = LocalLogin::with_token("opaque-token");
        let outcome = rt().block_on(login.adapter(&s).sign_in()).unwrap();
        assert_eq!(outcome, SignOutcome::AlreadySigned);
        let posts = rt()
            .block_on(s.received_requests())
            .unwrap()
            .into_iter()
            .filter(|r| r.method.as_str() == "POST")
            .count();
        assert_eq!(posts, 0, "已领取窗口不应重放 claim 请求");
    }

    async fn mount_usage(s: &MockServer, body: Value) {
        Mock::given(method("GET"))
            .and(path("/sash/api/v2/me/usage"))
            .and(header("cosy-clienttype", "10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(s)
            .await;
    }

    #[cfg(windows)]
    #[test]
    fn fetch_credits_reads_usage_endpoint() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_usage(&s, usage_body(20.0, 380.0)));
        let login = LocalLogin::with_token("opaque-token");
        let snap = rt().block_on(login.adapter(&s).fetch_credits()).unwrap();
        assert_eq!(snap.balance, 400.0);
    }

    #[test]
    fn account_label_reports_local_login_absence_instead_of_guessing() {
        let dir = tempfile::tempdir().unwrap();
        let adapter = QoderAdapter::new().with_data_dir(dir.path().to_path_buf());
        let err = rt().block_on(adapter.account_label()).expect_err("空目录没有账号信息");
        assert!(err.to_string().contains("Qoder"), "文案应指向 Qoder: {err}");
    }

    /// 没有本机登录态时，报错要发生在发请求之前（文案指向 Qoder 而不是接口异常）
    #[test]
    fn missing_local_login_is_reported_before_any_request() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_campaigns(&s, vec![]));
        let dir = tempfile::tempdir().unwrap();
        let adapter = QoderAdapter::new()
            .with_base_url(s.uri())
            .with_data_dir(dir.path().to_path_buf());
        let err = rt().block_on(adapter.query_sign_status()).unwrap_err();
        assert!(err.to_string().contains("Qoder"), "文案应指向 Qoder: {err}");
    }

    #[test]
    #[ignore = "只读诊断：读本机 Qoder 登录态，只打印长度与到期时间"]
    fn diag_local_qoder_state() {
        let dir = super::qoder_data_dir().expect("本机无 Qoder 数据目录");
        let cred = read_local_cred(&dir, chrono::Utc::now().timestamp_millis()).unwrap();
        println!(
            "token_len={} 到期={}",
            cred.token.len(),
            chrono::DateTime::<chrono::Utc>::from_timestamp_millis(cred.expires_at_ms)
                .map(|d| d.to_rfc3339())
                .unwrap_or_default()
        );
    }

    #[test]
    #[ignore = "只读诊断：真实接口查状态与积分，不签到"]
    fn diag_live_status_and_credits() {
        let rt = rt();
        let dir = super::qoder_data_dir().unwrap();
        let cred = read_local_cred(&dir, chrono::Utc::now().timestamp_millis()).unwrap();
        let adapter = QoderAdapter::new();
        let status = rt.block_on(adapter.query_sign_status()).unwrap();
        let snap = rt.block_on(adapter.fetch_credits()).unwrap();
        println!(
            "本地登录态：label={} token 长度={} 过期={}；status={status:?} balance={} expiring_today={}",
            cred.label, cred.token.len(), cred.expires_at_ms, snap.balance, snap.expiring_today
        );
    }

    #[test]
    #[ignore = "诊断：走生产路径（空 token → 本机登录态）。今日窗口已领取，故只会读到状态、不发 claim"]
    fn diag_live_sign_in_with_local_cred() {
        let rt = rt();
        let adapter = QoderAdapter::new();
        let outcome = rt.block_on(adapter.sign_in()).unwrap();
        println!("sign_in(空 token) -> {outcome:?}");
        assert_eq!(outcome, SignOutcome::AlreadySigned, "本机今日已领取，应为 AlreadySigned 且不发 claim 请求");
    }
}
