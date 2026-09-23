//! Trae 适配器：解密本机 TRAE 桌面登录态（AES-128-CBC 信封）+ 签到/积分接口。
//! 设计决策见 docs/recon/trae.md：绝不调用 ExchangeToken 主动刷新；
//! token 过期只提示「打开 TRAE 让它自己刷新」。

use std::path::{Path, PathBuf};

use aes::cipher::{block_padding::Pkcs7, BlockDecryptMut, KeyIvInit};
use base64::Engine;
use serde_json::Value;
use sha2::{Digest, Sha512};

use super::AdapterError;

const STORAGE_KEY: &str = "iCubeAuthInfo://icube.cloudide";
const STORAGE_VARIANTS: [&str; 3] = ["TRAE SOLO CN", "Trae CN", "Trae"];

/// 用户决策（2026-09-23）：过期不刷新，仅提示
pub const REFRESH_HINT: &str = "token 已过期，请打开 TRAE 让它自己刷新后重试";

/// TRAE 信封头（6 字节）
const HEADER: [u8; 6] = [116, 99, 5, 16, 0, 0];
const LEFT_SECRET: [u8; 64] = [
    82, 9, 106, 213, 48, 54, 165, 56, 191, 64, 163, 158, 129, 243, 215, 251, 124, 227, 57, 130,
    155, 47, 255, 135, 52, 142, 67, 68, 196, 222, 233, 203, 84, 123, 148, 50, 166, 194, 35, 61,
    238, 76, 149, 11, 66, 250, 195, 78, 8, 46, 161, 102, 40, 217, 36, 178, 118, 91, 162, 73, 109,
    139, 209, 37,
];
const RIGHT_SECRET: [u8; 64] = [
    31, 221, 168, 51, 136, 7, 199, 49, 177, 18, 16, 89, 39, 128, 236, 95, 96, 81, 127, 169, 25,
    181, 74, 13, 45, 229, 122, 159, 147, 201, 156, 239, 160, 224, 59, 77, 174, 42, 245, 176, 200,
    235, 187, 60, 131, 83, 153, 97, 23, 43, 4, 126, 186, 119, 214, 38, 225, 105, 20, 99, 85, 33,
    12, 125,
];

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

fn sha512(data: &[u8]) -> [u8; 64] {
    let mut h = Sha512::new();
    h.update(data);
    let out = h.finalize();
    let mut arr = [0u8; 64];
    arr.copy_from_slice(out.as_slice());
    arr
}

/// 解密 storage.json 里 iCubeAuthInfo 的 base64 信封 → payload JSON。
/// 信封 = HEADER(6) + randomKey(32) + AES-128-CBC( SHA512(payload)(64) ‖ payload )。
pub fn decrypt_auth_info(encoded: &str) -> Result<Value, AdapterError> {
    let envelope = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| AdapterError::SchemaChanged("TRAE 凭据信封不是合法 base64".into()))?;
    if envelope.len() <= 38 || envelope.get(0..6) != Some(&HEADER[..]) {
        return Err(AdapterError::SchemaChanged(
            "TRAE 凭据信封格式不受支持（可能已更换加密方式）".into(),
        ));
    }
    let random_key = &envelope[6..38];
    let mut secret = [0u8; 64];
    for i in 0..64 {
        secret[i] = LEFT_SECRET[i] ^ RIGHT_SECRET[i];
    }
    let mut derived_input = Vec::with_capacity(128);
    derived_input.extend_from_slice(&sha512(random_key));
    derived_input.extend_from_slice(&secret);
    let derived = sha512(&derived_input);
    let key = &derived[0..16];
    let iv = &derived[16..32];
    let mut buf = envelope[38..].to_vec();
    let plaintext = Aes128CbcDec::new(key.into(), iv.into())
        .decrypt_padded_mut::<Pkcs7>(&mut buf)
        .map_err(|_| AdapterError::SchemaChanged("TRAE 凭据信封解密失败".into()))?;
    if plaintext.len() < 64 {
        return Err(AdapterError::SchemaChanged("TRAE 凭据信封内容过短".into()));
    }
    let (expected, payload) = plaintext.split_at(64);
    if expected != sha512(payload).as_slice() {
        return Err(AdapterError::SchemaChanged("TRAE 凭据完整性校验失败".into()));
    }
    let json: Value = serde_json::from_slice(payload)
        .map_err(|_| AdapterError::SchemaChanged("TRAE 凭据 payload 不是 JSON".into()))?;
    Ok(json)
}

