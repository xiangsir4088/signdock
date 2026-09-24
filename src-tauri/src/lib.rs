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
        .plugin(tauri_plugin_opener::init())
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::TrayIconBuilder;
            use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

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
                                let adapter = commands::adapter_for(id);
                                let _ = engine::run_once(adapter.as_ref(), &cfg, &store2,
                                    &notify::TauriNotifier(app2.clone()), chrono::Utc::now().timestamp()).await;
                            }
                        });
                    }
                    _ => {}
                })
                .build(app)?;

            // 调度循环：每 60s tick，对到期产品执行引擎；临时网络错误最多重试 2 次（间隔 300s）
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    let store = app_handle.state::<Arc<store::Store>>().inner().clone();
                    let now = chrono::Local::now();
                    for id in commands::PRODUCT_IDS {
                        let Ok(cfg) = store.config(id) else { continue };
                        // 当日任意一次运行（不限 outcome）都会阻断重复触发，防止失败产品全天每 60s 重试
                        let last = store.last_run_at(id).ok().flatten();
                        if scheduler::is_due(now, id, &cfg, last) {
                            let adapter = commands::adapter_for(id);
                            let max_retries: u32 = 2;
                            let mut attempt: u32 = 0;
                            loop {
                                let outcome = engine::run_once(adapter.as_ref(), &cfg, &store,
                                    &notify::TauriNotifier(app_handle.clone()), chrono::Utc::now().timestamp()).await;
                                // FailedRetryable 仅由 AdapterError::Http 产生，等价于 engine::should_retry 的 Http 分支
                                // （attempt < max_retries 时重试）；AuthExpired/SchemaChanged 已归入 Failed 不重试。
                                if !matches!(&outcome, engine::RunResult::FailedRetryable(_)) || attempt >= max_retries { break; }
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
            commands::get_overview, commands::set_config, commands::sign_now, commands::get_runs,
            commands::get_credits, commands::wb_oauth_login, commands::wb_oauth_status
        ])
        .run(tauri::generate_context!())
        .expect("error while running SignDock");
}
