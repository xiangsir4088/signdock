//! SignDock 自有凭据文件的封存层：明文只允许存在于内存，落盘一律 DPAPI（当前用户）。
//!
//! 为什么不用厂商那套 os_crypt：SignDock 的凭据不需要跨进程互通，DPAPI 当前用户作用域
//! 已经足够把 `.json` 里的 token 从"任何本机进程都能读"变成"只有这个 Windows 账号能读"。

use std::path::Path;

use base64::Engine;
use serde_json::Value;

/// 信封前缀。见到它就说明这份文件是封存件，不是历史上的明文 JSON。
pub const ENVELOPE_PREFIX: &str = "SDP1:";

#[derive(Debug, thiserror::Error)]
pub enum SecretError {
    #[error("凭据文件不存在")]
    NotFound,
    #[error("凭据文件已损坏")]
    Malformed,
    #[error("本机加解密不可用: {0}")]
    Unavailable(String),
}

/// 明文 → 可落盘的信封文本（前缀 + base64(DPAPI)）
pub fn seal(plain: &str) -> Result<String, SecretError> {
    let blob = dpapi_protect(plain.as_bytes())?;
    Ok(format!("{ENVELOPE_PREFIX}{}", encode_base64(&blob)))
}

/// 信封文本 → 明文
pub fn unseal(text: &str) -> Result<String, SecretError> {
    let body = text
        .strip_prefix(ENVELOPE_PREFIX)
        .ok_or(SecretError::Malformed)?;
    // 解不开的信封（写坏了 / 被从别的账号机器拷来）对用户是同一件事：重新登录一次。
    // 报告成"本机加解密不可用"只会把一个坏文件说成系统故障。
    let plain = dpapi_unprotect(&decode_base64(body)?).map_err(|_| SecretError::Malformed)?;
    String::from_utf8(plain).map_err(|_| SecretError::Malformed)
}

pub fn is_sealed(text: &str) -> bool {
    text.starts_with(ENVELOPE_PREFIX)
}

/// 读凭据 JSON。封存件与历史明文件都认 —— 调用方不必关心本机有没有迁移过。
pub fn read_json(path: &Path) -> Result<Value, SecretError> {
    let raw = read_text(path)?;
    let text = raw.trim_end();
    let json = if is_sealed(text) { unseal(text)? } else { raw };
    serde_json::from_str(&json).map_err(|_| SecretError::Malformed)
}

/// 永远写封存件。
pub fn write_json(path: &Path, value: &Value) -> Result<(), SecretError> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| SecretError::Unavailable(e.to_string()))?;
    }
    let envelope = seal(&value.to_string())?;
    std::fs::write(path, envelope).map_err(|e| SecretError::Unavailable(e.to_string()))
}

/// 文件还在、但还是明文 —— 调用方读成功后应立刻用 write_json 就地升级。
pub fn needs_upgrade(path: &Path) -> bool {
    read_text(path).map(|raw| !is_sealed(raw.trim_end())).unwrap_or(false)
}

fn read_text(path: &Path) -> Result<String, SecretError> {
    match std::fs::read_to_string(path) {
        Ok(s) => Ok(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(SecretError::NotFound),
        Err(_) => Err(SecretError::Malformed),
    }
}

fn encode_base64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn decode_base64(text: &str) -> Result<Vec<u8>, SecretError> {
    base64::engine::general_purpose::STANDARD
        .decode(text)
        .map_err(|_| SecretError::Malformed)
}

#[cfg(windows)]
mod os {
    use super::SecretError;
    use std::ffi::c_void;
    use windows::Win32::Foundation::{HLOCAL, LocalFree};
    use windows::Win32::Security::Cryptography::{
        CryptProtectData, CryptUnprotectData, CRYPT_INTEGER_BLOB,
    };

    fn take(output: CRYPT_INTEGER_BLOB) -> Vec<u8> {
        let v = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
        unsafe { let _ = LocalFree(Some(HLOCAL(output.pbData as *mut c_void))); }
        v
    }

    pub fn protect(plain: &[u8]) -> Result<Vec<u8>, SecretError> {
        let input =
            CRYPT_INTEGER_BLOB { cbData: plain.len() as u32, pbData: plain.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(&input, None, None, Some(std::ptr::null()), None, 0, &mut output)
                .map_err(|e| SecretError::Unavailable(e.to_string()))?;
        }
        Ok(take(output))
    }

    pub fn unprotect(blob: &[u8]) -> Result<Vec<u8>, SecretError> {
        let input =
            CRYPT_INTEGER_BLOB { cbData: blob.len() as u32, pbData: blob.as_ptr() as *mut u8 };
        let mut output = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptUnprotectData(&input, None, None, Some(std::ptr::null()), None, 0, &mut output)
                .map_err(|e| SecretError::Unavailable(e.to_string()))?;
        }
        Ok(take(output))
    }
}

