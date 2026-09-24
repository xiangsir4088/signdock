use crate::adapter::{AdapterError, ProductAdapter, SignOutcome, SignStatus};
use crate::notify::Notifier;
#[cfg(test)]
use crate::notify::RecordingNotifier;
use crate::store::Store;
use crate::types::{Mode, ProductConfig, OUTCOME_RETRYABLE};

#[derive(Debug, PartialEq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub enum RunResult {
    Skipped,
    AlreadySigned,
    Success(String),
    Reminded,
    NeedManual(String),
    Failed(String),
    /// 临时性网络错误：调度器会按产品的重试设置补试；落库 outcome 记为 "retryable"。
    FailedRetryable(String),
}

fn notify_both(n: &dyn Notifier, product_id: &str, body: &str) {
    n.notify(&format!("SignDock · {product_id}"), body);
}

/// 按产品的「执行中」标记。调度 tick、托盘「签到（全部）」、界面「立即执行一次」三路触发
/// 互不隶属，同一产品并发跑会互踩（重复签到、重复通知）。用原子标记而不是异步 Mutex：
/// 取不到就走开（下一轮再看），绝不排队，也就绝不把别的产品堵在一个挂死的请求后面。
pub struct RunLocks {
    slots: Vec<(&'static str, std::sync::Arc<std::sync::atomic::AtomicBool>)>,
}

/// 持有即代表该产品正在执行；Drop 复位标记。
pub struct RunGuard {
    flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        self.flag.store(false, std::sync::atomic::Ordering::Release);
    }
}

impl RunLocks {
    pub fn new(ids: &[&'static str]) -> Self {
        Self { slots: ids.iter()
            .map(|id| (*id, std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))))
            .collect() }
    }

    /// 取执行权；None=该产品正在执行（调用方本轮跳过，绝不排队）。
    /// 没有槽位的 id 拿到的是只属于自己的标记：永远可执行，不会因为查不到而被静默跳过。
    pub fn acquire(&self, id: &str) -> Option<RunGuard> {
        let flag = self.slots.iter().find(|(name, _)| *name == id)
            .map(|(_, f)| f.clone())
            .unwrap_or_else(|| std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)));
        flag.compare_exchange(false, true,
            std::sync::atomic::Ordering::AcqRel, std::sync::atomic::Ordering::Acquire)
            .ok().map(|_| RunGuard { flag })
    }
}

pub async fn run_once(
    adapter: &dyn ProductAdapter,
    cfg: &ProductConfig,
    store: &Store,
    notifier: &dyn Notifier,
) -> RunResult {
    let id = cfg.product_id.as_str();
    if cfg.mode == Mode::Off {
        return RunResult::Skipped;
    }
    let token = cfg.token.as_deref().unwrap_or("");
    if token.trim().is_empty() && !adapter.accepts_empty_token() {
        notify_both(notifier, id, "缺少凭证：请先登录该产品，SignDock 会自动读取本机登录态");
        store.record_run(id, "failed", "缺少凭证").ok();
        return RunResult::Failed("缺少凭证".into());
    }
    let status = match adapter.query_sign_status(token).await {
        Ok(s) => s,
        Err(e) => {
            let msg = e.to_string();
            notify_both(notifier, id, &format!("查询签到状态失败：{msg}"));
            // 仅 Http 临时错误可重试；AuthExpired/SchemaChanged 重试无意义。
            // 落库 outcome 也要区分：retryable 不算「今天处理过了」，调度器还会补试。
            store.record_run(id, retry_outcome(&e), &msg).ok();
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
            store.record_run(id, retry_outcome(&e), &msg).ok();
            // 同上：仅 Http 临时错误可重试。
            match e {
                AdapterError::Http(_) => RunResult::FailedRetryable(msg),
                _ => RunResult::Failed(msg),
            }
        }
    }
}

/// 落库 outcome：临时网络失败记 retryable（今日尚未「处理完」，调度器还会补试），
/// 凭证失效/接口变更/业务拒绝都记 failed（补试也改变不了结果）。
fn retry_outcome(err: &AdapterError) -> &'static str {
    match err {
        AdapterError::Http(_) => OUTCOME_RETRYABLE,
        _ => "failed",
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
        ProductConfig::base("workbuddy", mode, "09:00", Some("tok".into()))
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
        let r = run_once(&a, &cfg(Mode::Remind), &store, &n).await;
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
        let r = run_once(&a, &cfg(Mode::Auto), &store, &n).await;
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
        let r = run_once(&a, &cfg(Mode::Auto), &store, &n).await;
        assert_eq!(r, RunResult::AlreadySigned);
        assert_eq!(store.last_success_at("workbuddy").unwrap(), None); // 已签≠本次成功记录
    }

    #[tokio::test]
    async fn missing_token_fails_with_notification() {
        let (n, rec) = RecordingNotifier::new();
        let s = MockServer::start().await;
        let a = MockAdapter::new("workbuddy", s.uri());
        let cfg = ProductConfig::base("workbuddy", Mode::Auto, "09:00", None);
        let store = store();
        let r = run_once(&a, &cfg, &store, &n).await;
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
        let cfg = ProductConfig::base("auto", Mode::Auto, "09:00", None);
        let r = run_once(&AutoCred, &cfg, &store, &n).await;
        assert!(matches!(r, RunResult::Success(_)));
    }

    /// 临时网络失败落库为 retryable：它不等于「今天已经处理过」，调度器还要按设置补试。
    #[tokio::test]
    async fn http_failure_records_retryable_outcome() {
        let (n, _rec) = RecordingNotifier::new();
        let a = MockAdapter::new("workbuddy", "http://127.0.0.1:1".into());
        let store = store();
        let r = run_once(&a, &cfg(Mode::Auto), &store, &n).await;
        assert!(matches!(r, RunResult::FailedRetryable(_)));
        assert_eq!(store.runs("workbuddy", 10).unwrap()[0].outcome, OUTCOME_RETRYABLE);
    }

    /// 认证/格式类失败重试无意义，仍按终态 failed 落库
    #[tokio::test]
    async fn auth_failure_records_terminal_failed_outcome() {
        let (n, _rec) = RecordingNotifier::new();
        let s = MockServer::start().await;
        Mock::given(method("GET")).and(path("/sign/status"))
            .respond_with(ResponseTemplate::new(401)).mount(&s).await;
        let a = MockAdapter::new("workbuddy", s.uri());
        let store = store();
        let r = run_once(&a, &cfg(Mode::Auto), &store, &n).await;
        assert!(matches!(r, RunResult::Failed(_)));
        assert_eq!(store.runs("workbuddy", 10).unwrap()[0].outcome, "failed");
    }

    #[test]
    fn run_locks_are_per_product() {
        let locks = RunLocks::new(&["a", "b"]);
        let held = locks.acquire("a").expect("首次应拿到执行权");
        assert!(locks.acquire("a").is_none(), "同一产品不该并发执行");
        assert!(locks.acquire("b").is_some(), "别的产品不该被挡住");
        drop(held);
        assert!(locks.acquire("a").is_some(), "释放后应能再拿");
    }

    /// 没有槽位的产品（例如临时接进来的第四款）照常执行，不因缺锁而被静默跳过
    #[test]
    fn run_locks_let_unknown_product_through() {
        assert!(RunLocks::new(&["a"]).acquire("unknown").is_some());
    }
}
