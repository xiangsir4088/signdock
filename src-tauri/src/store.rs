use crate::types::{Mode, ProductConfig, RunRow};
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
                time_of_day TEXT NOT NULL, token TEXT);
             CREATE TABLE IF NOT EXISTS sign_run(
                id INTEGER PRIMARY KEY AUTOINCREMENT, product_id TEXT NOT NULL,
                at INTEGER NOT NULL, outcome TEXT NOT NULL, detail TEXT NOT NULL);"
        ).map_err(|e| StoreError(e.to_string()))?;
        // 早期版本给 qoder 也播种了 09:00，而它的领取窗口 10:00 才开 → 会把整天「预占」掉。
        // 只回捞仍是出厂默认（remind + 无 token）的行，用户自己改过的不动。
        conn.execute(
            "UPDATE product_config SET time_of_day='10:05'
             WHERE product_id='qoder' AND mode='remind' AND time_of_day='09:00' AND token IS NULL",
            [],
        ).map_err(|e| StoreError(e.to_string()))?;
        Ok(Store { conn: Mutex::new(conn) })
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
            "SELECT mode, time_of_day, token FROM product_config WHERE product_id=?1")
            .map_err(|e| StoreError(e.to_string()))?;
        // 只有「查无此行」才播种默认值；真实 DB 错误必须上报，否则会静默覆盖用户配置
        match stmt.query_row(params![product_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
        }) {
            Ok(row) => return Ok(ProductConfig { product_id: product_id.into(), mode: mode_from_str(&row.0), time_of_day: row.1, token: row.2 }),
            Err(rusqlite::Error::QueryReturnedNoRows) => {}
            Err(e) => return Err(StoreError(e.to_string())),
        }
        let def = ProductConfig {
            product_id: product_id.into(),
            mode: Mode::Remind,
            time_of_day: default_time_of_day(product_id).into(),
            token: None,
        };
        conn.execute("INSERT INTO product_config(product_id, mode, time_of_day, token) VALUES(?1,?2,?3,?4)",
            params![def.product_id, mode_to_str(def.mode), def.time_of_day, def.token])
            .map_err(|e| StoreError(e.to_string()))?;
        Ok(def)
    }

    pub fn set_config(&self, cfg: &ProductConfig) -> Result<(), StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "INSERT INTO product_config(product_id, mode, time_of_day, token) VALUES(?1,?2,?3,?4)
             ON CONFLICT(product_id) DO UPDATE SET mode=?2, time_of_day=?3, token=?4",
            params![cfg.product_id, mode_to_str(cfg.mode), cfg.time_of_day, cfg.token])
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

    /// 任意结果最近一次执行时间（不分 outcome），供调度器做当日防重触发。
    pub fn last_run_at(&self, product_id: &str) -> Result<Option<i64>, StoreError> {
        let conn = self.conn.lock().unwrap();
        conn.query_row("SELECT MAX(at) FROM sign_run WHERE product_id=?1",
            params![product_id], |r| r.get::<_, Option<i64>>(0))
            .map_err(|e| StoreError(e.to_string()))
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
                 CREATE TABLE product_config(product_id TEXT PRIMARY KEY, mode TEXT, time_of_day TEXT, token TEXT);
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
            s.set_config(&ProductConfig {
                product_id: "qoder".into(), mode: Mode::Remind,
                time_of_day: "09:00".into(), token: None,
            })
            .unwrap();
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
            s.set_config(&ProductConfig {
                product_id: "qoder".into(), mode: Mode::Auto,
                time_of_day: "09:00".into(), token: None,
            })
            .unwrap();
            s.set_config(&ProductConfig {
                product_id: "workbuddy".into(), mode: Mode::Remind,
                time_of_day: "09:00".into(), token: None,
            })
            .unwrap();
        }
        let s = Store::open(&path).unwrap();
        assert_eq!(s.config("qoder").unwrap().time_of_day, "09:00");
        assert_eq!(s.config("workbuddy").unwrap().time_of_day, "09:00");
    }

    #[test]
    fn set_and_read_config_roundtrip() {
        let s = Store::in_memory().unwrap();
        s.set_config(&ProductConfig {
            product_id: "trae".into(), mode: Mode::Auto,
            time_of_day: "08:30".into(), token: Some("tok".into()),
        }).unwrap();
        let cfg = s.config("trae").unwrap();
        assert_eq!(cfg.mode, Mode::Auto);
        assert_eq!(cfg.time_of_day, "08:30");
        assert_eq!(cfg.token.as_deref(), Some("tok"));
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
