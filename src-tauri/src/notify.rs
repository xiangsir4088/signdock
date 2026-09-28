//! 通知抽象：解耦引擎与 Tauri 通知插件，便于测试。

pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str);
}

/// 测试用通知器：把 (title, body) 记录到共享 Vec。
pub type NotifySink = std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>;

pub struct RecordingNotifier {
    sink: NotifySink,
}

impl RecordingNotifier {
    pub fn new() -> (Self, NotifySink) {
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (Self { sink: sink.clone() }, sink)
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, title: &str, body: &str) {
        self.sink.lock().unwrap().push((title.into(), body.into()));
    }
}

/// Tauri 通知插件实现：经 AppHandle 发送系统通知。
pub struct TauriNotifier(pub tauri::AppHandle);

impl Notifier for TauriNotifier {
    fn notify(&self, title: &str, body: &str) {
        use tauri_plugin_notification::NotificationExt;
        let _ = self.0.notification()
            .builder()
            .title(title)
            .body(body)
            .show();
    }
}
