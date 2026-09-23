use crate::adapter::mock::MockAdapter;
use crate::adapter::workbuddy::WorkbuddyAdapter;
use crate::adapter::{CreditsSnapshot, ProductAdapter};
use crate::engine::{run_once, RunResult};
use crate::notify::TauriNotifier;
use crate::store::Store;
use crate::types::{ProductConfig, RunRow};
use std::sync::Arc;

pub const PRODUCT_IDS: [&str; 3] = ["workbuddy", "trae", "qoder"];

/// trae/qoder 尚无真实适配器：MockAdapter 默认走本地不可达端口 → 网络错误路径。
/// SIGNDOCK_MOCK_URL 存在时所有产品走 mock（开发/演示）。
pub fn mock_base_url() -> String {
    std::env::var("SIGNDOCK_MOCK_URL").unwrap_or_else(|_| "http://127.0.0.1:9".into())
}

/// 工厂：workbuddy 走真实适配器（本机登录态自动提取）；设 SIGNDOCK_MOCK_URL 时一切走 mock（开发/演示）。
pub fn adapter_for(product_id: &str) -> Box<dyn ProductAdapter> {
    let force_mock = std::env::var("SIGNDOCK_MOCK_URL").is_ok();
    if !force_mock && product_id == "workbuddy" {
        Box::new(WorkbuddyAdapter::new())
    } else {
        Box::new(MockAdapter::new(product_id, mock_base_url()))
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

#[tauri::command]
pub fn get_overview(store: State<'_>) -> Vec<Overview> {
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
pub fn set_config(store: State<'_>, cfg: ProductConfig) -> Result<(), String> {
    store.set_config(&cfg).map_err(|e| e.0)
}

#[tauri::command]
pub fn get_runs(store: State<'_>, product_id: String) -> Vec<RunRow> {
    store.runs(&product_id, 50).unwrap_or_default()
}

#[tauri::command]
pub async fn sign_now(app: tauri::AppHandle, store: State<'_>, product_id: String) -> Result<RunResult, String> {
    let cfg = store.config(&product_id).map_err(|e| e.0)?;
    let adapter = adapter_for(&product_id);
    let notifier = TauriNotifier(app);
    Ok(run_once(adapter.as_ref(), &cfg, &*store, &notifier, chrono::Utc::now().timestamp()).await)
}

#[tauri::command]
pub async fn get_credits(store: State<'_>, product_id: String) -> Result<CreditsSnapshot, String> {
    let cfg = store.config(&product_id).map_err(|e| e.0)?;
    let adapter = adapter_for(&product_id);
    adapter.fetch_credits(cfg.token.as_deref().unwrap_or("")).await.map_err(|e| e.to_string())
}
