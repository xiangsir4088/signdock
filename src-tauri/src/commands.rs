use crate::adapter::doubao::{self, DoubaoAdapter};
use crate::adapter::lingjing::{self, LingjingAdapter};
use crate::adapter::miaoda::{self, MiaodaAdapter};
use crate::adapter::minimax::{self, MinimaxAdapter};
use crate::adapter::mock::MockAdapter;
use crate::adapter::qoder::QoderAdapter;
use crate::adapter::trae::TraeAdapter;
use crate::adapter::workbuddy::{self, WorkbuddyAdapter};
use crate::adapter::{AdapterError, CreditsSnapshot, ProductAdapter};
use crate::engine::{run_once, RunLocks, RunResult};
use crate::notify::TauriNotifier;
use crate::store::Store;
use crate::types::{is_discontinued, ProductConfig, RunRow};
use std::sync::Arc;

pub const PRODUCT_IDS: [&str; 7] =
    ["workbuddy", "trae", "qoder", "miaoda", "doubao", "lingjing", "minimax"];

/// 秒哒登录窗口的标签。它用 SignDock 自己的 WebView2 profile，绝不借道用户的浏览器。
const MD_WINDOW: &str = "md-login";

/// 灵镜登录窗口的标签。同样用 SignDock 自有的 WebView2 profile。
const LJ_WINDOW: &str = "lj-login";

/// MiniMax 登录窗口的标签。同样用 SignDock 自有的 WebView2 profile（兜底登录路径）。
const MM_WINDOW: &str = "mm-login";

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
        "doubao" => Box::new(DoubaoAdapter::new()),
        "lingjing" => Box::new(LingjingAdapter::new()),
        "minimax" => Box::new(MinimaxAdapter::new()),
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
    PRODUCT_IDS
        .iter()
        .map(|id| {
            Ok(Overview {
                product_id: id.to_string(),
                config: store.config(id).map_err(|e| e.0)?,
                runs: store.runs(id, 20).map_err(|e| e.0)?,
            })
        })
        .collect()
}

#[tauri::command]
pub fn set_config(store: State<'_>, config: ProductConfig) -> Result<(), String> {
    store.set_config(&config).map_err(|e| e.0)
}

#[tauri::command]
pub async fn sign_now(
    app: tauri::AppHandle,
    store: State<'_>,
    locks: tauri::State<'_, Arc<RunLocks>>,
    product_id: String,
) -> Result<RunResult, String> {
    // 和调度循环、托盘「签到（全部）」抢同一把按产品的锁：抢不到就说明正在跑，
    // 直接回话而不是排队再跑一遍（重复签到、重复通知都源于此）。
    let _guard = locks
        .acquire(&product_id)
        .ok_or_else(|| "该产品正在执行中，请稍后再点。".to_string())?;
    // 厂商已下线：手动「立即执行一次」同样不碰死接口，给一句可读的话收尾。
    if is_discontinued(&product_id) {
        return Err("该产品的签到功能已被厂商下线，SignDock 不再执行；界面已置为「已下线」。".into());
    }
    let cfg = store.config(&product_id).map_err(|e| e.0)?;
    let adapter = adapter_for(&product_id);
    let notifier = TauriNotifier(app);
    Ok(run_once(adapter.as_ref(), &cfg, &store, &notifier).await)
}

/// 积分与账号标识都由适配器自备凭证（本机登录态 / SignDock 自有凭据文件），
/// 因此这两个命令连配置都不用读——没有任何路径能把用户粘贴的 token 送进请求。
#[tauri::command]
pub async fn get_credits(product_id: String) -> Result<CreditsSnapshot, String> {
    adapter_for(&product_id)
        .fetch_credits()
        .await
        .map_err(|e| e.to_string())
}

/// 当前登录账号的可读标识（只读本机凭据，绝不返回 token）。
#[tauri::command]
pub async fn get_account(product_id: String) -> Result<String, String> {
    adapter_for(&product_id)
        .account_label()
        .await
        .map_err(|e| e.to_string())
}

/// 豆包多账号：查询所有 profile 的今日签到状态（只读，不签到）。
/// 客户端运行时活跃 profile 的 Cookies 库被独占锁，会标记为 `locked`。
#[tauri::command]
pub async fn db_query_accounts() -> Result<Vec<doubao::DbAccountView>, String> {
    let dir = doubao::doubao_user_data_dir().ok_or("无法定位豆包数据目录")?;
    Ok(doubao::query_all_accounts(&dir, doubao::PROD_BASE).await)
}