#[cfg(not(windows))]
mod os {
    use super::SecretError;

    pub fn protect(_plain: &[u8]) -> Result<Vec<u8>, SecretError> {
        Err(SecretError::Unavailable("仅 Windows 支持本地凭据封存".into()))
    }

    pub fn unprotect(_blob: &[u8]) -> Result<Vec<u8>, SecretError> {
        Err(SecretError::Unavailable("仅 Windows 支持本地凭据封存".into()))
    }
}

fn dpapi_protect(plain: &[u8]) -> Result<Vec<u8>, SecretError> {
    os::protect(plain)
}

fn dpapi_unprotect(blob: &[u8]) -> Result<Vec<u8>, SecretError> {
    os::unprotect(blob)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::path::PathBuf;

    fn tmp(name: &str) -> PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!(
            "signdock-secret-{}-{}-{}",
            std::process::id(),
            name,
            uuid_like()
        ));
        p
    }

    fn uuid_like() -> u128 {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        use std::time::SystemTime;
        let mut h = DefaultHasher::new();
        SystemTime::now().hash(&mut h);
        std::thread::sleep(std::time::Duration::from_micros(1));
        let mut h2 = DefaultHasher::new();
        SystemTime::now().hash(&mut h2);
        ((h.finish() as u128) << 64) | h2.finish() as u128
    }

    #[test]
    fn sealed_text_does_not_look_like_plaintext() {
        let sealed = seal("{\"accessToken\":\"SECRET-XYZ\"}").unwrap();
        assert!(is_sealed(&sealed));
        assert!(!sealed.contains("SECRET-XYZ"), "信封里不该看得见明文");
        assert_eq!(unseal(&sealed).unwrap(), "{\"accessToken\":\"SECRET-XYZ\"}");
    }

    #[test]
    fn file_on_disk_hides_the_token() {
        let p = tmp("disk");
        write_json(&p, &json!({ "accessToken": "SECRET-XYZ" })).unwrap();
        let raw = std::fs::read_to_string(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert!(!raw.contains("SECRET-XYZ"), "落盘内容不该带得出 token");
        assert!(is_sealed(raw.trim_end()));
    }

    #[test]
    fn reads_back_what_was_written() {
        let p = tmp("roundtrip");
        write_json(&p, &json!({ "accessToken": "a.b.c", "uid": "u1" })).unwrap();
        let v = read_json(&p).unwrap();
        std::fs::remove_file(&p).ok();
        assert_eq!(v["accessToken"], "a.b.c");
        assert_eq!(v["uid"], "u1");
    }

    #[test]
    fn reads_legacy_plaintext_file_too() {
        let p = tmp("legacy");
        std::fs::write(&p, "{\"accessToken\":\"LEGACY-PLAIN\"}").unwrap();
        assert!(needs_upgrade(&p));
        let v = read_json(&p).unwrap();
        assert_eq!(v["accessToken"], "LEGACY-PLAIN");
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn sealed_file_does_not_need_upgrade() {
        let p = tmp("already-sealed");
        write_json(&p, &json!({ "accessToken": "x" })).unwrap();
        assert!(!needs_upgrade(&p));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn missing_file_reports_not_found() {
        let p = tmp("absent");
        std::fs::remove_file(&p).ok();
        assert!(matches!(read_json(&p), Err(SecretError::NotFound)));
        assert!(!needs_upgrade(&p));
    }

    #[test]
    fn corrupt_envelope_reports_malformed() {
        let p = tmp("corrupt");
        std::fs::write(&p, format!("{ENVELOPE_PREFIX}!!!not-base64!!!")).unwrap();
        assert!(matches!(read_json(&p), Err(SecretError::Malformed)));
        std::fs::remove_file(&p).ok();
    }

    #[test]
    fn envelope_of_garbage_payload_reports_malformed() {
        let p = tmp("garbage-payload");
        std::fs::write(&p, format!("{}{}", ENVELOPE_PREFIX, encode_base64(b"not json at all")))
            .unwrap();
        assert!(matches!(read_json(&p), Err(SecretError::Malformed)));
        std::fs::remove_file(&p).ok();
    }
}
