//! 百度秒哒（www.miaoda.cn，货币叫「秒点」）。
//!
//! 与三家的根本差别：**没有可调的领取接口**。每日 100 秒点由服务端记录的一次
//! 「当日登录」事件触发。2026-09-25 02:36 实测判定：带有效会话 cookie 的**只读 GET**
//! 就算当日登录，当场发出 `登录赠送 channel=62 +100`，不需要走 SSO 回跳。
//! 所以这里的「签到」= 发一次产品自己也在用的只读请求，判定则直接读流水
//! （流水本身就是凭证，比 status 接口更硬）。
//!
//! 凭据来自 SignDock 自己的 WebView2 profile：用户在设置页登录一次，我们把那次
//! 会话的整包 cookie 封存下来（DPAPI），之后每天只转发它。绝不读用户浏览器的 cookie。

use std::path::{Path, PathBuf};

use chrono::{DateTime, Local, NaiveDate};
use serde_json::Value;

use super::{
    transport_error, AdapterError, CreditsSnapshot, ProductAdapter, SignOutcome, SignStatus,
};
use crate::adapter::http_client;

pub const MIAODA_ORIGIN: &str = "https://www.miaoda.cn";
const RECHARGE_PATH: &str = "/api/miaoda/score/query_recharge";
const LOG_LIST_PATH: &str = "/api/miaoda/score/log_list";

/// 流水里「每日登录赠送」的发放渠道号（实测：09-22 / 09-24 / 09-25 三条都是它）
pub const LOGIN_GRANT_CHANNEL: i64 = 62;
/// logType：2=收入，1=支出
pub const LOG_TYPE_INCOME: i64 = 2;
pub const LOG_TYPE_CONSUME: i64 = 1;

/// 流水分页：一页 50 条（与产品前端一致），最多翻 6 页。
/// 封顶是因为"翻到没装满为止"依赖接口的分页语义 —— 语义变了也不该把一次签到变成几十发请求。
const LOG_PAGE_SIZE: usize = 50;
const LOG_MAX_PAGES: usize = 6;

const NOT_LOGGED_IN_HINT: &str = "尚未登录秒哒；点「打开登录窗口」登录并取会话";
const SESSION_DEAD_HINT: &str = "秒哒会话已失效，请在设置页重新登录一次";
const NO_GRANT_HINT: &str = "已访问秒哒但没看到今日发放（可能本月登录赠送已达上限，或接口已变更）";

fn app_data_dir() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(Path::new(&appdata).join("com.signdock.app"))
}

/// SignDock 自有的秒哒会话凭据（可以回写；产品的 cookie 存储我们从不碰）
pub fn managed_cred_path() -> Option<PathBuf> {
    Some(app_data_dir()?.join("miaoda-cred.json"))
}

#[derive(Debug, Clone, PartialEq)]
pub struct MiaodaCred {
    /// 整包转发，不区分哪个名字才是凭据（会话 cookie 是 HttpOnly，前端脚本读不到）
    pub cookie_header: String,
    pub saved_at_ms: i64,
}

pub fn read_cred(path: &Path) -> Result<MiaodaCred, AdapterError> {
    let v = crate::secret::read_json(path).map_err(|e| match e {
        crate::secret::SecretError::NotFound => {
            AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into())
        }
        _ => AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()),
    })?;
    let cookie_header = v.get("cookieHeader").and_then(Value::as_str).unwrap_or("").to_string();
    if cookie_header.trim().is_empty() {
        return Err(AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()));
    }
    Ok(MiaodaCred { cookie_header, saved_at_ms: v.get("savedAtMs").and_then(Value::as_i64).unwrap_or(0) })
}

pub fn write_cred(path: &Path, cred: &MiaodaCred) -> Result<(), AdapterError> {
    let v = serde_json::json!({ "cookieHeader": cred.cookie_header, "savedAtMs": cred.saved_at_ms });
    crate::secret::write_json(path, &v).map_err(|_| AdapterError::AuthExpired)
}

/// "2026-09-25T02:36:31"（naive 本地）与 "2026-10-01T23:59:59.999+08:00"（带偏移）都出现
/// 在同一份接口里，必须两种都能归到本地日期。
pub fn local_date_of(s: &str) -> Option<NaiveDate> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Local).date_naive());
    }
    // 无偏移的那一种是服务端按本地时间写的，直接取日期部分
    s.get(..10).and_then(|head| NaiveDate::parse_from_str(head, "%Y-%m-%d").ok())
}