/// 豆包多账号：逐个 profile 签到（已签的跳过）。返回每账号结果。
#[tauri::command]
pub async fn db_sign_all() -> Result<Vec<doubao::DbAccountResult>, String> {
    let dir = doubao::doubao_user_data_dir().ok_or("无法定位豆包数据目录")?;
    Ok(doubao::sign_all_accounts(&dir, doubao::PROD_BASE).await)
}

/// 发起 WorkBuddy 官方 OAuth 登录：拿到浏览器地址并打开，返回 loginId 供前端轮询。
/// 用户不必再手工粘贴 token（竞品 workbuddy-switch 的同一条路线）。
#[tauri::command]
pub async fn wb_oauth_login(app: tauri::AppHandle) -> Result<workbuddy::OAuthSession, String> {
    use tauri_plugin_opener::OpenerExt;
    let sess = workbuddy::WorkbuddyAdapter::new()
        .oauth_start()
        .await
        .map_err(|e| e.to_string())?;
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
        return Ok(MdProbe {
            done: false,
            window_gone: true,
        });
    };
    let url = tauri::Url::parse(miaoda::MIAODA_ORIGIN).map_err(|e| e.to_string())?;
    let jar = win.cookies_for_url(url).map_err(|e| e.to_string())?;
    let pairs: Vec<(String, String)> = jar
        .iter()
        .map(|c| (c.name().to_string(), c.value().to_string()))
        .collect();
    let header = miaoda::cookie_header(&pairs);
    if header.is_empty() {
        return Ok(MdProbe {
            done: false,
            window_gone: false,
        });
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
            Ok(MdProbe {
                done: true,
                window_gone: false,
            })
        }
        // 还没登录时秒哒回什么都可能：HTTP 401，或 HTTP 200 带 status≠0。对"取会话"
        // 这件事来说它们是同一件事 —— 还不能用来登录，那就继续等，而不是把服务端原文
        // 甩给用户当成他做错了什么。
        Err(e) => match md_fault_of(&e) {
            MdFault::KeepWaiting => Ok(MdProbe {
                done: false,
                window_gone: false,
            }),
            MdFault::Network => {
                Err("网络不通，秒哒没有回话：查一下网络再点一次「取会话」，登录窗口不用重开".into())
            }
            MdFault::Stop(m) => Err(m),
        },
    }
}