/// 本机 TRAE 登录态（jwt 为无前缀裸 token；user_id 用于设备派生 seed）
#[derive(Debug, Clone)]
pub struct TraeCred {
    pub jwt: String,
    pub user_id: String,
}

/// 宽松时间解析（RFC3339 / 常见 ISO）→ 毫秒时间戳
pub fn parse_time_ms(v: Option<&Value>) -> Option<i64> {
    let s = v?.as_str()?;
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.timestamp_millis());
    }
    for fmt in &["%Y-%m-%dT%H:%M:%SZ", "%Y-%m-%dT%H:%M:%S%.3fZ", "%Y-%m-%dT%H:%M:%S"] {
        if let Ok(ndt) = chrono::NaiveDateTime::parse_from_str(s, fmt) {
            return Some(ndt.and_utc().timestamp_millis());
        }
    }
    None
}

fn jwt_payload(token: &str) -> Option<Value> {
    let parts: Vec<&str> = token.trim().split('.').collect();
    if parts.len() < 2 {
        return None;
    }
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1].trim_end_matches('='))
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// JWT payload.exp → 毫秒（秒/毫秒/字符串宽松兼容）；解不出返回 None（不视为过期）
pub fn jwt_exp_ms(token: &str) -> Option<i64> {
    let raw = jwt_payload(token)?.get("exp").and_then(|v| {
        v.as_i64()
            .or_else(|| v.as_f64().map(|f| f as i64))
            .or_else(|| v.as_str().and_then(|s| s.parse::<i64>().ok()))
    })?;
    Some(if raw < 100_000_000_000 { raw * 1000 } else { raw })
}