fn date_field(item: &Value, key: &str) -> Option<NaiveDate> {
    item.get(key).and_then(Value::as_str).and_then(local_date_of)
}

pub fn is_login_grant(item: &Value) -> bool {
    item.get("channel").and_then(Value::as_i64) == Some(LOGIN_GRANT_CHANNEL)
}

pub fn has_today_grant(items: &[Value], today: NaiveDate) -> bool {
    grant_amount_today(items, today).is_some()
}

/// 秒哒没有 status 接口：今天发没发，只能看流水里那条 ch62 在不在。
/// 返回 None 表示"今天还没有发放记录"，而不是"这条记录是 0 秒点"。
pub fn grant_amount_today(items: &[Value], today: NaiveDate) -> Option<f64> {
    items
        .iter()
        .filter(|i| is_login_grant(i))
        .find(|i| date_field(i, "createdAt") == Some(today))
        .and_then(|i| i.get("scoreCnt").and_then(Value::as_f64))
}

/// 当天全部支出流水之和（接口没有"今日已用"字段，只能从流水聚合）
pub fn today_consumed(items: &[Value], today: NaiveDate) -> f64 {
    items
        .iter()
        .filter(|i| i.get("logType").and_then(Value::as_i64) == Some(LOG_TYPE_CONSUME))
        .filter(|i| date_field(i, "createdAt") == Some(today))
        .filter_map(|i| i.get("scoreCnt").and_then(Value::as_f64))
        .map(f64::abs)
        .sum()
}

/// 业务体统一是 `{status, msg, data}`；status=0 才是成功。
/// 非 0 是服务端明确告诉我们的业务失败，不能报成「接口可能已变更」。
pub fn unwrap_data(v: Value) -> Result<Value, AdapterError> {
    let msg = v.get("msg").and_then(Value::as_str).unwrap_or("");
    if let Some(code) = v.get("status").and_then(Value::as_i64).filter(|s| *s != 0) {
        return Err(AdapterError::Business(
            if msg.is_empty() { format!("秒哒返回 status={code}") } else { msg.to_string() },
        ));
    }
    v.get("data")
        .filter(|d| !d.is_null())
        .cloned()
        .ok_or_else(|| AdapterError::SchemaChanged("秒哒响应里没有 data 字段".into()))
}

/// `monRemainingScore` 就是界面侧栏那个数（月剩余），口径已与实测互证；
/// `expiryDetail` 是"最先到期的一批"，配合"优先消耗即将到期"的规则足以填到期两格。
pub fn snapshot_from(data: &Value, today: NaiveDate, consumed_today: f64) -> CreditsSnapshot {
    let expiry = data.get("expiryDetail");
    let available = expiry.and_then(|e| e.get("available")).and_then(Value::as_f64).unwrap_or(0.0);
    let expires_on = expiry
        .and_then(|e| e.get("expirationTime"))
        .and_then(Value::as_str)
        .and_then(local_date_of);
    let (mut expiring_today, mut expiring_tomorrow) = (0.0, 0.0);
    let tomorrow = today.checked_add_signed(chrono::Duration::days(1));
    match expires_on {
        Some(d) if d == today => expiring_today = available,
        Some(d) if Some(d) == tomorrow => expiring_tomorrow = available,
        _ => {}
    }
    CreditsSnapshot {
        balance: data.get("monRemainingScore").and_then(Value::as_f64).unwrap_or(0.0),
        today_used: consumed_today,
        expiring_today,
        expiring_tomorrow,
        fetched_at_ms: now_ms(),
    }
}

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

/// 界面只精确到分钟：这一格的作用是"这份会话有多新"，不是闹钟。
fn fmt_saved(ms: i64) -> String {
    DateTime::<chrono::Utc>::from_timestamp_millis(ms)
        .map(|d| d.with_timezone(&Local).format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "未知时间".into())
}

/// WebView2 抓回来的 cookie 拼成请求头。整包转发（秒哒的会话凭据不止一条），
/// 同名只认第一条：跨 domain 的重复项里，先出现的那条才是站点自己在用的。
pub fn cookie_header(pairs: &[(String, String)]) -> String {
    let mut out = String::new();
    let mut seen: Vec<&str> = Vec::new();
    for (k, v) in pairs {
        if v.is_empty() || seen.contains(&k.as_str()) {
            continue;
        }
        seen.push(k);
        if !out.is_empty() {
            out.push_str("; ");
        }
        out.push_str(k);
        out.push('=');
        out.push_str(v);
    }
    out
}

