use crate::types::{
    Mode, ProductConfig, RunRow, Settings, TodaySummary,
    DEFAULT_RETRY_INTERVAL_MIN, DEFAULT_RETRY_TIMES, OUTCOME_RETRYABLE,
};
use rusqlite::{params, Connection};
use std::path::Path;
use std::sync::Mutex;

#[derive(Debug)]
pub struct StoreError(pub String);
impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { write!(f, "{}", self.0) }
}
impl std::error::Error for StoreError {}

pub struct Store { conn: Mutex<Connection> }

fn mode_to_str(m: Mode) -> &'static str {
    match m { Mode::Off => "off", Mode::Remind => "remind", Mode::Auto => "auto" }
}
fn mode_from_str(s: &str) -> Mode {
    match s { "off" => Mode::Off, "auto" => Mode::Auto, _ => Mode::Remind }
}

/// Qoder 的当日福利窗口每天北京时间 10:00 才开（docs/recon/qoder.md），默认值须落在窗口之后
fn default_time_of_day(product_id: &str) -> &'static str {
    if product_id == "qoder" { "10:05" } else { "09:00" }
}

impl Store {
    fn from_conn(conn: Connection) -> Result<Store, StoreError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS product_config(
                product_id TEXT PRIMARY KEY, mode TEXT NOT NULL,
                time_of_day TEXT NOT NULL, token TEXT,
                retry_times INTEGER NOT NULL DEFAULT 2,
                retry_interval_min INTEGER NOT NULL DEFAULT 5);
             CREATE TABLE IF NOT EXISTS sign_run(
                id INTEGER PRIMARY KEY AUTOINCREMENT, product_id TEXT NOT NULL,
                at INTEGER NOT NULL, outcome TEXT NOT NULL, detail TEXT NOT NULL);"
        ).map_err(|e| StoreError(e.to_string()))?;
        // 老库没有重试两列：ALTER 带默认值即等价于「沿用出厂策略」
        Self::ensure_retry_columns(&conn)?;
        // 早期版本给 qoder 也播种了 09:00，而它的领取窗口 10:00 才开 → 会把整天「预占」掉。
        // 只回捞仍是出厂默认（remind + 无 token）的行，用户自己改过的不动。
        conn.execute(
            "UPDATE product_config SET time_of_day='10:05'
             WHERE product_id='qoder' AND mode='remind' AND time_of_day='09:00' AND token IS NULL",
            [],
        ).map_err(|e| StoreError(e.to_string()))?;
        Ok(Store { conn: Mutex::new(conn) })
    }

    /// 老库没有重试两列时补上（ALTER + DEFAULT 等价于「沿用出厂策略」）
    fn ensure_retry_columns(conn: &Connection) -> Result<(), StoreError> {
        for (col, decl) in [("retry_times", "INTEGER NOT NULL DEFAULT 2"),
                            ("retry_interval_min", "INTEGER NOT NULL DEFAULT 5")] {
            let has: bool = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('product_config') WHERE name=?1",
                [col], |r| r.get(0)).map_err(|e| StoreError(e.to_string()))?;
            if !has {
                conn.execute_batch(&format!("ALTER TABLE product_config ADD COLUMN {col} {decl}"))
                    .map_err(|e| StoreError(e.to_string()))?;
            }
        }
        Ok(())
    }

    pub fn open(path: &Path) -> Result<Store, StoreError> {
        if let Some(d) = path.parent() { std::fs::create_dir_all(d).map_err(|e| StoreError(e.to_string()))?; }
        Self::from_conn(Connection::open(path).map_err(|e| StoreError(e.to_string()))?)
    }

    pub fn in_memory() -> Result<Store, StoreError> {
        Self::from_conn(Connection::open_in_memory().map_err(|e| StoreError(e.to_string()))?)
    }

    pub fn config(&self, product_id: &str) -> Result<ProductConfig, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT mode, time_of_day, token, retry_times, retry_interval_min
             FROM product_config WHERE product_id=?1")
            .map_err(|e| StoreError(e.to_string()))?;
        // 只有「查无此行」才播种默认值；真实 DB 错误必须上报，否则会静默覆盖用户配置
        match stmt.query_row(params![product_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?,
                r.get::<_, i64>(3)?, r.get::<_, i64>(4)?))
        }) {
            Ok(row) => return Ok(ProductConfig {
                product_id: product_id.into(), mode: mode_from_str(&row.0), time_of_day: row.1, token: row.2,
                retry_times: row.3.max(0) as u32, retry_interval_min: row.4.max(1) as u32,
            }),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => return Err(StoreError(e.to_string())),
        }
        let def = ProductConfig {
            product_id: product_id.into(),
            mode: Mode::Remind,
            time_of_day: default_time_of_day(product_id).into(),
            token: None,
            retry_times: DEFAULT_RETRY_TIMES,
            retry_interval_min: DEFAULT_RETRY_INTERVAL_MIN,
        };
        self.write_config(&conn, &def)?;
        Ok(def)
    }

    /// 界面读写的配置视图（不含 token）
    pub fn settings(&self, product_id: &str) -> Result<Settings, StoreError> {
        self.config(product_id).map(|c| (&c).into())
    }

    pub fn set_config(&self, cfg: &ProductConfig) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        self.write_config(&conn, cfg)
    }

    fn write_config(&self, conn: &std::sync::MutexGuard<'_, Connection>, cfg: &ProductConfig) -> Result<(), StoreError> {
        conn.execute(
            "INSERT INTO product_config(product_id, mode, time_of_day, token, retry_times, retry_interval_min)
             VALUES(?1,?2,?3,?4,?5,?6)
             ON CONFLICT(product_id) DO UPDATE SET mode=?2, time_of_day=?3, token=?4, retry_times=?5, retry_interval_min=?6",
            params![cfg.product_id, mode_to_str(cfg.mode), cfg.time_of_day, cfg.token,
                cfg.retry_times as i64, cfg.retry_interval_min as i64])
            .map_err(|e| StoreError(e.to_string()))?;
        Ok(())
    }

    /// 保存界面设置：**绝不触碰 token 列**（前端没有原值，回写等于清空）。
    pub fn update_settings(&self, s: &Settings) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO product_config(product_id, mode, time_of_day, token, retry_times, retry_interval_min)
             VALUES(?1,?2,?3,NULL,?4,?5)
             ON CONFLICT(product_id) DO UPDATE SET mode=?2, time_of_day=?3, retry_times=?4, retry_interval_min=?5",
            params![s.product_id, mode_to_str(s.mode), s.time_of_day,
                s.retry_times as i64, s.retry_interval_min as i64])
            .map_err(|e| StoreError(e.to_string()))?;
        Ok(())
    }

    pub fn record_run(&self, product_id: &str, outcome: &str, detail: &str) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute("INSERT INTO sign_run(product_id, at, outcome, detail) VALUES(?1,?2,?3,?4)",
            params![product_id, chrono::Utc::now().timestamp(), outcome, detail])
            .map_err(|e| StoreError(e.to_string()))?;
        Ok(())
    }

    pub fn last_success_at(&self, product_id: &str) -> Result<Option<i64>, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT MAX(at) FROM sign_run WHERE product_id=?1 AND outcome='success'",
            params![product_id], |r| r.get::<_, Option<i64>>(0))
            .map_err(|e| StoreError(e.to_string()))
    }

    /// 今日（`day_start` 之后）的执行摘要：最近一次结果 + 临时失败次数。
    pub fn today_summary(&self, product_id: &str, day_start: i64) -> Result<TodaySummary, StoreError> {
        let conn = self.conn.lock().unwrap();
        let last: Option<(i64, String)> = conn.query_row(
            "SELECT at, outcome FROM sign_run WHERE product_id=?1 AND at>=?2 ORDER BY at DESC, id DESC LIMIT 1",
            params![product_id, day_start],
            |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
        ).ok();
        let retry_failures = conn.query_row(
            "SELECT COUNT(*) FROM sign_run WHERE product_id=?1 AND at>=?2 AND outcome=?3",
            params![product_id, day_start, OUTCOME_RETRYABLE], |r| r.get::<_, i64>(0))
            .map_err(|e| StoreError(e.to_string()))?;
        Ok(TodaySummary {
            last_at: last.as_ref().map(|(at, _)| *at),
            last_outcome: last.map(|(_, o)| o),
            retry_failures: retry_failures.max(0) as u32,
        })
    }

    pub fn runs(&self, product_id: &str, limit: i64) -> Result<Vec<RunRow>, StoreError> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, product_id, at, outcome, detail FROM sign_run WHERE product_id=?1 ORDER BY at DESC, id DESC LIMIT ?2")
            .map_err(|e| StoreError(e.to_string()))?;
        let rows = stmt.query_map(params![product_id, limit], |r| {
            Ok(RunRow {
                id: r.get(0)?, product_id: r.get(1)?,
                at: chrono::DateTime::<chrono::Utc>::from_timestamp(r.get::<_, i64>(2)?, 0)
                    .map(|d| d.to_rfc3339()).unwrap_or_default(),
                outcome: r.get(3)?, detail: r.get(4)?,
            })
        }).map_err(|e| StoreError(e.to_string()))?;
        rows.collect::<Result<Vec<_>, _>>().map_err(|e| StoreError(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings(id: &str, mode: Mode, time: &str, retry_times: u32, retry_interval_min: u32) -> Settings {
        Settings { product_id: id.into(), mode, time_of_day: time.into(), retry_times, retry_interval_min }
    }

    #[test]
    fn default_config_is_remind_0900() {
        let s = Store::in_memory().unwrap();
        let cfg = s.config("workbuddy").unwrap();
        assert_eq!(cfg.mode, Mode::Remind);
        assert_eq!(cfg.time_of_day, "09:00");
        assert_eq!(cfg.token, None);
    }

    /// 读不出来的坏行不能当「没有配置」：吞掉错误会去播种默认值，把用户的数据覆盖掉
    #[test]
    fn unreadable_row_is_reported_not_seeded_over() {
        let s = Store::in_memory().unwrap();
        {
            let conn = s.conn.lock().unwrap();
            // 同名列、放宽 NOT NULL，好造出一行 mode 为 NULL 的坏数据：prepare 仍成功，取列才失败
            conn.execute_batch(
                "DROP TABLE product_config;
                 CREATE TABLE product_config(product_id TEXT PRIMARY KEY, mode TEXT, time_of_day TEXT,
                    token TEXT, retry_times INTEGER NOT NULL DEFAULT 2, retry_interval_min INTEGER NOT NULL DEFAULT 5);
                 INSERT INTO product_config(product_id, mode, time_of_day, token) VALUES('workbuddy',NULL,NULL,NULL);",
            ).unwrap();
        }
        let err = s.config("workbuddy").unwrap_err();
        assert!(!err.to_string().contains("UNIQUE"), "播种默认值覆盖了坏行：{err}");
    }

    /// Qoder 的当日福利窗口在北京 10:00 才开（docs/recon/qoder.md），09:00 跑只会看到昨日窗口
    #[test]
    fn qoder_default_time_is_after_its_window_opens() {
        let s = Store::in_memory().unwrap();
        assert_eq!(s.config("qoder").unwrap().time_of_day, "10:05");
        assert_eq!(s.config("trae").unwrap().time_of_day, "09:00");
    }

    #[test]
    fn untouched_qoder_row_is_bumped_when_reopening_db() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signdock.db");
        {
            let s = Store::open(&path).unwrap();
            s.set_config(&ProductConfig::base("qoder", Mode::Remind, "09:00", None)).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.config("qoder").unwrap().time_of_day, "10:05");
    }

    #[test]
    fn user_picked_time_is_never_bumped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signdock.db");
        {
            let s = Store::open(&path).unwrap();
            s.set_config(&ProductConfig::base("qoder", Mode::Auto, "09:00", None)).unwrap();
            s.set_config(&ProductConfig::base("workbuddy", Mode::Remind, "09:00", None)).unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.config("qoder").unwrap().time_of_day, "09:00");
        assert_eq!(s.config("workbuddy").unwrap().time_of_day, "09:00");
    }

    #[test]
    fn set_and_read_config_roundtrip() {
        let s = Store::in_memory().unwrap();
        s.set_config(&ProductConfig::base("trae", Mode::Auto, "08:30", Some("tok".into()))).unwrap();
        let cfg = s.config("trae").unwrap();
        assert_eq!(cfg.mode, Mode::Auto);
        assert_eq!(cfg.time_of_day, "08:30");
        assert_eq!(cfg.token.as_deref(), Some("tok"));
    }

    /// 界面只改设置项：token 由 Rust 侧保管，改设置绝不能把已存凭证清掉
    #[test]
    fn update_settings_preserves_stored_token() {
        let s = Store::in_memory().unwrap();
        s.set_config(&ProductConfig::base("trae", Mode::Auto, "09:00", Some("secret".into()))).unwrap();
        s.update_settings(&settings("trae", Mode::Remind, "08:00", 2, 5)).unwrap();
        let cfg = s.config("trae").unwrap();
        assert_eq!(cfg.mode, Mode::Remind);
        assert_eq!(cfg.time_of_day, "08:00");
        assert_eq!(cfg.token.as_deref(), Some("secret"));
    }

    /// 尚无任何设置时，改设置要能建行（token 留空，交给自动提取登录态）
    #[test]
    fn update_settings_creates_row_when_missing() {
        let s = Store::in_memory().unwrap();
        s.update_settings(&settings("qoder", Mode::Auto, "10:30", 0, 5)).unwrap();
        let cfg = s.config("qoder").unwrap();
        assert_eq!(cfg.mode, Mode::Auto);
        assert_eq!(cfg.time_of_day, "10:30");
        assert_eq!(cfg.token, None);
    }

    /// 重试两列同样存取一致（旧库升级后走默认值）
    #[test]
    fn retry_settings_roundtrip() {
        let s = Store::in_memory().unwrap();
        assert_eq!((s.config("trae").unwrap().retry_times, s.config("trae").unwrap().retry_interval_min),
            (DEFAULT_RETRY_TIMES, DEFAULT_RETRY_INTERVAL_MIN));
        s.update_settings(&settings("trae", Mode::Auto, "09:00", 5, 10)).unwrap();
        let got = s.settings("trae").unwrap();
        assert_eq!(got.retry_times, 5);
        assert_eq!(got.retry_interval_min, 10);
    }

    /// 旧版本建的库没有重试列：重开时必须补列并按出厂值读回
    #[test]
    fn old_db_gains_retry_columns_with_factory_defaults() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("signdock.db");
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE product_config(product_id TEXT PRIMARY KEY, mode TEXT NOT NULL,
                    time_of_day TEXT NOT NULL, token TEXT);
                 INSERT INTO product_config VALUES('trae','auto','08:30','tok');").unwrap();
        }
        let s = Store::open(&path).unwrap();
        let cfg = s.config("trae").unwrap();
        assert_eq!(cfg.token.as_deref(), Some("tok"));
        assert_eq!((cfg.retry_times, cfg.retry_interval_min), (DEFAULT_RETRY_TIMES, DEFAULT_RETRY_INTERVAL_MIN));
    }

    #[test]
    fn today_summary_counts_only_today_retryable() {
        let s = Store::in_memory().unwrap();
        s.record_run("trae", OUTCOME_RETRYABLE, "网络错误").unwrap();
        s.record_run("trae", OUTCOME_RETRYABLE, "网络错误").unwrap();
        s.record_run("trae", "success", "+5积分").unwrap();
        let now = chrono::Utc::now().timestamp();
        let sum = s.today_summary("trae", now - 60).unwrap();
        assert_eq!(sum.retry_failures, 2);
        assert_eq!(sum.last_outcome.as_deref(), Some("success"));
        // 昨天那段（今日零点之后不含）不计
        assert_eq!(s.today_summary("trae", now + 60).unwrap().last_at, None);
    }

    #[test]
    fn last_success_none_then_some() {
        let s = Store::in_memory().unwrap();
        assert_eq!(s.last_success_at("qoder").unwrap(), None);
        s.record_run("qoder", "success", "+10积分").unwrap();
        assert!(s.last_success_at("qoder").unwrap().is_some());
    }

    #[test]
    fn failed_run_is_not_success() {
        let s = Store::in_memory().unwrap();
        s.record_run("qoder", "failed", "网络错误").unwrap();
        assert_eq!(s.last_success_at("qoder").unwrap(), None);
    }

    #[test]
    fn runs_desc_with_limit() {
        let s = Store::in_memory().unwrap();
        s.record_run("trae", "success", "a").unwrap();
        s.record_run("trae", "reminded", "b").unwrap();
        let rows = s.runs("trae", 10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].outcome, "reminded"); // 新的在前
    }
}
