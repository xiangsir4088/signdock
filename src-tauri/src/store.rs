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
        if let Some(row) = stmt.query_row(params![product_id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, Option<String>>(2)?))
        }).ok() {
            return Ok(ProductConfig { product_id: product_id.into(), mode: mode_from_str(&row.0), time_of_day: row.1, token: row.2 });
        }
        let def = ProductConfig { product_id: product_id.into(), mode: Mode::Remind, time_of_day: "09:00".into(), token: None };
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