fn today() -> NaiveDate {
    Local::now().date_naive()
}

pub struct MiaodaAdapter {
    base_url: String,
    cred_path: Option<PathBuf>,
}

impl Default for MiaodaAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl MiaodaAdapter {
    pub fn new() -> Self {
        Self { base_url: MIAODA_ORIGIN.into(), cred_path: managed_cred_path() }
    }

    pub fn with_base_url(url: &str) -> Self {
        Self { base_url: url.to_string(), cred_path: Some(PathBuf::from("Z:\\unused.json")) }
    }

    pub fn with_cred_path(mut self, path: PathBuf) -> Self {
        self.cred_path = Some(path);
        self
    }

    fn cred(&self) -> Result<MiaodaCred, AdapterError> {
        let path = self
            .cred_path
            .as_ref()
            .ok_or_else(|| AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into()))?;
        read_cred(path)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value, AdapterError> {
        let resp = req.send().await?;
        let status = resp.status();
        // 实测：会话失效回 401，body 是「内部服务器错误 Message: 未知的鉴权方式。。」。
        // 那是"要重新登录"，不是"接口变了"，也不该被当成临时网络错误重试成风暴。
        if matches!(status.as_u16(), 401 | 403) {
            return Err(AdapterError::AuthExpiredMsg(SESSION_DEAD_HINT.into()));
        }
        let body = resp.text().await?;
        // 传输层先判，业务体后判：网关 503 也可能带回一段合法 JSON，
        // 反过来写会把临时故障当成一次成功。
        if let Some(e) = transport_error(status) {
            return Err(e);
        }
        let v: Value = serde_json::from_str(&body)
            .map_err(|_| AdapterError::SchemaChanged(format!("秒哒返回不是 JSON（HTTP {}）", status.as_u16())))?;
        unwrap_data(v)
    }

    fn cookieed(&self, req: reqwest::RequestBuilder, cookie: &str) -> reqwest::RequestBuilder {
        req.header(reqwest::header::COOKIE, cookie)
            .header(reqwest::header::REFERER, format!("{}/", self.base_url))
    }

    /// 这一发就是"今天的登录"：产品自己的前端也在用同一个只读接口
    async fn get_recharge(&self, cookie: &str) -> Result<Value, AdapterError> {
        let url = format!("{}{}", self.base_url, RECHARGE_PATH);
        let req = self.cookieed(http_client().get(&url), cookie);
        self.send(req).await
    }

    /// 刚从登录窗口里取到的 cookie 算不算一次有效会话，只有服务端说了算。
    /// 用它探测而不是信"窗口里能看到主页"：未登录时秒哒也照样发匿名 cookie。
    pub async fn probe(&self, cookie: &str) -> Result<(), AdapterError> {
        self.get_recharge(cookie).await.map(|_| ())
    }

    async fn get_log_list(&self, cookie: &str, log_type: i64) -> Result<Vec<Value>, AdapterError> {
        // 只取第 1 页会少算：一天的流水能超过一页，而"今天那条 ch62 在不在"
        // 直接决定要不要再发一次登录。翻到某一页没装满为止，页数封顶。
        let url = format!("{}{}", self.base_url, LOG_LIST_PATH);
        let mut all = Vec::new();
        for page in 1..=LOG_MAX_PAGES {
            let req = self.cookieed(http_client().post(&url), cookie)
                .json(&serde_json::json!({ "logType": log_type, "pageNum": page, "pageSize": LOG_PAGE_SIZE }));
            let data = self.send(req).await?;
            let items = data.get("items").and_then(Value::as_array).cloned().unwrap_or_default();
            let short = items.len() < LOG_PAGE_SIZE;
            all.extend(items);
            if short {
                break;
            }
        }
        Ok(all)
    }
}

#[async_trait::async_trait]
impl ProductAdapter for MiaodaAdapter {
    fn id(&self) -> String {
        "miaoda".into()
    }

