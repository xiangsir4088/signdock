#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Mode {
    Off,
    Remind,
    Auto,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProductConfig {
    pub product_id: String,
    pub mode: Mode,
    pub time_of_day: String,          // "HH:MM"
    pub token: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunRow {
    pub id: i64,
    pub product_id: String,
    pub at: String,                    // RFC3339
    pub outcome: String,               // "success" | "alreadySigned" | "reminded" | "needManual" | "failed"
    pub detail: String,
}
