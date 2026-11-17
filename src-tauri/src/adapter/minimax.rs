//! MiniMax Code（agent.minimax.cn）每日签到。
//!
//! 机制：积分制。7 天一轮（每天 400/400/400/1000/400/400/1000 积分，共 4000），
//! 每轮领满后 30 天内有效。每日一次 `POST /signin/claim`。
//!
//! 2026-11-17 逆向确认（前端 chunk `page-bae2d2ec5bc8c546.js` + vendor `10489-*.js`）：
//!   GET  /minimax-cloud/api/v1/signin/status?timezone_id=Asia/Shanghai
//!         -> {data:{scene, days:[{day_no, points, status, is_today, bonus_points}]}, base_resp:{status_code,status_msg}}
//!         days[i].status: 1=Upcoming 2=Claimable 3=Claimed 4=Disabled
//!   POST /minimax-cloud/api/v1/signin/claim  body {}
//!         -> {data:{claim_result, day_no, points, claim_id, expire_at_ms}, base_resp:{...}}
//!         claim_result: 1=今天首次领取 2=今日已领取（幂等重放）
//!
//! 认证（gateway 强校验，不是标准 Bearer）：
//!   header `token` = JWT（前端存 localStorage `user_detail_agent`.token）
//!   header `x-timestamp` = unix 秒
//!   header `x-signature` = md5( timestamp + "I*7Cf%WZ#S&%1RlZJ&C2" + body )
//!                          body 为 POST 的 JSON 字符串（claim 为 "{}"），GET 为空串。
//!   query 参数：user_id、token、app_id=3001、biz_id=3、version_code=22201、unix、
//!               timezone_offset=28800、device_platform=web、client=web、timezone_id 等。
//!   用 `Authorization: Bearer` 会得到 401 invalid access token（服务器不认这个头）；
//!   用 `token` 头但签名错会得到 400 invalid signature。
//!
//! 凭据来源：用户已在 Chrome 登录 agent.minimax.cn —— 从 Chrome 的 Local Storage
//! leveldb 里扫出 `user_detail_agent`（JWT + realUserID），封存进 SignDock 自己的
//! DPAPI 文件；也提供 SignDock 自有的 WebView2 登录窗口作为兜底。绝不手工粘贴。

use std::path::{Path, PathBuf};

use reqwest::Method;
use serde_json::Value;

use super::{
    transport_error, AdapterError, CreditsSnapshot, ProductAdapter, SignOutcome, SignStatus,
};
use crate::adapter::http_client;

pub const MINIMAX_ORIGIN: &str = "https://agent.minimax.cn";

const SIGNIN_STATUS_PATH: &str = "/minimax-cloud/api/v1/signin/status";
const SIGNIN_CLAIM_PATH: &str = "/minimax-cloud/api/v1/signin/claim";

/// 网关签名盐（前端常量，写死在前端 bundle 里）。
const SIGNATURE_SALT: &str = "I*7Cf%WZ#S&%1RlZJ&C2";
const APP_ID: &str = "3001";
const BIZ_ID: &str = "3";
const VERSION_CODE: &str = "22201";
/// China UTC+8：`-60 * new Date().getTimezoneOffset()` = -60 * (-480) = 28800。
const TIMEZONE_OFFSET: &str = "28800";
const TIMEZONE_ID: &str = "Asia/Shanghai";

const NOT_LOGGED_IN_HINT: &str = "尚未导入 MiniMax 登录态；若已在 Chrome 登录 agent.minimax.cn，点「从 Chrome 导入登录」";
const SESSION_DEAD_HINT: &str = "MiniMax 会话已失效，请在 Chrome 里重新登录 agent.minimax.cn 后再导入一次";

fn app_data_dir() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(Path::new(&appdata).join("com.signdock.app"))
}

pub fn managed_cred_path() -> Option<PathBuf> {
    Some(app_data_dir()?.join("minimax-cred.json"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct MinimaxCred {
    pub token: String,
    pub user_id: String,
    pub user_name: String,
    pub saved_at_ms: i64,
}

pub fn read_cred(path: &Path) -> Result<MinimaxCred, AdapterError> {
    let v = crate::secret::read_json(path).map_err(|e| match e {
        crate::secret::SecretError::NotFound => {
            AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into())
        }
        _ => AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()),
    })?;
    let token = str_field(&v, "token");
    if token.is_empty() {
        return Err(AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()));
    }
    Ok(MinimaxCred {
        token,
        user_id: str_field(&v, "userId"),
        user_name: str_field(&v, "userName"),
        saved_at_ms: v.get("savedAtMs").and_then(Value::as_i64).unwrap_or(0),
    })
}