    async fn query_sign_status(&self) -> Result<SignStatus, AdapterError> {
        let cred = self.cred()?;
        let items = self.get_log_list(&cred.cookie_header, LOG_TYPE_INCOME).await?;
        Ok(if has_today_grant(&items, today()) {
            SignStatus::SignedToday
        } else {
            SignStatus::NotSigned
        })
    }

    async fn sign_in(&self) -> Result<SignOutcome, AdapterError> {
        let cred = self.cred()?;
        self.get_recharge(&cred.cookie_header).await?;
        let items = self.get_log_list(&cred.cookie_header, LOG_TYPE_INCOME).await?;
        // 只认流水，不认"请求成功了"：发放是服务端另一件事，成功了也可能没发
        match grant_amount_today(&items, today()) {
            Some(cnt) => Ok(SignOutcome::Success(format!("+{cnt} 秒点（登录赠送）"))),
            None => Ok(SignOutcome::Failed(NO_GRANT_HINT.into())),
        }
    }

    async fn fetch_credits(&self) -> Result<CreditsSnapshot, AdapterError> {
        let cred = self.cred()?;
        let data = self.get_recharge(&cred.cookie_header).await?;
        if data.get("monRemainingScore").and_then(Value::as_f64).is_none() {
            return Err(AdapterError::SchemaChanged("秒哒余额响应缺少 monRemainingScore".into()));
        }
        let spend = self.get_log_list(&cred.cookie_header, LOG_TYPE_CONSUME).await?;
        Ok(snapshot_from(&data, today(), today_consumed(&spend, today())))
    }

