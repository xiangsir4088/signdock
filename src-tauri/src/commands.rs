use crate::adapter::miaoda::{self, MiaodaAdapter};
use crate::adapter::mock::MockAdapter;
use crate::adapter::qoder::QoderAdapter;
use crate::adapter::trae::TraeAdapter;
use crate::adapter::workbuddy::{self, WorkbuddyAdapter};
use crate::adapter::{AdapterError, CreditsSnapshot, ProductAdapter};
use crate::engine::{run_once, RunLocks, RunResult};
use crate::notify::TauriNotifier;
use crate::store::Store;
use crate::types::{ProductConfig, RunRow};
use std::sync::Arc;

pub const PRODUCT_IDS: [&str; 4] = ["workbuddy", "trae", "qoder", "miaoda"];

/// 秒哒登录窗口的标签。它用 SignDock 自己的 WebView2 profile，绝不借道用户的浏览器。
const MD_WINDOW: &str = "md-login";

/// SIGNDOCK_MOCK_URL 存在时所有产品走 mock（开发/演示）。
pub fn mock_base_url() -> String {
    std::env::var("SIGNDOCK_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:9".into())
}

/// 工厂：三款产品均走真实适配器（本机登录态自动提取）；设 SIGNDOCK_MOCK_URL 时一切走 mock（开发/演示）。
pub fn adapter_for(product_id: &str) -> Box<dyn ProductAdapter> {
    let force_mock = std::env::var("SIGNDOCK_MOCK_URL").is_ok();
    if force_mock {
        return Box::new(MockAdapter::new(product_id, mock_base_url()));
    }
    match product_id {
        "workbuddy" => Box::new(WorkbuddyAdapter::new()),
        "trae" => Box::new(TraeAdapter::new()),
        "qoder" => Box::new(QoderAdapter::new()),
        "miaoda" => Box::new(MiaodaAdapter::new()),
        _ => Box::new(MockAdapter::new(product_id, mock_base_url())),
    }
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Overview {
    pub product_id: String,
    pub config: ProductConfig,
    pub runs: Vec<RunRow>,
}

type State<'a> = tauri::State<'a, Arc<Store>>;

/// 读失败就上报：静默回落到出厂值会把「库里到底存了什么」变成猜谜。
#[tauri::command]
pub fn get_overview(store: State<'_>) -> Result<Vec<Overview>, String> {
    PRODUCT_IDS.iter().map(|id| Ok(Overview {
        product_id: id.to_string(),
        config: store.config(id).map_err(|e| e.0)?,
        runs: store.runs(id, 20).map_err(|e| e.0)?,
    })).collect()
}

#[tauri::command]
pub fn set_config(store: State<'_>, config: ProductConfig) -> Result<(), String> {
    store.set_config(&config).map_err(|e| e.0)
}

#[tauri::command]
pub async fn sign_now(app: tauri::AppHandle, store: State<'_>, locks: tauri::State<'_, Arc<RunLocks>>,
    product_id: String) -> Result<RunResult, String> {
    // 和调度循环、托盘「签到（全部）」抢同一把按产品的锁：抢不到就说明正在跑，
    // 直接回话而不是排队再跑一遍（重复签到、重复通知都源于此）。
    let _guard = locks.acquire(&product_id).ok_or_else(|| "该产品正在执行中，请稍后再点。".to_string())?;
    let cfg = store.config(&product_id).map_err(|e| e.0)?;
    let adapter = adapter_for(&product_id);
    let notifier = TauriNotifier(app);
    Ok(run_once(adapter.as_ref(), &cfg, &store, &notifier).await)
}

/// 积分与账号标识都由适配器自备凭证（本机登录态 / SignDock 自有凭据文件），
/// 因此这两个命令连配置都不用读——没有任何路径能把用户粘贴的 token 送进请求。
#[tauri::command]
pub async fn get_credits(product_id: String) -> Result<CreditsSnapshot, String> {
    adapter_for(&product_id).fetch_credits().await.map_err(|e| e.to_string())
}

/// 当前登录账号的可读标识（只读本机凭据，绝不返回 token）。
#[tauri::command]
pub async fn get_account(product_id: String) -> Result<String, String> {
    adapter_for(&product_id).account_label().await.map_err(|e| e.to_string())
}

/// 发起 WorkBuddy 官方 OAuth 登录：拿到浏览器地址并打开，返回 loginId 供前端轮询。
/// 用户不必再手工粘贴 token（竞品 workbuddy-switch 的同一条路线）。
#[tauri::command]
pub async fn wb_oauth_login(app: tauri::AppHandle) -> Result<workbuddy::OAuthSession, String> {
    use tauri_plugin_opener::OpenerExt;
    let sess = workbuddy::WorkbuddyAdapter::new().oauth_start().await.map_err(|e| e.to_string())?;
    // 打不开浏览器不算登录失败：前端仍展示可点击链接，轮询照常进行
    let _ = app.opener().open_url(&sess.auth_url, None::<&str>);
    Ok(sess)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthStatus {
    pub done: bool,
    pub nickname: String,
}

/// 取会话的结果。`window_gone` 单独成字段，是为了让前端知道"按钮还可以再点"
/// 和"窗口已经没了，点了也没用"是两回事。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MdProbe {
    pub done: bool,
    pub window_gone: bool,
}

/// 发起秒哒登录：开一个 SignDock 自有的 WebView2 窗口，用户在里面登一次。
/// 必须是 async 命令：同步命令跑在主线程上，也就是正停在事件循环回调里，
/// build() 内部那几处「等 WebView2 回调」的消息泵就永远等不到下一步 ——
/// 窗口建出来了、停在 about:blank，命令不返回，整个进程连托盘一起僵死。
#[tauri::command]
pub async fn md_login_open(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
    if let Some(w) = app.get_webview_window(MD_WINDOW) {
        let _ = w.set_focus();
        return Ok(());
    }
    let url = tauri::Url::parse(miaoda::MIAODA_ORIGIN).map_err(|e| e.to_string())?;
    // 专属的 WebView2 数据目录（必须是绝对路径：它直接当成 user data folder 交给宿主）。
    // 单开一个目录有两个好处：这份会话跨重启留存，且它绝不与用户自己的 Edge/Chrome 共用 profile。
    let profile = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("md-webview");
    // capabilities 里只给了 main 窗口权限：这个窗口里跑的是百度的页面，
    // 它一条 SignDock 的命令都调不动。
    // 登录页里 target=_blank / window.open 是常态（扫码、第三方授权按钮）。不装这个
    // 处理器的话 wry 走的是「SetHandled(true) 但不给新窗口」那条分支，弹窗被静默吞掉，
    // 用户点了没反应，看起来就像"这个窗口登不上"。
    WebviewWindowBuilder::new(&app, MD_WINDOW, WebviewUrl::External(url))
        .title("秒哒登录（SignDock 自有窗口）")
        .inner_size(1100.0, 800.0)
        .data_directory(profile)
        .on_new_window(|_, _| tauri::webview::NewWindowResponse::Allow)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 取走那次登录的整包 cookie。
/// 探针本身也要服务端认账才算登录成功 —— 未登录时秒哒同样会发匿名 cookie。
///
/// 这个命令只能由用户点一次，绝不能用定时器反复调：`cookies_for_url` 在 Windows 上要
/// 走进 WebView2 的宿主消息泵，反复调就是把一条本不该出现在事件循环里的嵌套泵反复叠上去。
#[tauri::command]
pub async fn md_login_probe(app: tauri::AppHandle) -> Result<MdProbe, String> {
    use tauri::Manager;
    let Some(win) = app.get_webview_window(MD_WINDOW) else {
        return Ok(MdProbe { done: false, window_gone: true });
    };
    let url = tauri::Url::parse(miaoda::MIAODA_ORIGIN).map_err(|e| e.to_string())?;
    let jar = win.cookies_for_url(url).map_err(|e| e.to_string())?;
    let pairs: Vec<(String, String)> =
        jar.iter().map(|c| (c.name().to_string(), c.value().to_string())).collect();
    let header = miaoda::cookie_header(&pairs);
    if header.is_empty() {
        return Ok(MdProbe { done: false, window_gone: false });
    }
    match MiaodaAdapter::new().probe(&header).await {
        Ok(()) => {
            let path = miaoda::managed_cred_path().ok_or("无法定位应用数据目录")?;
            let cred = miaoda::MiaodaCred {
                cookie_header: header,
                saved_at_ms: chrono::Utc::now().timestamp_millis(),
            };
            miaoda::write_cred(&path, &cred).map_err(|e| e.to_string())?;
            let _ = win.close();
            Ok(MdProbe { done: true, window_gone: false })
        }
        // 还没登录时秒哒回什么都可能：HTTP 401，或 HTTP 200 带 status≠0。对"取会话"
        // 这件事来说它们是同一件事 —— 还不能用来登录，那就继续等，而不是把服务端原文
        // 甩给用户当成他做错了什么。
        Err(e) => match md_fault_of(&e) {
            MdFault::KeepWaiting => Ok(MdProbe { done: false, window_gone: false }),
            MdFault::Network => Err("网络不通，秒哒没有回话：查一下网络再点一次「取会话」，登录窗口不用重开".into()),
            MdFault::Stop(m) => Err(m),
        },
    }
}

/// 轮询登录结果。完成时把凭据写入 SignDock 自有文件（此后刷新可自动续期，无需再登录）。
/// 响应只含昵称，绝不把 token 送进 webview。
#[tauri::command]
pub async fn wb_oauth_status(login_id: String) -> Result<OAuthStatus, String> {
    let adapter = workbuddy::WorkbuddyAdapter::new();
    match adapter.oauth_poll(&login_id).await {
        Ok(None) => Ok(OAuthStatus { done: false, nickname: String::new() }),
        // 轮询期间的网络抖动不该终结会话
        Err(AdapterError::Http(_)) => Ok(OAuthStatus { done: false, nickname: String::new() }),
        Ok(Some(c)) => {
            let path = workbuddy::managed_cred_path().ok_or("无法定位应用数据目录")?;
            workbuddy::write_managed_cred(&path, &c.cred).map_err(|e| e.to_string())?;
            Ok(OAuthStatus { done: true, nickname: c.nickname })
        }
        Err(e) => Err(e.to_string()),
    }
}

/// 探针失败对用户意味着哪一类事 —— 混成一类就会让人对着一台断网的机器反复点按钮。
#[derive(Debug, PartialEq)]
enum MdFault {
    /// 秒哒不认这包 cookie：还没登录，继续等
    KeepWaiting,
    /// 请求根本没到秒哒：网络问题，重试的钥匙在用户手里
    Network,
    /// 回话压根不是秒哒的接口（被打到别的站点去了）：立刻停下来
    Stop(String),
}

fn md_fault_of(e: &AdapterError) -> MdFault {
    match e {
        AdapterError::Http(_) => MdFault::Network,
        AdapterError::SchemaChanged(m) => MdFault::Stop(m.clone()),
        _ => MdFault::KeepWaiting,
    }
}

#[cfg(test)]
mod tests {
    use super::{md_fault_of, MdFault};
    use crate::adapter::AdapterError;

    #[test]
    fn network_fault_is_not_reported_as_not_logged_in() {
        assert_eq!(md_fault_of(&AdapterError::Http("超时".into())), MdFault::Network);
    }

    #[test]
    fn auth_and_business_faults_mean_still_not_logged_in() {
        for e in [AdapterError::AuthExpired,
                  AdapterError::AuthExpiredMsg("未知的鉴权方式".into()),
                  AdapterError::Business("status=-1".into())] {
            assert_eq!(md_fault_of(&e), MdFault::KeepWaiting, "{e:?} 该继续等");
        }
    }

    #[test]
    fn non_json_answer_stops_and_keeps_the_server_reason() {
        assert_eq!(md_fault_of(&AdapterError::SchemaChanged("不是 JSON".into())),
            MdFault::Stop("不是 JSON".into()));
    }
}
