//! 通知抽象：解耦引擎与 Tauri 通知插件，便于测试。

pub trait Notifier: Send + Sync {
    fn notify(&self, title: &str, body: &str);
}

/// 测试用通知器：把 (title, body) 记录到共享 Vec。
pub struct RecordingNotifier {
    sink: std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>,
}

impl RecordingNotifier {
    pub fn new() -> (Self, std::sync::Arc<std::sync::Mutex<Vec<(String, String)>>>) {
        let sink = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        (Self { sink: sink.clone() }, sink)
    }
}

impl Notifier for RecordingNotifier {
    fn notify(&self, title: &str, body: &str) {
        self.sink.lock().unwrap().push((title.into(), body.into()));
    }
}