    /// 秒哒没有一个"只读本地就能拿到昵称"的地方（它的用户信息接口在另一个 host 上，
    /// 且要联网）。所以这一格报"本机这份会话是什么时候封存的"—— 这恰恰是用户要看的信息：
    /// 会话越新，越不可能已经失效。没登录时把该点哪个按钮说出来。
    async fn account_label(&self) -> Result<String, AdapterError> {
        let cred = self.cred()?;
        Ok(if cred.saved_at_ms <= 0 {
            "已封存秒哒会话".into()
        } else {
            format!("已封存秒哒会话 · 保存于 {}", fmt_saved(cred.saved_at_ms))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn day(y: i32, m: u32, d: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(y, m, d).unwrap()
    }

    fn pair(k: &str, v: &str) -> (String, String) {
        (k.into(), v.into())
    }

    #[test]
    fn cookie_header_joins_pairs_and_drops_anonymous_blanks() {
        let header = cookie_header(&[pair("SESSION", "abc"), pair("PTSID", ""), pair("OTHER", "x")]);
        assert_eq!(header, "SESSION=abc; OTHER=x");
    }

    /// 跨 domain 同名（.miaoda.cn 与 www.miaoda.cn 各一条）时留第一条：
    /// 两条都发出去服务端只会看到一串没定义的东西，那比挑一条更难查。
    #[test]
    fn cookie_header_keeps_first_of_duplicate_names() {
        let header = cookie_header(&[pair("S", "1st"), pair("S", "2nd")]);
        assert_eq!(header, "S=1st");
    }

    /// 实测样本（2026-09-25 02:36:31 那条），字段名一个都没改
    fn grant(created_at: &str, cnt: f64) -> Value {
        json!({
            "id": 63352004, "bizId": "user-ehxnvcus6dxc", "userId": "user-ehxnvcus6dxc",
            "createdAt": created_at, "startTime": created_at,
            "expirationTime": "2026-10-03T00:00:00", "scoreCnt": cnt,
            "channel": 62, "operatorName": "登录赠送", "logType": 2,
        })
    }

    #[test]
    fn local_date_of_accepts_naive_local_and_offset_forms() {
        assert_eq!(local_date_of("2026-09-25T02:36:31"), Some(day(2026, 9, 25)));
        assert_eq!(local_date_of("2026-10-01T23:59:59.999+08:00"), Some(day(2026, 10, 1)));
        assert_eq!(local_date_of(""), None);
        assert_eq!(local_date_of("昨天"), None);
    }

    #[test]
    fn today_grant_needs_channel_62_and_todays_date() {
        let items = vec![grant("2026-09-25T02:36:31", 100.0)];
        assert!(has_today_grant(&items, day(2026, 9, 25)));
        assert_eq!(grant_amount_today(&items, day(2026, 9, 25)), Some(100.0));
    }

    /// 昨天那条流水不能被当成今天已发放，否则永远等不到第二次执行
    #[test]
    fn yesterdays_grant_is_not_todays() {
        let items = vec![grant("2026-09-24T11:14:51", 100.0)];
        assert!(!has_today_grant(&items, day(2026, 9, 25)));
        assert_eq!(grant_amount_today(&items, day(2026, 9, 25)), None);
    }

    /// 新手任务等其它渠道的收入流水同表混放，渠道号不对就不算
    #[test]
    fn other_channels_do_not_count_as_login_grant() {
        let items = vec![json!({"createdAt": "2026-09-25T00:00:00", "scoreCnt": 50.0,
                                "channel": 8, "operatorName": "系统发放", "logType": 2})];
        assert!(!has_today_grant(&items, day(2026, 9, 25)));
    }

    /// 流水里的支出条目实测带正数（−40 显示在界面上），但服务端哪天改成负数
    /// 也不该让"今日消耗"变成负值 —— 取绝对值求和
    #[test]
    fn consumed_sums_only_todays_rows() {
        let items = vec![
            json!({"createdAt": "2026-09-25T09:00:00", "scoreCnt": 40.0, "logType": 1}),
            json!({"createdAt": "2026-09-25T10:00:00", "scoreCnt": 20.0, "logType": 1}),
            json!({"createdAt": "2026-09-25T11:00:00", "scoreCnt": -30.0, "logType": 1}),
            json!({"createdAt": "2026-09-24T09:00:00", "scoreCnt": 99.0, "logType": 1}),
            json!({"createdAt": "2026-09-25T12:00:00", "scoreCnt": 100.0, "logType": 2}),
        ];
        assert_eq!(today_consumed(&items, day(2026, 9, 25)), 90.0);
    }

    #[test]
    fn snapshot_maps_balances_and_expiry_bucket() {
        let data = json!({
            "dayRemainingScore": 100.0, "monRemainingScore": 309.0,
            "monLimitScore": 9000.0, "dayLimitScore": 15000.0,
            "expiryDetail": {"available": 100.0, "total": 100.0, "type": 62,
                             "expirationTime": "2026-09-26T23:59:59.999+08:00"},
        });
        let s = snapshot_from(&data, day(2026, 9, 25), 40.0);
        assert_eq!(s.balance, 309.0);
        assert_eq!(s.today_used, 40.0);
        assert_eq!(s.expiring_today, 0.0);
        assert_eq!(s.expiring_tomorrow, 100.0);
        assert!(s.fetched_at_ms > 0);
    }

    #[test]
    fn snapshot_without_expiry_detail_reports_zero_expiry() {
        let data = json!({"monRemainingScore": 10.0});
        let s = snapshot_from(&data, day(2026, 9, 25), 0.0);
        assert_eq!(s.balance, 10.0);
        assert_eq!((s.expiring_today, s.expiring_tomorrow), (0.0, 0.0));
    }

    #[test]
    fn non_zero_business_status_is_business_not_schema() {
        let e = unwrap_data(json!({"status": 403, "msg": "今日额度已用尽"})).unwrap_err();
        match e {
            AdapterError::Business(m) => assert!(m.contains("今日额度已用尽"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_data_object_reports_schema_changed() {
        let e = unwrap_data(json!({"status": 0, "msg": ""})).unwrap_err();
        assert!(matches!(e, AdapterError::SchemaChanged(_)));
    }

    #[test]
    fn cred_roundtrip_keeps_cookie_and_is_sealed_on_disk() {
        let p = tmp_path("cred");
        write_cred(&p, &MiaodaCred { cookie_header: "session=SECRET-COOKIE".into(), saved_at_ms: 1 })
            .unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        assert!(!raw.contains("SECRET-COOKIE"), "会话 cookie 落盘必须是封存件");
        assert_eq!(read_cred(&p).unwrap().cookie_header, "session=SECRET-COOKIE");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn absent_cred_file_asks_for_one_login_instead_of_crashing() {
        let e = read_cred(&tmp_path("absent")).unwrap_err();
        match e {
            AdapterError::AuthExpiredMsg(m) => assert!(m.contains("打开登录窗口"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    fn tmp_path(tag: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("signdock-miaoda-{tag}-{}.json", std::process::id()));
        p
    }

    /// 实测过的两份响应；`recharge_seen` 用来验证"先发只读 GET 再查流水"的顺序
    async fn mount_real_responses(s: &MockServer, items: Vec<Value>, cookie: &str) {
        Mock::given(method("GET"))
            .and(path(RECHARGE_PATH))
            .and(wiremock::matchers::header("cookie", cookie))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": 0, "msg": "",
                "data": {"dayRemainingScore": 100.0, "monRemainingScore": 309.0,
                         "monLimitScore": 9000.0, "dayLimitScore": 15000.0},
            })))
            .mount(s)
            .await;
        Mock::given(method("POST"))
            .and(path(LOG_LIST_PATH))
            .and(wiremock::matchers::header("cookie", cookie))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": 0, "msg": "", "data": {"items": items, "total": items.len()},
            })))
            .mount(s)
            .await;
    }

    fn adapter_with(s: &MockServer, p: PathBuf) -> MiaodaAdapter {
        MiaodaAdapter::with_base_url(&s.uri()).with_cred_path(p)
    }

    /// 一天的流水可以远超一页（pageSize=50）。只取第 1 页时，那条 ch62 若落在第 2 页
    /// 就等于"今天没发过"——于是白重发一次登录，积分面板也跟着少算。
    #[tokio::test]
    async fn income_log_paging_reaches_the_second_page() {
        let s = MockServer::start().await;
        let filler: Vec<Value> = (0..50)
            .map(|_| json!({"channel": 30, "scoreCnt": 1.0, "createdAt": "2026-01-01T10:00:00"}))
            .collect();
        let granted = json!({"channel": LOGIN_GRANT_CHANNEL, "scoreCnt": 100.0,
            "createdAt": format!("{}T10:00:00", today_str())});
        Mock::given(method("POST"))
            .and(path(LOG_LIST_PATH))
            .and(wiremock::matchers::body_json(json!({"logType": 2, "pageNum": 1, "pageSize": 50})))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"status": 0, "data": {"items": filler, "total": 51}})))
            .mount(&s).await;
        Mock::given(method("POST"))
            .and(path(LOG_LIST_PATH))
            .and(wiremock::matchers::body_json(json!({"logType": 2, "pageNum": 2, "pageSize": 50})))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"status": 0, "data": {"items": [granted], "total": 51}})))
            .mount(&s).await;
        let p = tmp_path("paging");
        write_cred(&p, &MiaodaCred { cookie_header: "c=1".into(), saved_at_ms: 0 }).unwrap();
        let st = adapter_with(&s, p.clone()).query_sign_status().await.unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(st, SignStatus::SignedToday, "第 2 页那条 ch62 也要被看见");
    }

    fn today_str() -> String {
        chrono::Local::now().date_naive().format("%Y-%m-%d").to_string()
    }

    #[tokio::test]
    async fn gateway_503_with_json_body_is_retryable_not_a_success() {
        // 网关抖动也可能回一段合法 JSON。先 parse 后看状态码，就会把 503 读成"今日已发放"。
        let s = MockServer::start().await;
        let p = tmp_path("503");
        write_cred(&p, &MiaodaCred { cookie_header: "c=1".into(), saved_at_ms: 0 }).unwrap();
        Mock::given(method("GET"))
            .and(path(RECHARGE_PATH))
            .respond_with(ResponseTemplate::new(503)
                .set_body_json(json!({"status": 0, "data": {"monRemainingScore": 999}})))
            .mount(&s)
            .await;
        let e = adapter_with(&s, p.clone()).fetch_credits().await.unwrap_err();
        std::fs::remove_file(&p).ok();
        assert!(matches!(e, AdapterError::Http(_)), "5xx 属可重试的传输层错误: {e:?}");
    }

    #[tokio::test]
    async fn http_404_reports_schema_changed_not_retryable() {
        // 路径没了 = 接口变了。重试改变不了结果。
        let s = MockServer::start().await;
        let p = tmp_path("404");
        write_cred(&p, &MiaodaCred { cookie_header: "c=1".into(), saved_at_ms: 0 }).unwrap();
        Mock::given(method("GET"))
            .and(path(RECHARGE_PATH))
            .respond_with(ResponseTemplate::new(404).set_body_json(json!({"status": 0, "data": {}})))
            .mount(&s)
            .await;
        let e = adapter_with(&s, p.clone()).fetch_credits().await.unwrap_err();
        std::fs::remove_file(&p).ok();
        assert!(matches!(&e, AdapterError::SchemaChanged(m) if m.contains("404")), "{e:?}");
    }

    #[tokio::test]
    async fn status_reads_ledger_not_a_sign_in_endpoint() {
        let s = MockServer::start().await;
        let p = tmp_path("st-ok");
        write_cred(&p, &MiaodaCred { cookie_header: "c=1".into(), saved_at_ms: 0 }).unwrap();
        let today = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        mount_real_responses(&s, vec![grant(&today, 100.0)], "c=1").await;
        assert_eq!(adapter_with(&s, p.clone()).query_sign_status().await.unwrap(),
                   SignStatus::SignedToday);
        std::fs::remove_file(&p).ok();
    }

    /// 秒哒没有"窗口"概念，但"今天没发"必须报 NotSigned 而不是 Unknown：
    /// Unknown 在引擎里是转人工，NotSigned 才是"该动手了"
    #[tokio::test]
    async fn status_without_todays_record_is_not_signed() {
        let s = MockServer::start().await;
        let p = tmp_path("st-no");
        write_cred(&p, &MiaodaCred { cookie_header: "c=1".into(), saved_at_ms: 0 }).unwrap();
        mount_real_responses(&s, vec![grant("2020-01-01T00:00:00", 100.0)], "c=1").await;
        assert_eq!(adapter_with(&s, p.clone()).query_sign_status().await.unwrap(),
                   SignStatus::NotSigned);
        std::fs::remove_file(&p).ok();
    }

    #[tokio::test]
    async fn sign_in_visits_the_product_then_confirms_from_ledger() {
        let s = MockServer::start().await;
        let p = tmp_path("sign");
        write_cred(&p, &MiaodaCred { cookie_header: "c=1".into(), saved_at_ms: 0 }).unwrap();
        let today = Local::now().format("%Y-%m-%dT%H:%M:%S").to_string();
        // 只有"访问之后"才出现的那条发放记录 —— 访问之前查流水是空的
        Mock::given(method("GET"))
            .and(path(RECHARGE_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": 0, "data": {"monRemainingScore": 309.0}})))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path(LOG_LIST_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "status": 0, "data": {"items": [grant(&today, 100.0)]}})))
            .mount(&s)
            .await;
        let out = adapter_with(&s, p.clone()).sign_in().await.unwrap();
        std::fs::remove_file(&p).ok();
        assert!(matches!(&out, SignOutcome::Success(d) if d.contains("100")), "{out:?}");
    }

    /// 访问了但流水里没有：不能报成功（用户会以为已经到账）
    #[tokio::test]
    async fn sign_in_without_record_reports_failed_without_retrying() {
        let s = MockServer::start().await;
        let p = tmp_path("sign-empty");
        write_cred(&p, &MiaodaCred { cookie_header: "c=1".into(), saved_at_ms: 0 }).unwrap();
        Mock::given(method("GET"))
            .and(path(RECHARGE_PATH))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"status": 0, "data": {}})))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path(LOG_LIST_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"status": 0, "data": {"items": []}})))
            .mount(&s)
            .await;
        let out = adapter_with(&s, p.clone()).sign_in().await.unwrap();
        std::fs::remove_file(&p).ok();
        match out {
            SignOutcome::Failed(m) => assert!(m.contains("上限") || m.contains("变更"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    /// 会话失效的实测签名：401 + 「未知的鉴权方式」。必须是 AuthExpired（转人工重登），
    /// 既不能重试成风暴，也不能报成"接口可能已变更"
    #[tokio::test]
    async fn dead_session_reports_auth_expired_with_relogin_hint() {
        let s = MockServer::start().await;
        let p = tmp_path("dead");
        write_cred(&p, &MiaodaCred { cookie_header: "stale=1".into(), saved_at_ms: 0 }).unwrap();
        Mock::given(method("POST"))
            .and(path(LOG_LIST_PATH))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "status": 401, "code": "server.error",
                "msg": "内部服务器错误 Message: 未知的鉴权方式。。",
            })))
            .mount(&s)
            .await;
        let e = adapter_with(&s, p.clone()).query_sign_status().await.unwrap_err();
        std::fs::remove_file(&p).ok();
        match e {
            AdapterError::AuthExpiredMsg(m) => assert!(m.contains("重新登录"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    /// 登录窗口里抓到的整包 cookie 要服务端认账才算登录完成。
    /// 探针通过 = 封存这份会话；不通过就继续等 —— 秒哒在未登录时也会发匿名 cookie，
    /// 只看"窗口里有 cookie"会把匿名会话封存成"已登录"。
    #[tokio::test]
    async fn probe_passes_only_when_the_server_accepts_the_session() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(RECHARGE_PATH))
            .and(wiremock::matchers::header("cookie", "SESSION=real"))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"status": 0, "data": {"monRemainingScore": 309.0}})))
            .mount(&s)
            .await;
        MiaodaAdapter::with_base_url(&s.uri()).probe("SESSION=real").await.unwrap();
    }

    #[tokio::test]
    async fn probe_on_anonymous_session_is_auth_expired_not_a_success() {
        let s = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path(RECHARGE_PATH))
            .respond_with(ResponseTemplate::new(401).set_body_json(json!({
                "status": 401, "msg": "内部服务器错误 Message: 未知的鉴权方式。。",
            })))
            .mount(&s)
            .await;
        let e = MiaodaAdapter::with_base_url(&s.uri()).probe("BIDUPSID=anon").await.unwrap_err();
        assert!(matches!(e, AdapterError::AuthExpiredMsg(_)), "{e:?}");
    }

    /// 服务端换了字段名 → 报接口变更，而不是静悄悄显示 0
    #[tokio::test]
    async fn unexpected_recharge_shape_reports_schema_changed() {
        let s = MockServer::start().await;
        let p = tmp_path("shape");
        write_cred(&p, &MiaodaCred { cookie_header: "c=1".into(), saved_at_ms: 0 }).unwrap();
        Mock::given(method("GET"))
            .and(path(RECHARGE_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"status": 0, "data": {"whatever": 1}})))
            .mount(&s)
            .await;
        Mock::given(method("POST"))
            .and(path(LOG_LIST_PATH))
            .respond_with(ResponseTemplate::new(200)
                .set_body_json(json!({"status": 0, "data": {"items": []}})))
            .mount(&s)
            .await;
        let e = adapter_with(&s, p.clone()).fetch_credits().await.unwrap_err();
        std::fs::remove_file(&p).ok();
        assert!(matches!(e, AdapterError::SchemaChanged(_)), "{e:?}");
    }

    /// 账号那一格对秒哒报的是"这份会话有多新"：它不联网，也未必要用户重登，
    /// 但用户一眼能看出是不是该重新登录一次了。
    #[tokio::test]
    async fn account_label_reports_when_the_session_was_sealed() {
        let p = tmp_path("label");
        let saved = chrono::Utc::now().timestamp_millis();
        write_cred(&p, &MiaodaCred { cookie_header: "c=1".into(), saved_at_ms: saved }).unwrap();
        let label = MiaodaAdapter::with_base_url("http://127.0.0.1:9").with_cred_path(p.clone())
            .account_label().await.unwrap();
        std::fs::remove_file(&p).ok();
        assert!(label.starts_with("已封存秒哒会话 · 保存于 20"), "{label}");
    }

    #[tokio::test]
    async fn account_label_without_cred_names_the_button() {
        let p = tmp_path("label-none");
        std::fs::remove_file(&p).ok();
        let e = MiaodaAdapter::with_base_url("http://127.0.0.1:9")
            .with_cred_path(p).account_label().await.unwrap_err();
        match e {
            AdapterError::AuthExpiredMsg(m) => assert!(m.contains("打开登录窗口"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test]
    async fn never_called_without_cred_and_hint_names_the_button() {        let p = tmp_path("no-cred");
        std::fs::remove_file(&p).ok();
        let e = MiaodaAdapter::with_base_url("http://127.0.0.1:9")
            .with_cred_path(p)
            .sign_in()
            .await
            .unwrap_err();
        match e {
            AdapterError::AuthExpiredMsg(m) => assert!(m.contains("打开登录窗口"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn timestamp_helpers_stay_local_based() {
        // 流水时间是本地无偏移串，测试口径必须与之一致（跨时区机器上也不能错位一天）
        let now_local = Local::now();
        let ts = now_local.format("%Y-%m-%dT%H:%M:%S").to_string();
        assert_eq!(local_date_of(&ts), Some(now_local.date_naive()));
        assert_eq!(today(), now_local.date_naive());
    }
}
