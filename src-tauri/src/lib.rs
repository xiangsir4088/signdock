pub mod adapter;
pub mod commands;
pub mod engine;
pub mod notify;
pub mod scheduler;
pub mod secret;
pub mod store;
pub mod types;

use std::sync::Arc;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init())
        .plugin(tauri_plugin_opener::init())
        // 单实例锁：用户在任务栏上再点一次图标，Windows 会 CreateProcess 一个新进程；
        // 没这个插件的话每个新进程都会跑一遍 setup、各建一个 main 窗口，堆积成"好几个小窗"。
        // 装上后第二次（及后续）启动不会真起 UI，而是把参数/工作目录转发给已存在的
        // 首个实例，由首个实例把 main 窗口 show + set_focus 拉回前台。
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.show();
                let _ = w.unminimize();
                let _ = w.set_focus();
            } else {
                // 首个实例还在 setup 里没建完窗口 / 用户主动退到托盘前又点了图标，
                // 兜底建一个，避免出现"点了任务栏没反应"的情况。
                let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::default()).build();
            }
        }))
        .setup(|app| {
            use tauri::menu::{Menu, MenuItem};
            use tauri::tray::TrayIconBuilder;
            use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

            let data_dir = app.path().app_data_dir()?;
            let store = Arc::new(store::Store::open(&data_dir.join("signdock.db")).map_err(|e| e.0)?);
            app.manage(store.clone());
            // 按产品的执行权：调度 tick、托盘、界面「立即执行一次」三路触发靠它互斥
            let locks = Arc::new(engine::RunLocks::new(&commands::PRODUCT_IDS));
            app.manage(locks.clone());

            let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
            let sign_all = MenuItem::with_id(app, "sign_all", "立即签到（全部）", true, None::<&str>)?;
            let open_settings = MenuItem::with_id(app, "open", "打开设置", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&open_settings, &sign_all, &quit])?;
            let tray = TrayIconBuilder::with_id("main-tray")
                .menu(&menu)
                .tooltip("SignDock")
                .on_menu_event(move |app, ev| match ev.id.as_ref() {
                    "quit" => app.exit(0),
                    "open" => {
                        if let Some(w) = app.get_webview_window("main") { let _ = w.show(); let _ = w.set_focus(); }
                        else { let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::default()).build(); }
                    }
                    "sign_all" => {
                        let app2 = app.clone();
                        let store2 = app.state::<Arc<store::Store>>().inner().clone();
                        let locks2 = app.state::<Arc<engine::RunLocks>>().inner().clone();
                        for id in commands::PRODUCT_IDS {
                            // 正在执行的产品直接跳过：宁可少跑一次，也不重复签到
                            let cfg = match store2.config(id) { Ok(c) => c, Err(_) => continue };
                            if cfg.mode == types::Mode::Off { continue; }
                            let Some(guard) = locks2.acquire(id) else { continue };
                            // 一个产品一个任务：谁挂住都不影响其它产品立刻开跑
                            let app3 = app2.clone();
                            let store3 = store2.clone();
                            let adapter = commands::adapter_for(id);
                            tauri::async_runtime::spawn(async move {
                                let _guard = guard;
                                let _ = engine::run_once(adapter.as_ref(), &cfg, &store3,
                                    &notify::TauriNotifier(app3)).await;
                            });
                        }
                    }
                    _ => {}
                })
                .build(app)?;
            // 图标拿不到就留给系统默认：为一个装饰让托盘进程起不来，等于签到整个停摆
            if let Some(icon) = app.default_window_icon() {
                let _ = tray.set_icon(Some(icon.clone()));
            }

            // 调度循环：每 60s 看一眼。是否到期由 scheduler::is_due 判定，临时失败的补试
            // 也走同一条路（等间隔 + 次数上限），所以这里绝不 sleep —— 一次挂起的请求
            // 不该把别的产品和后面的 tick 一起拖住。
            let app_handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    let store = app_handle.state::<Arc<store::Store>>().inner().clone();
                    let locks = app_handle.state::<Arc<engine::RunLocks>>().inner().clone();
                    let now = chrono::Local::now();
                    let day_start = scheduler::day_start(now);
                    for id in commands::PRODUCT_IDS {
                        let Ok(cfg) = store.config(id) else { continue };
                        let Ok(today) = store.today_summary(id, day_start) else { continue };
                        if !scheduler::is_due(now, id, &cfg, &today) { continue; }
                        let Some(guard) = locks.acquire(id) else { continue };
                        // 一个产品一个任务：一次挂起的请求既拖不住别的产品，也拖不住后面的 tick
                        let app = app_handle.clone();
                        let store = store.clone();
                        let adapter = commands::adapter_for(id);
                        tauri::async_runtime::spawn(async move {
                            let _guard = guard;   // 执行权跟着任务走，跑完（或 panic）自动归还
                            let _ = engine::run_once(adapter.as_ref(), &cfg, &store,
                                &notify::TauriNotifier(app)).await;
                        });
                    }
                }
            });
            Ok(())
        })
        // 关窗=收进托盘：Tauri 默认最后一个窗口关闭即退出进程，那样调度循环和托盘一起消失，
        // 「无人值守自动签到」会变成「窗口必须开着才签到」。真正退出只走托盘「退出」。
        // 只拦主窗口：秒哒那个登录子窗口拦下来就再也关不掉了。
        .on_window_event(|window: &tauri::Window, event: &tauri::WindowEvent| {
            if window.label() != "main" { return; }
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                use tauri::Manager;
                if let Some(w) = window.get_webview_window("main") { let _ = w.hide(); }
            }
        })
        .invoke_handler(tauri::generate_handler![
            commands::get_overview, commands::set_config, commands::sign_now,
            commands::get_credits, commands::get_account, commands::wb_oauth_login, commands::wb_oauth_status,
            commands::md_login_open, commands::md_login_probe
        ])
        .run(tauri::generate_context!())
        .expect("error while running SignDock");
}