pub fn write_cred(path: &Path, cred: &MinimaxCred) -> Result<(), AdapterError> {
    let v = serde_json::json!({
        "token": cred.token,
        "userId": cred.user_id,
        "userName": cred.user_name,
        "savedAtMs": cred.saved_at_ms,
    });
    crate::secret::write_json(path, &v).map_err(|_| AdapterError::AuthExpired)
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

// ---------------------------------------------------------------------------
// MD5（内联实现，避免新增依赖）。仅用于构造网关签名，无其它用途。
// ---------------------------------------------------------------------------
mod md5 {
    const S: [u32; 64] = [
        7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 7, 12, 17, 22, 5, 9, 14, 20, 5, 9, 14, 20, 5,
        9, 14, 20, 5, 9, 14, 20, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 4, 11, 16, 23, 6, 10,
        15, 21, 6, 10, 15, 21, 6, 10, 15, 21, 6, 10, 15, 21,
    ];
    const K: [u32; 64] = [
        0xd76aa478, 0xe8c7b756, 0x242070db, 0xc1bdceee, 0xf57c0faf, 0x4787c62a, 0xa8304613,
        0xfd469501, 0x698098d8, 0x8b44f7af, 0xffff5bb1, 0x895cd7be, 0x6b901122, 0xfd987193,
        0xa679438e, 0x49b40821, 0xf61e2562, 0xc040b340, 0x265e5a51, 0xe9b6c7aa, 0xd62f105d,
        0x02441453, 0xd8a1e681, 0xe7d3fbc8, 0x21e1cde6, 0xc33707d6, 0xf4d50d87, 0x455a14ed,
        0xa9e3e905, 0xfcefa3f8, 0x676f02d9, 0x8d2a4c8a, 0xfffa3942, 0x8771f681, 0x6d9d6122,
        0xfde5380c, 0xa4beea44, 0x4bdecfa9, 0xf6bb4b60, 0xbebfbc70, 0x289b7ec6, 0xeaa127fa,
        0xd4ef3085, 0x04881d05, 0xd9d4d039, 0xe6db99e5, 0x1fa27cf8, 0xc4ac5665, 0xf4292244,
        0x432aff97, 0xab9423a7, 0xfc93a039, 0x655b59c3, 0x8f0ccc92, 0xffeff47d, 0x85845dd1,
        0x6fa87e4f, 0xfe2ce6e0, 0xa3014314, 0x4e0811a1, 0xf7537e82, 0xbd3af235, 0x2ad7d2bb,
        0xeb86d391,
    ];

    pub fn hex(input: &[u8]) -> String {
        let mut a0: u32 = 0x67452301;
        let mut b0: u32 = 0xefcdab89;
        let mut c0: u32 = 0x98badcfe;
        let mut d0: u32 = 0x10325476;

        let mut msg = input.to_vec();
        let bit_len = (input.len() as u64).wrapping_mul(8);
        msg.push(0x80);
        while msg.len() % 64 != 56 {
            msg.push(0);
        }
        msg.extend_from_slice(&bit_len.to_le_bytes());

        for chunk in msg.chunks(64) {
            let mut m = [0u32; 16];
            for (i, w) in m.iter_mut().enumerate() {
                *w = u32::from_le_bytes([
                    chunk[i * 4],
                    chunk[i * 4 + 1],
                    chunk[i * 4 + 2],
                    chunk[i * 4 + 3],
                ]);
            }
            let (mut a, mut b, mut c, mut d) = (a0, b0, c0, d0);
            for i in 0..64 {
                let (f, g) = match i / 16 {
                    0 => ((b & c) | ((!b) & d), i),
                    1 => ((d & b) | ((!d) & c), (5 * i + 1) % 16),
                    2 => (b ^ c ^ d, (3 * i + 5) % 16),
                    _ => (c ^ (b | (!d)), (7 * i) % 16),
                };
                let tmp = d;
                d = c;
                c = b;
                b = b.wrapping_add(
                    a.wrapping_add(f)
                        .wrapping_add(K[i])
                        .wrapping_add(m[g])
                        .rotate_left(S[i]),
                );
                a = tmp;
            }
            a0 = a0.wrapping_add(a);
            b0 = b0.wrapping_add(b);
            c0 = c0.wrapping_add(c);
            d0 = d0.wrapping_add(d);
        }

        let mut out = String::with_capacity(32);
        for w in [a0, b0, c0, d0] {
            for byte in w.to_le_bytes() {
                out.push_str(&format!("{:02x}", byte));
            }
        }
        out
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn known_vectors() {
            assert_eq!(super::hex(b""), "d41d8cd98f00b204e9800998ecf8427e");
            assert_eq!(super::hex(b"abc"), "900150983cd24fb0d6963f7d28e17f72");
            assert_eq!(
                super::hex(b"1234567890"),
                "e807f1fcf82d132f9bb018ca6738a19f"
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Chrome Local Storage leveldb 扫描（只读，不写任何 Chrome 文件）
// ---------------------------------------------------------------------------
fn chrome_leveldb_dir() -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    Some(
        Path::new(&local)
            .join("Google")
            .join("Chrome")
            .join("User Data")
            .join("Default")
            .join("Local Storage")
            .join("leveldb"),
    )
}

/// 从一段文本里，提取 `"<key>":"<value>"` 的 value（value 须为 ASCII，如 JWT/数字 id）。
fn quoted_value(text: &str, from: usize, key: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let rel = text[from..].find(&needle)?;
    let start = from + rel + needle.len();
    let end = text[start..].find('"')? + start;
    Some(text[start..end].to_string())
}

/// 扫描 Chrome 的 Local Storage leveldb，返回 `user_detail_agent` 的关键字段。
/// 值可能含非 UTF-8 字节（页面数据混写/页边界截断），所以先 lossy 解码再按文本
/// 扫描；token/realUserID/userName 都是 ASCII，按字符串抽取，不做脆弱的 JSON 平衡。
pub fn chrome_user_detail() -> Option<(String, String, String)> {
    let dir = chrome_leveldb_dir()?;
    let mut entries: Vec<PathBuf> = Vec::new();
    if let Ok(rd) = std::fs::read_dir(&dir) {
        for e in rd.flatten() {
            let p = e.path();
            if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                if ext == "log" || ext == "ldb" {
                    entries.push(p);
                }
            }
        }
    }
    const KEY: &str = "user_detail_agent";
    for p in entries {
        let Ok(bytes) = std::fs::read(&p) else { continue };
        let text = String::from_utf8_lossy(&bytes);
        let mut pos = 0usize;
        while let Some(rel) = text[pos..].find(KEY) {
            let after = pos + rel + KEY.len();
            let Some(window) = text.get(after..(after + 8192).min(text.len())) else {
                pos = after;
                continue;
            };
            if let (Some(token), Some(user_id)) = (
                quoted_value(window, 0, "token"),
                quoted_value(window, 0, "realUserID"),
            ) {
                let user_name = quoted_value(window, 0, "userName").unwrap_or_default();
                return Some((token, user_id, user_name));
            }
            pos = after;
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 适配器
// ---------------------------------------------------------------------------
pub struct MinimaxAdapter {
    base_url: String,
    client: reqwest::Client,
    cred_path: Option<PathBuf>,
}

impl Default for MinimaxAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl MinimaxAdapter {
    pub fn new() -> Self {
        Self::with_base_url(MINIMAX_ORIGIN).with_cred_path(managed_cred_path())
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

    fn cred(&self) -> Result<MinimaxCred, AdapterError> {
        let path = self.cred_path.clone().ok_or_else(|| {
            AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into())
        })?;
        read_cred(&path)
    }

    /// 从 Chrome 导入登录态并落盘。返回 (account_label, user_id)。
    /// Chrome 没登录/没找到 → 带提示的 AuthExpiredMsg。
    pub fn import_from_chrome(&self) -> Result<(String, String), AdapterError> {
        let (token, user_id, user_name) = chrome_user_detail()
            .ok_or_else(|| AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into()))?;
        if token.is_empty() || user_id.is_empty() {
            return Err(AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into()));
        }
        let cred = MinimaxCred {
            token,
            user_id: user_id.clone(),
            user_name: user_name.clone(),
            saved_at_ms: chrono::Utc::now().timestamp_millis(),
        };
        let path = self
            .cred_path
            .clone()
            .ok_or_else(|| AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()))?;
        write_cred(&path, &cred)?;
        let label = if user_name.is_empty() {
            user_id.clone()
        } else {
            user_name
        };
        Ok((label, user_id))
    }

    /// 登录探针：用传入的裸 token 验证服务端是否认账（未写凭据前调用）。
    /// 200=有效；401=失效；400=签名错（说明流程变了）；网络=临时失败。
    pub async fn probe(&self, token: &str, user_id: &str) -> Result<(), AdapterError> {
        self.signed(Method::GET, SIGNIN_STATUS_PATH, token, user_id, None)
            .await
            .map(|_| ())
    }

    /// 构造带网关签名的请求并发送。`body: None` = GET（签名体为空串）；
    /// `body: Some("{}")` = POST claim。
    async fn signed(
        &self,
        method: Method,
        path: &str,
        token: &str,
        user_id: &str,
        body: Option<&str>,
    ) -> Result<Value, AdapterError> {
        let ms = chrono::Utc::now().timestamp_millis();
        let ts = ms / 1000;
        let body_str = body.unwrap_or("");
        let sig = md5::hex(format!("{ts}{SIGNATURE_SALT}{body_str}").as_bytes());
        let url = format!("{}{}", self.base_url, path);

        // query 参数（与前端 mC() 同构；uuid/device_id 不参与签名校验，给稳定值即可）
        let params: Vec<(String, String)> = vec![
            ("device_platform", "web"),
            ("biz_id", BIZ_ID),
            ("app_id", APP_ID),
            ("version_code", VERSION_CODE),
            ("unix", &ms.to_string()),
            ("timezone_offset", TIMEZONE_OFFSET),
            ("sys_language", "zh"),
            ("lang", "zh"),
            ("uuid", "7e8a0471-4c9b-4b80-9de2-4a33b56e8ee5"),
            ("device_id", "0"),
            ("os_name", "Windows"),
            ("browser_name", "Chrome"),
            ("user_id", user_id),
            ("token", token),
            ("client", "web"),
            ("timezone_id", TIMEZONE_ID),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();

        let mut req = self
            .client
            .request(method.clone(), &url)
            .query(&params)
            .header("token", token)
            .header("x-timestamp", ts.to_string())
            .header("x-signature", sig)
            .header("Accept", "application/json");
        if method == Method::POST {
            req = req
                .header("Content-Type", "application/json")
                .body(body_str.to_string());
        }
        let resp = req.send().await?;
        let status = resp.status();
        // 401 = token 服务端不认（已失效）；400 = 签名/参数不对（接口变更）。
        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()));
        }
        if status == reqwest::StatusCode::BAD_REQUEST {
            return Err(AdapterError::SchemaChanged("HTTP 400 invalid signature".into()));
        }
        if let Some(e) = transport_error(status) {
            return Err(e);
        }
        let v: Value = resp
            .json()
            .await
            .map_err(|_| AdapterError::SchemaChanged("返回非 JSON".into()))?;
        let base = v.get("base_resp").cloned().unwrap_or(Value::Null);
        let code = base.get("status_code").and_then(Value::as_i64).unwrap_or(-1);
        if code != 0 {
            let msg = base
                .get("status_msg")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            if code == 1406010011 {
                // timezone_id 无效（理论不该发生：我们固定传 Asia/Shanghai）
                return Err(AdapterError::SchemaChanged(format!(
                    "timezone_id 无效: code={code} {msg}"
                )));
            }
            return Err(AdapterError::Business(format!("code={code} {msg}")));
        }
        Ok(v.get("data").cloned().unwrap_or(Value::Null))
    }

    /// 读取签到面板（7 天），返回 data.days。
    async fn panel(&self) -> Result<Value, AdapterError> {
        let cred = self.cred()?;
        self.signed(Method::GET, SIGNIN_STATUS_PATH, &cred.token, &cred.user_id, None)
            .await
            .map(|data| data.get("days").cloned().unwrap_or(Value::Null))
    }
}

#[async_trait::async_trait]
impl ProductAdapter for MinimaxAdapter {
    fn id(&self) -> String {
        "minimax".into()
    }

    async fn query_sign_status(&self) -> Result<SignStatus, AdapterError> {
        let days = self.panel().await?;
        // status 语义：1=Upcoming 2=Claimable 3=Claimed 4=Disabled
        let today_claimed = days.as_array().map(|arr| {
            arr.iter().any(|d| {
                d.get("is_today").and_then(Value::as_bool).unwrap_or(false)
                    && d.get("status").and_then(Value::as_i64) == Some(3)
            })
        });
        match today_claimed {
            Some(true) => Ok(SignStatus::SignedToday),
            _ => Ok(SignStatus::NotSigned),
        }
    }

    async fn sign_in(&self) -> Result<SignOutcome, AdapterError> {
        let cred = self.cred()?;
        let data = self
            .signed(
                Method::POST,
                SIGNIN_CLAIM_PATH,
                &cred.token,
                &cred.user_id,
                Some("{}"),
            )
            .await?;
        let claim_result = data.get("claim_result").and_then(Value::as_i64).unwrap_or(-1);
        let points = data.get("points").and_then(Value::as_i64).unwrap_or(0);
        let day_no = data.get("day_no").and_then(Value::as_i64).unwrap_or(0);
        match claim_result {
            1 => Ok(SignOutcome::Success(format!(
                "+{points}积分（第{day_no}天）"
            ))),
            2 => Ok(SignOutcome::AlreadySigned),
            other => Ok(SignOutcome::Failed(format!(
                "未知领取结果 claim_result={other}"
            ))),
        }
    }

    async fn fetch_credits(&self) -> Result<CreditsSnapshot, AdapterError> {
        let days = self.panel().await?;
        // 余额口径：本轮已领取（status=3）的天数积分之和 —— 面板给的就是签到进度。
        let balance = days
            .as_array()
            .map(|arr| {
                arr.iter()
                    .filter(|d| d.get("status").and_then(Value::as_i64) == Some(3))
                    .filter_map(|d| d.get("points").and_then(Value::as_i64))
                    .sum::<i64>() as f64
            })
            .unwrap_or(0.0);
        Ok(CreditsSnapshot {
            balance,
            today_used: 0.0,
            expiring_today: 0.0,
            expiring_tomorrow: 0.0,
            fetched_at_ms: chrono::Utc::now().timestamp_millis(),
        })
    }

    async fn account_label(&self) -> Result<String, AdapterError> {
        let cred = self.cred()?;
        if !cred.user_name.is_empty() {
            Ok(cred.user_name)
        } else if !cred.user_id.is_empty() {
            Ok(cred.user_id)
        } else {
            Ok("MiniMax 用户".into())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cred_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("signdock-mm-test-{}-{}", std::process::id(), tag));
        p
    }

    fn adapter(server_uri: &str, path: &Path) -> MinimaxAdapter {
        MinimaxAdapter::with_base_url(server_uri).with_cred_path(Some(path.to_path_buf()))
    }

    fn seed_cred(path: &Path, token: &str, user_id: &str) {
        write_cred(
            path,
            &MinimaxCred {
                token: token.into(),
                user_id: user_id.into(),
                user_name: "MiniMax313709".into(),
                saved_at_ms: 1,
            },
        )
        .unwrap();
    }

    fn panel_body(claimed_today: bool) -> serde_json::Value {
        // 7 天面板：第 2 天是今天；claimed_today=true 时它 status=3，否则 2（可领）。
        let days: Vec<serde_json::Value> = (1..=7)
            .map(|n| {
                let status = if n == 2 {
                    if claimed_today { 3 } else { 2 }
                } else {
                    1
                };
                serde_json::json!({
                    "day_no": n, "points": 400, "status": status,
                    "is_today": n == 2, "bonus_points": 0
                })
            })
            .collect();
        serde_json::json!({
            "data": {"scene": 2, "days": days},
            "base_resp": {"status_code": 0, "status_msg": "ok"}
        })
    }

    #[tokio::test]
    async fn status_not_signed_returns_not_signed() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/minimax-cloud/api/v1/signin/status"))
            .and(header("token", "tok"))
            .and(query_param("user_id", "u1"))
            .respond_with(ResponseTemplate::new(200).set_body_json(panel_body(false)))
            .mount(&s)
            .await;
        let p = cred_path("notsigned");
        seed_cred(&p, "tok", "u1");
        let r = adapter(&s.uri(), &p).query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert_eq!(r.unwrap(), SignStatus::NotSigned);
    }

    #[tokio::test]
    async fn status_signed_today_short_circuits() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/minimax-cloud/api/v1/signin/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(panel_body(true)))
            .mount(&s)
            .await;
        let p = cred_path("signed");
        seed_cred(&p, "tok", "u1");
        let r = adapter(&s.uri(), &p).query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert_eq!(r.unwrap(), SignStatus::SignedToday);
    }

    #[tokio::test]
    async fn claim_success_reports_points() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/minimax-cloud/api/v1/signin/claim"))
            .and(header("token", "tok"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {"claim_result": 1, "day_no": 2, "points": 400, "claim_id": "x"},
                "base_resp": {"status_code": 0, "status_msg": "ok"}
            })))
            .mount(&s)
            .await;
        let p = cred_path("claim");
        seed_cred(&p, "tok", "u1");
        let r = adapter(&s.uri(), &p).sign_in().await;
        std::fs::remove_file(&p).ok();
        assert!(matches!(&r, Ok(SignOutcome::Success(d)) if d.contains("400") && d.contains("2")));
    }

    #[tokio::test]
    async fn claim_already_signed_maps_to_already() {
        let s = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/minimax-cloud/api/v1/signin/claim"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "data": {"claim_result": 2, "day_no": 2, "points": 400, "claim_id": "x"},
                "base_resp": {"status_code": 0, "status_msg": "ok"}
            })))
            .mount(&s)
            .await;
        let p = cred_path("alreadysigned");
        seed_cred(&p, "tok", "u1");
        let r = adapter(&s.uri(), &p).sign_in().await;
        std::fs::remove_file(&p).ok();
        assert_eq!(r.unwrap(), SignOutcome::AlreadySigned);
    }

    #[tokio::test]
    async fn unauthorized_reports_auth_expired() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/minimax-cloud/api/v1/signin/status"))
            .respond_with(ResponseTemplate::new(401).set_body_json(serde_json::json!({
                "error": "invalid access token"
            })))
            .mount(&s)
            .await;
        let p = cred_path("auth");
        seed_cred(&p, "tok", "u1");
        let r = adapter(&s.uri(), &p).query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert!(matches!(r, Err(AdapterError::AuthExpiredMsg(_))));
    }

    #[tokio::test]
    async fn bad_signature_reports_schema_changed() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/minimax-cloud/api/v1/signin/status"))
            .respond_with(ResponseTemplate::new(400).set_body_json(serde_json::json!({
                "error": "invalid signature"
            })))
            .mount(&s)
            .await;
        let p = cred_path("badsig");
        seed_cred(&p, "tok", "u1");
        let r = adapter(&s.uri(), &p).query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert!(matches!(r, Err(AdapterError::SchemaChanged(_))));
    }

    #[tokio::test]
    async fn missing_cred_reports_login_hint() {
        let p = cred_path("nocred");
        std::fs::remove_file(&p).ok();
        let a = MinimaxAdapter::with_base_url("http://127.0.0.1:9").with_cred_path(Some(p.clone()));
        let r = a.query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert!(matches!(r, Err(AdapterError::AuthExpiredMsg(m)) if m.contains("导入")));
    }

    #[tokio::test]
    async fn network_failure_is_http_retryable() {
        let p = cred_path("net");
        seed_cred(&p, "tok", "u1");
        let a = MinimaxAdapter::with_base_url("http://127.0.0.1:1").with_cred_path(Some(p.clone()));
        let r = a.query_sign_status().await;
        std::fs::remove_file(&p).ok();
        assert!(matches!(r, Err(AdapterError::Http(_))));
    }

    /// 实况诊断：从 Chrome 导入，查状态，若未签则签到（幂等）
    #[tokio::test]
    #[ignore = "诊断：读本机 Chrome 导入 MiniMax 登录态并签到，不 mock"]
    async fn diag_minimax_live() {
        let a = MinimaxAdapter::new();
        match a.import_from_chrome() {
            Ok((label, uid)) => println!("imported = {label} ({uid})"),
            Err(e) => println!("import ERR = {e:?}"),
        }
        match a.account_label().await {
            Ok(l) => println!("account = {l}"),
            Err(e) => println!("account ERR = {e:?}"),
        }
        match a.query_sign_status().await {
            Ok(st) => println!("status = {st:?}"),
            Err(e) => println!("status ERR = {e:?}"),
        }
        match a.fetch_credits().await {
            Ok(c) => println!("credits = {:?}", c.balance),
            Err(e) => println!("credits ERR = {e:?}"),
        }
        println!("--- sign_in ---");
        match a.sign_in().await {
            Ok(out) => println!("sign_in = {out:?}"),
            Err(e) => println!("sign_in ERR = {e:?}"),
        }
    }
}
