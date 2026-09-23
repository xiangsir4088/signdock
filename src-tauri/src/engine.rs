use crate::adapter::{AdapterError, ProductAdapter, SignOutcome, SignStatus};
use crate::notify::Notifier;
#[cfg(test)]
use crate::notify::RecordingNotifier;
use crate::store::Store;
use crate::types::{Mode, ProductConfig};

#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RunResult {
    Skipped,
    AlreadySigned,
    Success(String),
    Reminded,
    NeedManual(String),
    Failed(String),
    /// 临时性网络错误，调度循环可重试；落库 outcome 仍记为 "failed"。
    FailedRetryable(String),
}

/// AuthExpired / SchemaChanged 不重试；Http 临时错误在重试上限内重试。
pub fn should_retry(err: &AdapterError, attempt: u32, max_retries: u32) -> bool {
    match err {
        AdapterError::Http(_) => attempt < max_retries,
        AdapterError::AuthExpired | AdapterError::SchemaChanged(_) => false,
    }
}

fn notify_both(n: &dyn Notifier, product_id: &str, body: &str) {
    n.notify(&format!("SignDock · {product_id}"), body);
}

pub async fn run_once(
    adapter: &dyn ProductAdapter,
    cfg: &ProductConfig,
    store: &Store,
    notifier: &dyn Notifier,
    _now_epoch: i64,
) -> RunResult {
    let id = cfg.product_id.as_str();
    if cfg.mode == Mode::Off {
        return RunResult::Skipped;
    }
    let token = cfg.token.as_deref().unwrap_or("");
    if token.trim().is_empty() && !adapter.accepts_empty_token() {
        notify_both(notifier, id, "缺少凭证：请登录产品（自动提取）或在设置中粘贴 token");
        store.record_run(id, "failed", "缺少凭证").ok();
        return RunResult::Failed("缺少凭证".into());
    }
    let status = match adapter.query_sign_status(token).await {
        Ok(s) => s,
        Err(e) => {
            let msg = e.to_string();
            notify_both(notifier, id, &format!("查询签到状态失败：{msg}"));
            store.record_run(id, "failed", &msg).ok();
            // 仅 Http 临时错误可重试；AuthExpired/SchemaChanged 重试无意义。
            return match e {
                AdapterError::Http(_) => RunResult::FailedRetryable(msg),
                _ => RunResult::Failed(msg),
            };
        }
    };
    if status == SignStatus::SignedToday {
        store.record_run(id, "alreadySigned", "今日已签到").ok();
        return RunResult::AlreadySigned;
    }
    if cfg.mode == Mode::Remind {
        notify_both(notifier, id, "该签到了（今日未签到）");
        store.record_run(id, "reminded", "提醒已发出").ok();
        return RunResult::Reminded;
    }
    match adapter.sign_in(token).await {
        Ok(SignOutcome::Success(detail)) => {
            notify_both(notifier, id, &format!("签到成功 {detail}"));
            store.record_run(id, "success", &detail).ok();
            RunResult::Success(detail)
        }
        Ok(SignOutcome::AlreadySigned) => {
            store.record_run(id, "alreadySigned", "今日已签到").ok();
            RunResult::AlreadySigned
        }
        Ok(SignOutcome::NeedManual(msg)) => {
            notify_both(notifier, id, &msg);
            store.record_run(id, "needManual", &msg).ok();
            RunResult::NeedManual(msg)
        }
        Ok(SignOutcome::Failed(msg)) => {
            notify_both(notifier, id, &format!("签到失败：{msg}"));
            store.record_run(id, "failed", &msg).ok();
            RunResult::Failed(msg)
        }
        Err(e) => {
            let msg = e.to_string();
            notify_both(notifier, id, &format!("签到失败：{msg}"));
            store.record_run(id, "failed", &msg).ok();
            // 同上：仅 Http 临时错误可重试。
            match e {
                AdapterError::Http(_) => RunResult::FailedRetryable(msg),
                _ => RunResult::Failed(msg),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapter::mock::MockAdapter;
    use crate::store::Store;
    use crate::types::{Mode, ProductConfig};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn cfg(mode: Mode) -> ProductConfig {
        ProductConfig { product_id: "workbuddy".into(), mode, time_of_day: "09:00".into(), token: Some("tok".into()) }
    }
    fn store() -> Store { Store::in_memory().unwrap() }

    #[tokio::test]
    async fn remind_mode_notifies_without_signing() {
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"signed": false})))
            .mount(&s).await;
        let (n, rec) = RecordingNotifier::new();
        let a = MockAdapter::new("workbuddy", s.uri());
        let store = store();
        let r = run_once(&a, &cfg(Mode::Remind), &store, &n, 0).await;
        assert_eq!(r, RunResult::Reminded);
        assert_eq!(store.runs("workbuddy", 10).unwrap().len(), 1);
        assert_eq!(store.runs("workbuddy", 10).unwrap()[0].outcome, "reminded");
        assert_eq!(rec.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn auto_mode_signs_in_and_records_success() {
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"signed": false})))
            .mount(&s).await;
        Mock::given(method("POST")).and(path("/sign"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"ok": true, "points": 5})))
            .mount(&s).await;
        let (n, rec) = RecordingNotifier::new();
        let a = MockAdapter::new("workbuddy", s.uri());
        let store = store();
        let r = run_once(&a, &cfg(Mode::Auto), &store, &n, 0).await;
        assert!(matches!(&r, RunResult::Success(d) if d.contains('5')));
        assert_eq!(store.runs("workbuddy", 10).unwrap()[0].outcome, "success");
        assert_eq!(rec.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn signed_today_short_circuits() {
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"signed": true})))
            .mount(&s).await;
        let (n, _rec) = RecordingNotifier::new();
        let a = MockAdapter::new("workbuddy", s.uri());
        let store = store();
        let r = run_once(&a, &cfg(Mode::Auto), &store, &n, 0).await;
        assert_eq!(r, RunResult::AlreadySigned);
        assert_eq!(store.last_success_at("workbuddy").unwrap(), None); // 已签≠本次成功记录
    }

    #[tokio::test]
    async fn missing_token_fails_with_notification() {
        let (n, rec) = RecordingNotifier::new();
        let s = MockServer::start().await;
        let a = MockAdapter::new("workbuddy", s.uri());
        let cfg = ProductConfig { product_id: "workbuddy".into(), mode: Mode::Auto, time_of_day: "09:00".into(), token: None };
        let store = store();
        let r = run_once(&a, &cfg, &store, &n, 0).await;
        assert!(matches!(r, RunResult::Failed(_)));
        assert_eq!(rec.lock().unwrap().len(), 1);
    }

    struct AutoCred;
    #[async_trait::async_trait]
    impl ProductAdapter for AutoCred {
        fn id(&self) -> String { "auto".into() }
        fn accepts_empty_token(&self) -> bool { true }
        async fn query_sign_status(&self, token: &str) -> Result<SignStatus, AdapterError> {
            Ok(if token.is_empty() { SignStatus::NotSigned } else { SignStatus::SignedToday })
        }
        async fn sign_in(&self, _t: &str) -> Result<SignOutcome, AdapterError> {
            Ok(SignOutcome::Success("ok".into()))
        }
    }

    #[tokio::test]
    async fn empty_token_passes_for_auto_extract_adapters() {
        let (n, _rec) = RecordingNotifier::new();
        let store = store();
        let cfg = ProductConfig { product_id: "auto".into(), mode: Mode::Auto,
            time_of_day: "09:00".into(), token: None };
        let r = run_once(&AutoCred, &cfg, &store, &n, 0).await;
        assert!(matches!(r, RunResult::Success(_)));
    }

    #[test]
    fn should_retry_policy() {
        use crate::adapter::AdapterError;
        assert!(!should_retry(&AdapterError::AuthExpired, 0, 2));
        assert!(!should_retry(&AdapterError::SchemaChanged("x".into()), 0, 2));
        let mk_err = || -> reqwest::Error {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(async { reqwest::Client::new().get("http://127.0.0.1:1/x").send().await.unwrap_err() })
        };
        assert!(should_retry(&AdapterError::Http(mk_err()), 0, 2));
        assert!(!should_retry(&AdapterError::Http(mk_err()), 2, 2));
    }
}