/// JWT payload 的 data.id（回退 auth_id、sub）→ user_id
pub fn jwt_user_id(token: &str) -> Option<String> {
    let payload = jwt_payload(token)?;
    payload
        .get("data")
        .and_then(|d| d.get("id"))
        .and_then(|v| v.as_str().map(|s| s.to_string()).or_else(|| v.as_i64().map(|n| n.to_string())))
        .or_else(|| payload.get("auth_id").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .or_else(|| payload.get("sub").and_then(|v| v.as_str()).map(|s| s.to_string()))
}

/// "Cloud-IDE-JWT x"/"Bearer x"/裸 token → 裸 token
pub fn strip_prefix(token: &str) -> &str {
    let t = token.trim();
    t.strip_prefix("Cloud-IDE-JWT ")
        .or_else(|| t.strip_prefix("Bearer "))
        .unwrap_or(t)
        .trim()
}

/// authorization 头的完整值：缺前缀时补 `Cloud-IDE-JWT `
pub fn normalize_full(jwt: &str) -> String {
    let t = jwt.trim();
    if t.starts_with("Cloud-IDE-JWT ") || t.starts_with("Bearer ") {
        t.to_string()
    } else {
        format!("Cloud-IDE-JWT {t}")
    }
}

fn storage_paths(override_path: Option<&Path>) -> Vec<PathBuf> {
    if let Some(p) = override_path {
        return vec![p.to_path_buf()];
    }
    let Ok(appdata) = std::env::var("APPDATA") else { return vec![] };
    STORAGE_VARIANTS
        .iter()
        .map(|v| PathBuf::from(&appdata).join(v).join("User").join("globalStorage").join("storage.json"))
        .collect()
}

/// 读当前活动 TRAE 账号：按变体顺序取第一个存在且可解密的目录；
/// 解出即定（过期**不**回退下一目录）。now_ms 外传以便测试稳定。
pub fn read_local_credential_at(
    override_path: Option<&Path>,
    now_ms: i64,
) -> Result<TraeCred, AdapterError> {
    let mut last_err: Option<AdapterError> = None;
    for path in storage_paths(override_path) {
        let Ok(raw) = std::fs::read_to_string(&path) else { continue };
        let Ok(storage) = serde_json::from_str::<Value>(&raw) else { continue };
        let Some(encoded) = storage.get(STORAGE_KEY).and_then(|v| v.as_str()) else { continue };
        match decrypt_auth_info(encoded) {
            Ok(info) => {
                let jwt = strip_prefix(info.get("token").and_then(|v| v.as_str()).unwrap_or("")).to_string();
                if jwt.is_empty() {
                    last_err = Some(AdapterError::SchemaChanged("TRAE 凭据缺少 token".into()));
                    continue;
                }
                let user_id = info.get("userId").and_then(|v| v.as_str()).unwrap_or_default().to_string();
                let env_exp = parse_time_ms(info.get("expiredAt")).unwrap_or(0);
                let jwt_exp = jwt_exp_ms(&jwt).unwrap_or(i64::MAX);
                if env_exp <= now_ms || jwt_exp <= now_ms {
                    return Err(AdapterError::AuthExpiredMsg(REFRESH_HINT.into()));
                }
                return Ok(TraeCred { jwt, user_id });
            }
            Err(e) => last_err = Some(e),
        }
    }
    Err(last_err.unwrap_or_else(|| {
        AdapterError::AuthExpiredMsg("未找到 TRAE 登录信息，请打开 TRAE 登录后重试（也可在设置中粘贴 token）".into())
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 由 Temp/gen_trae_test_envelopes.mjs 以固定 embedded_key 加密的假凭证（不含真实 token）
    pub const ENVELOPE_VALID: &str = "dGMFEAAABwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwflMiwPUbmm/55iGLIVK+MVhwxJ6h+/yYAtI46lXl358O6PnsYIoS44h6mjpVUzuDDnc6QmqJygB6x3uX+jcwfaFNIFAJ2R26Z1iQyGPefD6AUxDsutUWn9LEWPlSAPFlTMkkr39Cx9gkx01BRoMJDjCudtbXG7AcNGeXPon62apJT9SnA5iIkGuWfYAwyZbhYWfGf3vNe3ERkb26MFdQ+nx/C+Yr7QC6TMaWDVxMxdJflKrCdrKRMZi5LfpZLt3dBtj37azklyIYjpDucs3KeDknmHfKy2dSTAHKezIk35Znl6Ofstjtn6W1kDYcw2dlRlS5brDgFsNeBQXiJoHZhGj/BBWb9cO+/JnDRKJ1pnn6YNRqoy7yvl9IkNs4lAetcgRpA3gQoJBx5KdOpcKSPe1G18TRl+9G6nawtnaXCx/g==";
    pub const ENVELOPE_EXPIRED: &str = "dGMFEAAACQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQl/9cOgu58jZV+HL8+M9HidVON3hupmB62m1LpDoVn+M/bUZL9iI6Lrc6hhrn9d8ivHZ7WOkMAPpcUaXtHooai2lxYB7/fvTzPrw5Ra1ABDOBkV9fmNgAzPngJ06t8PNr5CQiFEMtSWUhCH74aCeaLlBdgppKxUwYEV0HEHdKZOottbSz4oWhZcUVqBzKsb9gPHZ+2SrzX7FHIUiB1gVQrasLTLVRmCdGDRQcj2c6I6BUZF7E2L354obfDQzvuvlD8ikvjuf+X2GOQArzARXCwW6jEsvWRRKjrwQ80qSMzJqynnBPH9UpQ2NLNi+9NHon/8QEbQRiFIKzMmzX5dRQTJgsU6QbvQSqfYbTVPvdJwx/MsY0uNJ67/USuiZNH6Pwfzx+y9fLtOlt2NlfZeQtIxhwBnKL9AVe7R1wCrgaVgbA==";

    /// 固定"现在"=2026-09-23T00:00:00Z（ENVELOPE_VALID 到期于 2026-10-23，过期信封到期于 2026-09-22）
    pub fn now_ms() -> i64 {
        chrono::DateTime::parse_from_rfc3339("2026-09-23T00:00:00Z").unwrap().timestamp_millis()
    }

    pub fn write_storage(dir: &Path, envelope: &str) -> PathBuf {
        let p = dir.join("storage.json");
        std::fs::write(&p, format!("{{\"{STORAGE_KEY}\":\"{envelope}\"}}")).unwrap();
        p
    }

    #[test]
    fn decrypt_returns_payload_fields() {
        let info = decrypt_auth_info(ENVELOPE_VALID).unwrap();
        assert_eq!(info["token"], "TK-VALID");
        assert_eq!(info["userId"], "1234567890123456");
        assert_eq!(info["host"], "https://api.trae.cn");
    }

    #[test]
    fn decrypt_rejects_bad_header() {
        let envelope = base64::engine::general_purpose::STANDARD.encode([0u8; 80]);
        assert!(matches!(decrypt_auth_info(&envelope), Err(AdapterError::SchemaChanged(_))));
    }

    #[test]
    fn decrypt_rejects_non_base64() {
        assert!(matches!(decrypt_auth_info("!!!not base64!!!"), Err(AdapterError::SchemaChanged(_))));
    }

    #[test]
    fn read_local_accepts_valid_envelope_with_fixed_now() {
        let dir = tempfile::tempdir().unwrap();
        let storage = write_storage(dir.path(), ENVELOPE_VALID);
        let cred = read_local_credential_at(Some(&storage), now_ms()).unwrap();
        assert_eq!(cred.jwt, "TK-VALID");
        assert_eq!(cred.user_id, "1234567890123456");
    }

    #[test]
    fn read_local_expired_envelope_is_auth_expired_msg() {
        let dir = tempfile::tempdir().unwrap();
        let storage = write_storage(dir.path(), ENVELOPE_EXPIRED);
        let err = read_local_credential_at(Some(&storage), now_ms()).unwrap_err();
        assert!(matches!(&err, AdapterError::AuthExpiredMsg(m) if m.contains("请打开 TRAE")));
    }

    #[test]
    fn read_local_missing_file_is_auth_expired_with_hint() {
        let err = read_local_credential_at(Some(Path::new("X:/definitely/not/here/storage.json")), now_ms()).unwrap_err();
        assert!(matches!(&err, AdapterError::AuthExpiredMsg(m) if m.contains("未找到")));
    }

    #[test]
    fn read_local_unsupported_header_surfaces_schema_changed() {
        // 模拟 "Trae CN" 新版信封（头不匹配）：解密错误记为 last_err，最终上浮而非误报"未找到"
        let dir = tempfile::tempdir().unwrap();
        let bogus = base64::engine::general_purpose::STANDARD.encode([7u8; 80]);
        let storage = write_storage(dir.path(), &bogus);
        let err = read_local_credential_at(Some(&storage), now_ms()).unwrap_err();
        assert!(matches!(err, AdapterError::SchemaChanged(_)));
    }

    #[test]
    fn parse_time_accepts_rfc3339_and_naive() {
        assert!(parse_time_ms(Some(&Value::String("2026-10-23T06:53:44.733Z".into()))).unwrap() > 0);
        assert!(parse_time_ms(Some(&Value::String("not-a-date".into()))).is_none());
        assert!(parse_time_ms(None).is_none());
    }

    #[test]
    fn jwt_exp_accepts_seconds_millis_and_none_for_bare_token() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"data":{"id":"7"},"exp":2000000000}"#);
        assert_eq!(jwt_exp_ms(&format!("aa.{payload}.bb")), Some(2000000000 * 1000));
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"exp":2000000000000}"#);
        assert_eq!(jwt_exp_ms(&format!("aa.{payload}.bb")), Some(2000000000000));
        assert_eq!(jwt_exp_ms("TK-VALID"), None);
    }

    #[test]
    fn jwt_user_id_reads_data_id_with_fallbacks() {
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(r#"{"data":{"id":"1234567890123456"}}"#);
        assert_eq!(jwt_user_id(&format!("aa.{payload}.bb")).as_deref(), Some("1234567890123456"));
        let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"sub":"u9"}"#);
        assert_eq!(jwt_user_id(&format!("aa.{payload}.bb")).as_deref(), Some("u9"));
        assert_eq!(jwt_user_id("bare"), None);
    }

    #[test]
    fn strip_and_normalize_prefixes() {
        assert_eq!(strip_prefix("Cloud-IDE-JWT abc"), "abc");
        assert_eq!(strip_prefix("  Bearer abc "), "abc");
        assert_eq!(strip_prefix("abc"), "abc");
        assert_eq!(normalize_full("abc"), "Cloud-IDE-JWT abc");
        assert_eq!(normalize_full("Cloud-IDE-JWT abc"), "Cloud-IDE-JWT abc");
    }
}