/// 发起灵镜登录：开一个 SignDock 自有的 WebView2 窗口，用户在里面登一次。
/// 灵镜是纯 JWT（token 存 localStorage `uumit-aigc-auth`），不是 cookie——所以取会话走
/// `eval_with_callback` 读 localStorage，而不是 `cookies_for_url`。
#[tauri::command]
pub async fn lj_login_open(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
    if let Some(w) = app.get_webview_window(LJ_WINDOW) {
        let _ = w.set_focus();
        return Ok(());
    }
    let url = tauri::Url::parse(lingjing::LINGJING_ORIGIN).map_err(|e| e.to_string())?;
    let profile = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("lj-webview");
    WebviewWindowBuilder::new(&app, LJ_WINDOW, WebviewUrl::External(url))
        .title("灵镜登录（SignDock 自有窗口）")
        .inner_size(1100.0, 800.0)
        .data_directory(profile)
        .on_new_window(|_, _| tauri::webview::NewWindowResponse::Allow)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 取走那次登录的 token。灵镜用 JWT（localStorage），probe 同样要服务端认账：
/// 未登录时灵镜回 HTTP 401 / code=40100，对"取会话"而言就是还不能用，继续等。
#[tauri::command]
pub async fn lj_login_probe(app: tauri::AppHandle) -> Result<MdProbe, String> {
    use tauri::Manager;
    let Some(win) = app.get_webview_window(LJ_WINDOW) else {
        return Ok(MdProbe {
            done: false,
            window_gone: true,
        });
    };
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    // eval_with_callback 的回调是 Fn（可能被调用多次），oneshot 的 Sender 只能 send 一次；
    // 用 Mutex<Option<_>> 把"发过了没"装进回调里，重复调用直接忽略。
    let cell = std::sync::Mutex::new(Some(tx));
    win.eval_with_callback(
        "localStorage.getItem('uumit-aigc-auth') || ''",
        move |result| {
            if let Some(tx) = cell.lock().ok().and_then(|mut g| g.take()) {
                let _ = tx.send(result);
            }
        },
    )
    .map_err(|e| e.to_string())?;
    let result = match tokio::time::timeout(std::time::Duration::from_secs(3), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => return Err("读取登录状态失败".into()),
        Err(_) => return Err("读取登录状态超时，请再点一次「取会话」".into()),
    };
    // eval 结果序列化为 JSON 字符串（localStorage 值是字符串，这里外层是一层转义）。
    let outer: serde_json::Value =
        serde_json::from_str(&result).map_err(|e| format!("解析登录数据失败: {e}"))?;
    let storage_str = outer.as_str().unwrap_or("");
    if storage_str.is_empty() {
        return Ok(MdProbe {
            done: false,
            window_gone: false,
        });
    }
    let auth: serde_json::Value = serde_json::from_str(storage_str)
        .map_err(|e| format!("解析登录数据失败: {e}"))?;
    let access = auth["state"]["accessToken"]
        .as_str()
        .unwrap_or("")
        .to_string();
    let refresh = auth["state"]["refreshToken"]
        .as_str()
        .unwrap_or("")
        .to_string();
    if access.is_empty() {
        return Ok(MdProbe {
            done: false,
            window_gone: false,
        });
    }
    match LingjingAdapter::new().probe(&access).await {
        Ok(()) => {
            let path = lingjing::managed_cred_path().ok_or("无法定位应用数据目录")?;
            let cred = lingjing::LingjingCred {
                access_token: access,
                refresh_token: refresh,
                saved_at_ms: chrono::Utc::now().timestamp_millis(),
            };
            lingjing::write_cred(&path, &cred).map_err(|e| e.to_string())?;
            let _ = win.close();
            Ok(MdProbe {
                done: true,
                window_gone: false,
            })
        }
        Err(e) => match lj_fault_of(&e) {
            LjFault::KeepWaiting => Ok(MdProbe {
                done: false,
                window_gone: false,
            }),
            LjFault::Network => {
                Err("网络不通，灵镜没有回话：查一下网络再点一次「取会话」，登录窗口不用重开".into())
            }
            LjFault::Stop(m) => Err(m),
        },
    }
}

/// 灵镜探针失败对用户意味着哪一类事（与秒哒同构，但单独命名避免把灵镜的接口变更说成秒哒）。
#[derive(Debug, PartialEq)]
enum LjFault {
    KeepWaiting,
    Network,
    Stop(String),
}

fn lj_fault_of(e: &AdapterError) -> LjFault {
    match e {
        AdapterError::Http(_) => LjFault::Network,
        AdapterError::SchemaChanged(m) => LjFault::Stop(m.clone()),
        _ => LjFault::KeepWaiting,
    }
}

/// MiniMax 登录的故障分类（与灵镜同构：网络可重试，接口变更要停下来说清楚）。
#[derive(Debug, PartialEq)]
enum MmFault {
    KeepWaiting,
    Network,
    Stop(String),
}

fn mm_fault_of(e: &AdapterError) -> MmFault {
    match e {
        AdapterError::Http(_) => MmFault::Network,
        AdapterError::SchemaChanged(m) => MmFault::Stop(m.clone()),
        _ => MmFault::KeepWaiting,
    }
}

/// 从 Chrome 导入 MiniMax 登录态（用户已在浏览器登录 agent.minimax.cn 的最省事路径）。
/// 成功 = 返回账号标签；失败 = 可读提示。
#[tauri::command]
pub fn mm_import_chrome() -> Result<MmImportResult, String> {
    let adapter = MinimaxAdapter::new();
    let (label, user_id) = adapter.import_from_chrome().map_err(|e| e.to_string())?;
    Ok(MmImportResult { label, user_id })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MmImportResult {
    pub label: String,
    pub user_id: String,
}

/// 发起 MiniMax 登录（兜底路径）：开一个 SignDock 自有的 WebView2 窗口，用户在里面登一次。
/// 正常路径是「从 Chrome 导入」，此窗口仅在 Chrome 未登录/想换账号时使用。
#[tauri::command]
pub async fn mm_login_open(app: tauri::AppHandle) -> Result<(), String> {
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
    if let Some(w) = app.get_webview_window(MM_WINDOW) {
        let _ = w.set_focus();
        return Ok(());
    }
    let url = tauri::Url::parse(minimax::MINIMAX_ORIGIN).map_err(|e| e.to_string())?;
    let profile = app
        .path()
        .app_local_data_dir()
        .map_err(|e| e.to_string())?
        .join("mm-webview");
    WebviewWindowBuilder::new(&app, MM_WINDOW, WebviewUrl::External(url))
        .title("MiniMax 登录（SignDock 自有窗口）")
        .inner_size(1100.0, 800.0)
        .data_directory(profile)
        .on_new_window(|_, _| tauri::webview::NewWindowResponse::Allow)
        .build()
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 取走登录窗口里的会话。MiniMax 用 JWT（localStorage `user_detail_agent`），
/// 不是 cookie——所以走 `eval_with_callback` 读 localStorage，再让服务端验一次货。
#[tauri::command]
pub async fn mm_login_probe(app: tauri::AppHandle) -> Result<MdProbe, String> {
    use tauri::Manager;
    let Some(win) = app.get_webview_window(MM_WINDOW) else {
        return Ok(MdProbe {
            done: false,
            window_gone: true,
        });
    };
    let (tx, rx) = tokio::sync::oneshot::channel::<String>();
    let cell = std::sync::Mutex::new(Some(tx));
    win.eval_with_callback(
        "localStorage.getItem('user_detail_agent') || ''",
        move |result| {
            if let Some(tx) = cell.lock().ok().and_then(|mut g| g.take()) {
                let _ = tx.send(result);
            }
        },
    )
    .map_err(|e| e.to_string())?;
    let result = match tokio::time::timeout(std::time::Duration::from_secs(3), rx).await {
        Ok(Ok(r)) => r,
        Ok(Err(_)) => return Err("读取登录状态失败".into()),
        Err(_) => return Err("读取登录状态超时，请再点一次「取会话」".into()),
    };
    let outer: serde_json::Value =
        serde_json::from_str(&result).map_err(|e| format!("解析登录数据失败: {e}"))?;
    let storage_str = outer.as_str().unwrap_or("");
    if storage_str.is_empty() {
        return Ok(MdProbe {
            done: false,
            window_gone: false,
        });
    }
    let auth: serde_json::Value = serde_json::from_str(storage_str)
        .map_err(|e| format!("解析登录数据失败: {e}"))?;
    let token = auth["token"].as_str().unwrap_or("").to_string();
    let user_id = auth["realUserID"].as_str().unwrap_or("").to_string();
    let user_name = auth["userName"].as_str().unwrap_or("").to_string();
    if token.is_empty() || user_id.is_empty() {
        return Ok(MdProbe {
            done: false,
            window_gone: false,
        });
    }
    match MinimaxAdapter::new().probe(&token, &user_id).await {
        Ok(()) => {
            let path = minimax::managed_cred_path().ok_or("无法定位应用数据目录")?;
            let cred = minimax::MinimaxCred {
                token,
                user_id,
                user_name,
                saved_at_ms: chrono::Utc::now().timestamp_millis(),
            };
            minimax::write_cred(&path, &cred).map_err(|e| e.to_string())?;
            let _ = win.close();
            Ok(MdProbe {
                done: true,
                window_gone: false,
            })
        }
        Err(e) => match mm_fault_of(&e) {
            MmFault::KeepWaiting => Ok(MdProbe {
                done: false,
                window_gone: false,
            }),
            MmFault::Network => Err(
                "网络不通，MiniMax 没有回话：查一下网络再点一次「取会话」，登录窗口不用重开".into(),
            ),
            MmFault::Stop(m) => Err(m),
        },
    }
}

/// 轮询登录结果。完成时把凭据写入 SignDock 自有文件（此后刷新可自动续期，无需再登录）。
/// 响应只含昵称，绝不把 token 送进 webview。
#[tauri::command]
pub async fn wb_oauth_status(login_id: String) -> Result<OAuthStatus, String> {
    let adapter = workbuddy::WorkbuddyAdapter::new();
    match adapter.oauth_poll(&login_id).await {
        Ok(None) => Ok(OAuthStatus {
            done: false,
            nickname: String::new(),
        }),
        // 轮询期间的网络抖动不该终结会话
        Err(AdapterError::Http(_)) => Ok(OAuthStatus {
            done: false,
            nickname: String::new(),
        }),
        Ok(Some(c)) => {
            let path = workbuddy::managed_cred_path().ok_or("无法定位应用数据目录")?;
            workbuddy::write_managed_cred(&path, &c.cred).map_err(|e| e.to_string())?;
            Ok(OAuthStatus {
                done: true,
                nickname: c.nickname,
            })
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
        assert_eq!(
            md_fault_of(&AdapterError::Http("超时".into())),
            MdFault::Network
        );
    }

    #[test]
    fn auth_and_business_faults_mean_still_not_logged_in() {
        for e in [
            AdapterError::AuthExpired,
            AdapterError::AuthExpiredMsg("未知的鉴权方式".into()),
            AdapterError::Business("status=-1".into()),
        ] {
            assert_eq!(md_fault_of(&e), MdFault::KeepWaiting, "{e:?} 该继续等");
        }
    }

    #[test]
    fn non_json_answer_stops_and_keeps_the_server_reason() {
        assert_eq!(
            md_fault_of(&AdapterError::SchemaChanged("不是 JSON".into())),
            MdFault::Stop("不是 JSON".into())
        );
    }
}
