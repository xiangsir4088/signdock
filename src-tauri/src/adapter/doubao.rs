//! 豆包桌面版（www.doubao.com，每日任务中心签到 +200 POINT）。
//!
//! 与 qoder 同一路线：本机 Chromium os_crypt 登录态（DPAPI 数据密钥 + v10 cookie），
//! 差别在于豆包的会话在 **SQLite Cookies 库**里而不是独立凭据文件，且客户端运行时
//! 该库被独占锁（实测 err=32）——所以提取策略是「能拷贝就拷贝提取并回写缓存，
//! 拷不动（客户端开着）就回落 SignDock 自有缓存件」。接口契约实测见 docs/recon/doubao.md。

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aes_gcm::aead::Aead;
use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
use base64::Engine;
use serde_json::Value;

use super::miaoda::cookie_header;
use super::{transport_error, AdapterError, ProductAdapter, SignOutcome, SignStatus};

pub const PROD_BASE: &str = "https://www.doubao.com";
const CHECKIN_PATH: &str = "/alice/ug/campaign/landing/checkin";
/// 实测带尾斜杠（checkin 没有）；query 必带，否则业务 code 返回「系统错误」
const RECORDS_PATH: &str = "/alice/ug/campaign/quota/records/";
/// query 模板。`version_code` 不写死：按 `major*10000 + minor*100 + patch` 从本机客户端
/// 版本派生（实测 2.32.6→23206、2.32.5→23205、2.32.4→23204）。曾写死 20800，约两周
/// 就与客户端脱节；客户端周更，任何硬编码值都会腐坏（见 docs/recon/doubao.md）。
const QUERY_TPL: &str =
    "?version_code={vc}&language=zh&device_platform=web&doubao_device_platform=web&aid=497858&real_aid=497858";
const SCENE: &str = "pc_incentive_task_mvp_v3";
const UA_BASE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 ";
/// UA 尾缀同样从客户端版本派生（实测 2.32.6）；派生失败才用这个最后兜底
const FALLBACK_VERSION: &str = "2.32.6";

/// 实测成功的请求形态里的固定头；Agw-Js-Conv 缺失时数值字段会被序列化成对象
const AGW_JS_CONV: &str = "str";
const TASK_CODE: &str = "daily_signin";

const USER_DATA_SUBDIR: &str = "Doubao\\User Data";
const LOCAL_STATE_FILE: &str = "Local State";
const COOKIES_REL: &str = "Profile 1\\Network\\Cookies";
const OS_CRYPT_TAG: &[u8] = b"v10";
const NONCE_LEN: usize = 12;
const TAG_LEN: usize = 16;
const DPAPI_PREFIX: &[u8] = b"DPAPI";
/// Chromium WebKit 时间戳纪元偏移（1601-01-01 → Unix 秒）
const WEBKIT_EPOCH_US: i64 = 11_644_473_600_000_000;
/// 新版 Chromium 的 cookie 明文 = 32 字节域摘要前缀 + 真实值（实测剥离 32 字节即可）
const PLAIN_PREFIX_LEN: usize = 32;

/// 启动器：PE 版本资源里的 ProductVersion 就是客户端版本
const EXE_REL: &str = "Doubao\\Doubao.exe";
const PF_VAR: &[&str] = &["PROGRAMFILES", "PROGRAMFILES(X86)"];
/// 本机实测装盘位置不在 %PROGRAMFILES% 下（落在 D:\Program Files），所以还要枚举盘符
const EXTRA_PF: [&str; 5] =
    [r"C:\Program Files", r"D:\Program Files", r"E:\Program Files", r"F:\Program Files", r"G:\Program Files"];

pub const NOT_INSTALLED_HINT: &str =
    "本机没有豆包客户端数据（%LOCALAPPDATA%\\Doubao），请先安装并登录豆包";
pub const NOT_LOGGED_IN_HINT: &str = "豆包本机没有登录会话，请打开豆包客户端登录";
pub const EXTRACT_HINT: &str =
    "未能读取豆包本机登录态：请先登录豆包客户端，然后完全退出豆包（含托盘）再重试";
pub const REFRESH_HINT: &str =
    "豆包登录态已失效：请打开豆包客户端确认登录，然后完全退出豆包客户端，SignDock 会在下一轮自动重新提取";

#[derive(Debug, Clone, PartialEq)]
pub struct DoubaoCred {
    pub cookie_header: String,
    pub saved_at_ms: i64,
}

pub fn doubao_user_data_dir() -> Option<PathBuf> {
    let local = std::env::var("LOCALAPPDATA").ok()?;
    Some(Path::new(&local).join(USER_DATA_SUBDIR))
}

/// SignDock 自有的豆包 cookie 缓存件（DPAPI 封存，同秒哒封存件的存放方式）
pub fn cache_path() -> Option<PathBuf> {
    let appdata = std::env::var("APPDATA").ok()?;
    Some(
        Path::new(&appdata)
            .join("com.signdock.app")
            .join("doubao-cred.json"),
    )
}

/// Chromium os_crypt v10：`v10` + nonce(12) + AES-256-GCM(明文 ‖ tag(16))，无 AAD
pub fn decrypt_os_crypt(key: &[u8], blob: &[u8]) -> Result<Vec<u8>, AdapterError> {
    let cipher = Aes256Gcm::new_from_slice(key)
        .map_err(|_| AdapterError::SchemaChanged("豆包数据密钥长度不受支持".into()))?;
    if blob.len() < OS_CRYPT_TAG.len() + NONCE_LEN + TAG_LEN || &blob[..3] != OS_CRYPT_TAG {
        return Err(AdapterError::SchemaChanged(
            "豆包 cookie 加密格式不受支持（可能已更换加密方式）".into(),
        ));
    }
    let (nonce, ct) = blob[3..].split_at(NONCE_LEN);
    cipher
        .decrypt(Nonce::from_slice(nonce), ct)
        .map_err(|_| AdapterError::SchemaChanged("豆包 cookie 解密失败".into()))
}

/// DPAPI（当前用户）解包 Local State 里的 os_crypt 数据密钥
#[cfg(windows)]
fn dpapi_unwrap(blob: &[u8]) -> Result<Vec<u8>, AdapterError> {
    use std::ffi::c_void;
    use windows::Win32::Foundation::{LocalFree, HLOCAL};
    use windows::Win32::Security::Cryptography::{CryptUnprotectData, CRYPT_INTEGER_BLOB};

    let input = CRYPT_INTEGER_BLOB {
        cbData: blob.len() as u32,
        pbData: blob.as_ptr() as *mut u8,
    };
    let mut output = CRYPT_INTEGER_BLOB::default();
    let unwrapped = unsafe {
        CryptUnprotectData(
            &input,
            None,
            None,
            Some(std::ptr::null()),
            None,
            0,
            &mut output,
        )
    };
    if unwrapped.is_err() {
        return Err(AdapterError::AuthExpiredMsg(EXTRACT_HINT.into()));
    }
    let data =
        unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) }.to_vec();
    unsafe {
        let _ = LocalFree(Some(HLOCAL(output.pbData as *mut c_void)));
    }
    Ok(data)
}

#[cfg(not(windows))]
fn dpapi_unwrap(_blob: &[u8]) -> Result<Vec<u8>, AdapterError> {
    Err(AdapterError::SchemaChanged(
        "仅 Windows 支持读取豆包本机登录态".into(),
    ))
}

/// 读任意 PE 文件的版本资源，返回 `ProductVersion`。
/// 用 version.dll 只取 VS_VERSION_INFO 资源块，块内容自己解析——`VerQueryValueW`
/// 会按 codepage 标签算长度，实测豆包启动器的 Unicode 表标成 04b0，
/// ProductVersion "2.32.6" 被截成 "2.32"。
fn pe_product_version(exe: &Path) -> Option<String> {
    let block = version_info_block(exe)?;
    numeric_product_version(&block).or_else(|| product_version_string(&block))
}

/// 优先路径：VS_FIXEDFILEINFO.dwProductVersionMS/LS → "major.minor.build"。
/// 定长数值字段，没有字符串表那套 ANSI/Unicode 编码歧义。
/// VS_VERSION_INFO 头部宽度随 szKey 编码变（ANSI 16 字节 / Unicode 32 字节），
/// 所以不靠固定偏移，直接按 0xFEEF04BD 签名定位。
fn numeric_product_version(block: &[u8]) -> Option<String> {
    let fixed = find_bytes(block, 0, &[0xBD, 0x04, 0xEF, 0xFE])?;
    // dwSignature(+0) dwStrucVersion(+4) dwFileVersionMS(+8) LS(+12)
    // dwProductVersionMS(+16) LS(+20)
    let ms = u32_at(block, fixed + 16)?;
    let ls = u32_at(block, fixed + 20)?;
    let s = format!(
        "{}.{}.{}",
        (ms >> 16) as u32,
        ms as u16 as u32,
        (ls >> 16) as u32
    );
    if parse_version_parts(&s).is_some() {
        return Some(s);
    }
    None
}

/// 兜底路径：扫 VS_VERSION_INFO 块里的 StringFileInfo 子块取 ProductVersion 字符串
fn product_version_string(block: &[u8]) -> Option<String> {
    let unicode = block.get(7) == Some(&0u8);
    let sig = key_bytes("StringFileInfo", unicode);
    let mut pos = 0usize;
    while let Some(rel) = find_bytes(block, pos, &sig) {
        pos = rel + sig.len();
        let Some(table) = u32_at(block, rel + sig.len() + 4).map(|v| v as usize) else {
            continue;
        };
        if let Some(v) = string_entry(block, table, unicode, "ProductVersion") {
            if parse_version_parts(&v).is_some() {
                return Some(v);
            }
        }
    }
    None
}

fn string_entry(block: &[u8], table: usize, unicode: bool, want: &str) -> Option<String> {
    let klen = if unicode { 32 } else { 16 };
    let mut p = table + 6; // lang / charset / codepage
    while p + klen + 2 <= block.len() {
        let key = key_str(block, p, klen, unicode)?;
        let wlen = u16_at(block, p + klen)?;
        if key == want {
            let off = if wlen & 0x8000 != 0 {
                (wlen & 0x7FFF) as usize
            } else {
                p + klen + 2
            };
            return read_ver_string(block, off, unicode);
        }
        let chars = if wlen & 0x8000 != 0 {
            0
        } else {
            (wlen & 0x7FFF) as usize
        };
        let next = p + klen + 2 + if unicode { chars * 2 } else { chars };
        p = if next & 1 == 1 { next + 1 } else { next };
    }
    None
}

fn key_str(block: &[u8], off: usize, klen: usize, unicode: bool) -> Option<String> {
    let raw = block.get(off..off + klen)?;
    if unicode {
        let u: Vec<u16> = raw
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        return Some(String::from_utf16_lossy(&u));
    }
    let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
    Some(std::str::from_utf8(raw[..end].as_ref()).ok()?.to_string())
}

fn read_ver_string(block: &[u8], off: usize, unicode: bool) -> Option<String> {
    let s = if unicode {
        let mut u = Vec::new();
        let mut p = off;
        while p + 1 < block.len() {
            let c = u16_at(block, p)?;
            if c == 0 {
                break;
            }
            u.push(c);
            p += 2;
        }
        String::from_utf16_lossy(&u)
    } else {
        let slice = block.get(off..)?;
        let end = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
        std::str::from_utf8(&slice[..end]).ok()?.to_string()
    };
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(s.to_string())
}

fn key_bytes(key: &str, unicode: bool) -> Vec<u8> {
    if unicode {
        let mut v = Vec::with_capacity((key.len() + 1) * 2);
        for b in key.bytes() {
            v.push(b);
            v.push(0);
        }
        v.push(0);
        v.push(0);
        v
    } else {
        let mut v = key.as_bytes().to_vec();
        v.push(0);
        v
    }
}

fn find_bytes(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    let n = needle.len();
    if n == 0 || hay.len() < n {
        return None;
    }
    let from = from.min(hay.len() - n);
    for off in from..=hay.len() - n {
        if &hay[off..off + n] == needle {
            return Some(off);
        }
    }
    None
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// GetFileVersionInfoSizeW + GetFileVersionInfoW → 版本资源块（所有权归调用方）
fn version_info_block(exe: &Path) -> Option<Vec<u8>> {
    let name = exe.to_string_lossy().into_owned();
    let hname = windows::core::HSTRING::from(name.as_str());
    let sz = unsafe {
        windows::Win32::Storage::FileSystem::GetFileVersionInfoSizeW(&hname, None)
    };
    if sz == 0 || sz as usize > (1usize << 20) {
        return None;
    }
    let mut buf = vec![0u8; sz as usize];
    unsafe {
        windows::Win32::Storage::FileSystem::GetFileVersionInfoW(
            &hname,
            None,
            sz,
            buf.as_mut_ptr().cast(),
        )
        .ok()?;
    };
    Some(buf)
}

/// 定位本机豆包启动器：优先 %PROGRAMFILES% 系，再按盘符枚举 %Program Files%
fn doubao_exe_path() -> Option<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for v in PF_VAR {
        if let Ok(d) = std::env::var(v) {
            roots.push(PathBuf::from(d));
        }
    }
    for p in EXTRA_PF {
        roots.push(PathBuf::from(p));
    }
    for r in roots {
        let p = r.join(EXE_REL);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// 版本字符串 → `major*10000 + minor*100 + patch`。任一段 ≥1000 视为不支持（会串号）。
fn version_to_code(s: &str) -> Option<u64> {
    let v = parse_version_parts(s)?;
    Some(v.major as u64 * 10000 + v.minor as u64 * 100 + v.patch as u64)
}

fn parse_version_parts(s: &str) -> Option<VersionParts> {
    let parts: Vec<&str> = s.trim().split('.').collect();
    // 只接受 1~3 段：多于 3 段直接拒收，避免 2.32.6.1 被静默截断成 2.32.6
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let seg = |i: usize| -> Option<u32> {
        match parts.get(i) {
            // 段缺失按 0 处理（"2" / "2.31" 合法）
            None => Some(0),
            Some(p) => {
                let x = p.trim().parse::<u32>().ok()?;
                if x < 1000 {
                    Some(x)
                } else {
                    None
                }
            }
        }
    };
    let major = seg(0)?;
    let minor = seg(1)?;
    let patch = seg(2)?;
    Some(VersionParts { major, minor, patch })
}

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
struct VersionParts {
    major: u32,
    minor: u32,
    patch: u32,
}

static CLIENT_VERSION: OnceLock<String> = OnceLock::new();

/// 客户端版本：读失败（没装/装到非常规位置）回落到 FALLBACK_VERSION，
/// 绝不因为读不到就拒绝签到。读一次缓存进进程，客户端升级后下次启动自动跟上。
fn client_version() -> String {
    CLIENT_VERSION
        .get_or_init(|| {
            doubao_exe_path()
                .and_then(|p| pe_product_version(&p))
                .unwrap_or_else(|| FALLBACK_VERSION.to_string())
        })
        .clone()
}

fn query_for(version: &str) -> String {
    let vc = version_to_code(version)
        .unwrap_or_else(|| FALLBACK_VERSION.parse::<u64>().unwrap_or(0));
    QUERY_TPL.replace("{vc}", &vc.to_string())
}

fn user_agent_for(version: &str) -> String {
    format!("{UA_BASE}SamanthaDoubao/{version}")
}

fn query() -> String {
    query_for(&client_version())
}

fn user_agent() -> String {
    user_agent_for(&client_version())
}

fn os_crypt_key(user_data: &Path) -> Result<Vec<u8>, AdapterError> {
    let text = std::fs::read_to_string(user_data.join(LOCAL_STATE_FILE))
        .map_err(|_| AdapterError::AuthExpiredMsg(NOT_INSTALLED_HINT.into()))?;
    let json: Value = serde_json::from_str(&text)
        .map_err(|_| AdapterError::SchemaChanged("豆包 Local State 不是 JSON".into()))?;
    let encoded = json
        .get("os_crypt")
        .and_then(|o| o.get("encrypted_key"))
        .and_then(|x| x.as_str())
        .ok_or_else(|| AdapterError::SchemaChanged("豆包 Local State 缺少 os_crypt 密钥".into()))?;
    let blob = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| AdapterError::SchemaChanged("豆包数据密钥不是 base64".into()))?;
    if blob.get(..DPAPI_PREFIX.len()) != Some(DPAPI_PREFIX) {
        return Err(AdapterError::SchemaChanged("豆包数据密钥头不受支持".into()));
    }
    dpapi_unwrap(&blob[DPAPI_PREFIX.len()..])
}

/// 浏览器域匹配口径：发给 www.doubao.com 的 cookie 只能来自 .doubao.com 或 www.doubao.com。
/// 子域（如 soda.doubao.com）的 cookie 浏览器不会带上，混进来只会污染同名项。
const COOKIES_SQL: &str = "SELECT name, encrypted_value, expires_utc FROM cookies \
     WHERE host_key = '.doubao.com' OR host_key = 'www.doubao.com' \
        OR host_key LIKE '%.www.doubao.com'";

/// 拷贝 Cookies 库到临时目录后离线解密（客户端运行时原库被独占锁，拷贝即失败）。
/// 单行解密失败只跳过该行（老格式/新格式的杂项 cookie 不该拖垮整包）。
pub fn extract_cookie_pairs(
    user_data: &Path,
    now_s: i64,
) -> Result<Vec<(String, String)>, AdapterError> {
    let key = os_crypt_key(user_data)?;
    let src = user_data.join(COOKIES_REL);
    // 后缀带纳秒：并行调用互不踩同一个临时文件
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!(
        "signdock-doubao-ck-{}-{}.db",
        std::process::id(),
        unique
    ));
    // 锁失败在这里浮出：文案解释「客户端在运行」，上层据此回落缓存
    std::fs::copy(&src, &tmp)
        .map_err(|e| AdapterError::Http(format!("豆包 Cookies 被占用（客户端可能在运行）：{e}")))?;
    let result = (|| {
        let con =
            rusqlite::Connection::open_with_flags(&tmp, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .map_err(|e| AdapterError::SchemaChanged(format!("豆包 Cookies 打不开：{e}")))?;
        let mut stmt = con
            .prepare(COOKIES_SQL)
            .map_err(|e| AdapterError::SchemaChanged(format!("豆包 Cookies 结构变了：{e}")))?;
        let rows = stmt
            .query_map([], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, Vec<u8>>(1)?,
                    r.get::<_, i64>(2)?,
                ))
            })
            .map_err(|e| AdapterError::SchemaChanged(format!("豆包 Cookies 读不出：{e}")))?;
        let mut pairs = Vec::new();
        for row in rows {
            let (name, ev, exp) =
                row.map_err(|e| AdapterError::SchemaChanged(format!("豆包 Cookies 行损坏：{e}")))?;
            if ev.len() < 20 {
                continue;
            }
            // expires_utc=0 是会话 cookie（照发）；过期的浏览器不会带，我们也不带
            if exp > 0 && (exp - WEBKIT_EPOCH_US) / 1_000_000 < now_s {
                continue;
            }
            let plain = match decrypt_os_crypt(&key, &ev) {
                Ok(p) => p,
                Err(_) => continue,
            };
            let Some(value) = plain.get(PLAIN_PREFIX_LEN..) else {
                continue;
            };
            pairs.push((name, String::from_utf8_lossy(value).to_string()));
        }
        Ok(pairs)
    })();
    let _ = std::fs::remove_file(&tmp);
    result
}

/// 本机 Cookies → 请求头。没有会话主体（sessionid 族）说明用户已登出，
/// 这时缓存也必然是死的，直接报「去登录」而不是回落一份过期凭证。
pub fn cookie_cred_of(
    pairs: Vec<(String, String)>,
    saved_at_ms: i64,
) -> Result<DoubaoCred, AdapterError> {
    let has_session = pairs
        .iter()
        .any(|(n, _)| n == "sessionid" || n == "sessionid_ss");
    if !has_session {
        return Err(AdapterError::AuthExpiredMsg(NOT_LOGGED_IN_HINT.into()));
    }
    Ok(DoubaoCred {
        cookie_header: cookie_header(&pairs),
        saved_at_ms,
    })
}

/// cookie 头 → 账号 uid。multi_sids 形如 `<uid>:<sid>|<uid2>:<sid2>`，
/// **原始值是全 URL 编码的**（冒号 %3A、分段竖线 %7C，2026-10-15 实测），
/// 必须先整体解码再分段，否则整段切不开直接失败。只采信 sid 与会话主体
/// （sessionid/sessionid_ss 任一）一致的段，防同名残留行带偏；
/// sid_guard 新版首段已变成 sessionid、uid_tt 是 32 位哈希，都不可用作 uid 来源。
pub fn uid_from_cookie_header(header: &str) -> Option<String> {
    let value_of = |name: &str| header.split("; ").find_map(|p| p.strip_prefix(name));
    let sessions: Vec<&str> = ["sessionid=", "sessionid_ss="]
        .iter()
        .filter_map(|n| value_of(n))
        .collect();
    if sessions.is_empty() {
        return None;
    }
    let multi = value_of("multi_sids=")?;
    let decoded = multi
        .replace("%3A", ":")
        .replace("%3a", ":")
        .replace("%7C", "|")
        .replace("%7c", "|");
    for seg in decoded.split('|') {
        let Some((uid, sid)) = seg.split_once(':') else {
            continue;
        };
        if !uid.is_empty() && uid.chars().all(|c| c.is_ascii_digit()) && sessions.contains(&sid) {
            return Some(uid.to_string());
        }
    }
    None
}

fn read_cache(path: &Path) -> Result<DoubaoCred, AdapterError> {
    let v = crate::secret::read_json(path)
        .map_err(|_| AdapterError::AuthExpiredMsg(EXTRACT_HINT.into()))?;
    let cookie_header = v
        .get("cookieHeader")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if cookie_header.trim().is_empty() {
        return Err(AdapterError::AuthExpiredMsg(EXTRACT_HINT.into()));
    }
    Ok(DoubaoCred {
        cookie_header,
        saved_at_ms: v.get("savedAtMs").and_then(Value::as_i64).unwrap_or(0),
    })
}

fn write_cache(path: &Path, cred: &DoubaoCred) -> Result<(), AdapterError> {
    let v =
        serde_json::json!({ "cookieHeader": cred.cookie_header, "savedAtMs": cred.saved_at_ms });
    crate::secret::write_json(path, &v).map_err(|_| AdapterError::AuthExpired)
}

/// 业务体统一是 `{code, data, message?}`；code=0 才是成功。
pub fn unwrap_code(v: Value) -> Result<Value, AdapterError> {
    match v.get("code").and_then(Value::as_i64) {
        Some(0) => v
            .get("data")
            .filter(|d| !d.is_null())
            .cloned()
            .ok_or_else(|| AdapterError::SchemaChanged("豆包响应缺少 data".into())),
        Some(c) => {
            let msg = v
                .get("message")
                .or_else(|| v.get("msg"))
                .and_then(Value::as_str)
                .unwrap_or("");
            Err(AdapterError::Business(if msg.is_empty() {
                format!("豆包返回 code={c}")
            } else {
                msg.to_string()
            }))
        }
        None => Err(AdapterError::SchemaChanged("豆包响应缺少 code".into())),
    }
}

/// 北京时间今日 [0点, 明日0点)（固定 +08:00，系统时区不可信；签到按北京日滚动）
fn beijing_today(now_ms: i64) -> (i64, i64) {
    let cst = chrono::FixedOffset::east_opt(8 * 3_600).unwrap();
    let start = chrono::DateTime::<chrono::Utc>::from_timestamp_millis(now_ms)
        .map(|d| d.with_timezone(&cst).date_naive())
        .unwrap_or_default()
        .and_hms_opt(0, 0, 0)
        .and_then(|t| t.and_local_timezone(cst).single())
        .map(|d| d.timestamp_millis())
        .unwrap_or(now_ms);
    (start, start + 86_400_000)
}

/// 今日已签 = 流水里有 task_code=daily_signin 且发放时刻落在北京今日
pub fn signed_today(records: &[Value], today_start_ms: i64, tomorrow_start_ms: i64) -> bool {
    records.iter().any(|r| {
        r.get("task_code").and_then(Value::as_str) == Some(TASK_CODE)
            && matches!(
                r.get("create_time_ms").and_then(Value::as_i64),
                Some(t) if (today_start_ms..tomorrow_start_ms).contains(&t)
            )
    })
}

/// checkin 应答 → 结果。幂等标记是 `idempotent`（实测：重复 POST 返回 idempotent:true）
pub fn outcome_of(data: &Value) -> SignOutcome {
    if data.get("checked_in").and_then(Value::as_bool) != Some(true) {
        return SignOutcome::Failed("服务端未确认签到成功".into());
    }
    if data.get("idempotent").and_then(Value::as_bool) == Some(true) {
        return SignOutcome::AlreadySigned;
    }
    let amount = data
        .get("reward_amount")
        .and_then(Value::as_f64)
        .unwrap_or(0.0);
    SignOutcome::Success(format!("+{amount:.0} 额度"))
}

pub struct DoubaoAdapter {
    base_url: String,
    data_dir: Option<PathBuf>,
    cache: Option<PathBuf>,
}

impl Default for DoubaoAdapter {
    fn default() -> Self {
        Self {
            base_url: PROD_BASE.into(),
            data_dir: doubao_user_data_dir(),
            cache: cache_path(),
        }
    }
}

impl DoubaoAdapter {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with_base_url(mut self, base: impl Into<String>) -> Self {
        self.base_url = base.into();
        self
    }
    pub fn with_data_dir(mut self, dir: PathBuf) -> Self {
        self.data_dir = Some(dir);
        self
    }
    pub fn with_cache_path(mut self, path: PathBuf) -> Self {
        self.cache = Some(path);
        self
    }

    /// 凭证来源：先试本机 Cookies（最鲜活，成功即回写缓存），
    /// 技术性失败（客户端开着把库锁住）回落缓存件。明确「未登录」不回落：缓存也必然是死的。
    fn resolve(&self) -> Result<DoubaoCred, AdapterError> {
        let now_s = chrono::Utc::now().timestamp();
        let now_ms = chrono::Utc::now().timestamp_millis();
        match self.data_dir.as_deref() {
            Some(dir) => match extract_cookie_pairs(dir, now_s) {
                Ok(pairs) => {
                    let cred = cookie_cred_of(pairs, now_ms)?;
                    if let Some(p) = self.cache.as_ref() {
                        let _ = write_cache(p, &cred);
                    }
                    Ok(cred)
                }
                Err(e) => {
                    if let Some(p) = self.cache.as_ref() {
                        if let Ok(c) = read_cache(p) {
                            return Ok(c);
                        }
                    }
                    Err(e)
                }
            },
            None => Err(AdapterError::AuthExpiredMsg(NOT_INSTALLED_HINT.into())),
        }
    }

    async fn call(&self, path: &str, body: Value) -> Result<Value, AdapterError> {
        let cred = self.resolve()?;
        let url = format!("{}{}{}", self.base_url, path, query());
        let resp = super::http_client()
            .post(&url)
            .header(reqwest::header::COOKIE, &cred.cookie_header)
            .header(reqwest::header::USER_AGENT, user_agent())
            .header(reqwest::header::ACCEPT, "application/json, text/plain, */*")
            .header("agw-js-conv", AGW_JS_CONV)
            .header(reqwest::header::ORIGIN, PROD_BASE)
            .header(
                reqwest::header::REFERER,
                format!("{PROD_BASE}/chat/task-center"),
            )
            .json(&body)
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await?;
        let status = resp.status();
        if matches!(status.as_u16(), 401 | 403) {
            return Err(AdapterError::AuthExpiredMsg(REFRESH_HINT.into()));
        }
        if let Some(e) = transport_error(status) {
            return Err(e);
        }
        let v: Value = resp.json().await.map_err(|_| {
            AdapterError::SchemaChanged(format!("豆包返回不是 JSON（HTTP {}）", status.as_u16()))
        })?;
        unwrap_code(v)
    }
}

#[async_trait::async_trait]
impl ProductAdapter for DoubaoAdapter {
    fn id(&self) -> String {
        "doubao".into()
    }

    async fn query_sign_status(&self) -> Result<SignStatus, AdapterError> {
        let data = self.call(RECORDS_PATH, serde_json::json!({})).await?;
        let records = data
            .get("records")
            .and_then(Value::as_array)
            .ok_or_else(|| AdapterError::SchemaChanged("豆包流水响应缺少 records".into()))?;
        let (today_start, tomorrow_start) = beijing_today(chrono::Utc::now().timestamp_millis());
        Ok(if signed_today(records, today_start, tomorrow_start) {
            SignStatus::SignedToday
        } else {
            SignStatus::NotSigned
        })
    }

    async fn sign_in(&self) -> Result<SignOutcome, AdapterError> {
        let data = self
            .call(CHECKIN_PATH, serde_json::json!({ "scene": SCENE }))
            .await?;
        Ok(outcome_of(&data))
    }

    /// 流水接口里没有余额字段，bundle 也无 balance 类路径（见 recon 未决问题）；
    /// credits_snapshot 沿用 trait 默认实现报「暂不支持积分查询」，等找到接口再补。
    ///
    /// 登录账号提示以实际用于签到的 cookie 为准（uid_from_cookie_header）。不直接信
    /// public_config：2026-10-15 重装实测，新版客户端把顶层 user_id 挪进了
    /// text_picker.current_user，且那里残留的是切号前的旧账号。cookie 解析不出 uid 时
    /// 退回 public_config 兼容新旧两处位置，仍拿不到就给中性提示，不误报「接口可能已变更」。
    async fn account_label(&self) -> Result<String, AdapterError> {
        let cred = self.resolve()?;
        if let Some(uid) = uid_from_cookie_header(&cred.cookie_header) {
            return Ok(format!("豆包账号 {uid}"));
        }
        let cfg = std::env::var("APPDATA")
            .ok()
            .map(|a| Path::new(&a).join("Doubao").join("public_config.json"));
        if let Some(json) = cfg
            .and_then(|p| std::fs::read_to_string(p).ok())
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            let uid = json
                .get("user_id")
                .or_else(|| json.pointer("/text_picker/current_user/user_id"))
                .and_then(|x| {
                    x.as_str()
                        .map(str::to_string)
                        .or_else(|| x.as_i64().map(|n| n.to_string()))
                })
                .filter(|s| !s.is_empty());
            if let Some(uid) = uid {
                return Ok(format!("豆包账号 {uid}"));
            }
        }
        Ok("豆包账号未知（不影响签到）".into())
    }
}

#[cfg(test)]
mod tests {
    use aes_gcm::aead::Aead;
    use aes_gcm::{Aes256Gcm, KeyInit, Nonce};
    use serde_json::{json, Value};
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{
        beijing_today, client_version, cookie_cred_of, decrypt_os_crypt, doubao_exe_path,
        extract_cookie_pairs, outcome_of, parse_version_parts, pe_product_version, query_for,
        signed_today, unwrap_code, user_agent_for, version_to_code, DoubaoAdapter, COOKIES_REL,
        DPAPI_PREFIX, LOCAL_STATE_FILE, OS_CRYPT_TAG, PLAIN_PREFIX_LEN,
    };
    use crate::adapter::{AdapterError, ProductAdapter, SignOutcome, SignStatus};
    use base64::Engine as _;

    fn gcm_encrypt(key: &[u8; 32], nonce: &[u8; 12], plain: &[u8]) -> Vec<u8> {
        Aes256Gcm::new_from_slice(key)
            .unwrap()
            .encrypt(Nonce::from_slice(nonce), plain)
            .unwrap()
    }

    #[test]
    fn decrypt_os_crypt_reverses_v10_framing_and_strips_domain_prefix() {
        let key = [7u8; 32];
        let nonce = [3u8; 12];
        // 新版 Chromium：明文 = 32 字节域摘要 + 真实值
        let plain = [&[0xAB; 32][..], b"real-cookie-value"].concat();
        let mut blob = OS_CRYPT_TAG.to_vec();
        blob.extend_from_slice(&nonce);
        blob.extend(gcm_encrypt(&key, &nonce, &plain));
        assert_eq!(decrypt_os_crypt(&key, &blob).unwrap(), plain.to_vec());
    }

    #[test]
    fn decrypt_os_crypt_rejects_foreign_framing() {
        let key = [7u8; 32];
        assert!(decrypt_os_crypt(&key, b"v11abcdefghijklmnop").is_err());
        assert!(decrypt_os_crypt(&key, b"v10short").is_err());
        let mut tampered = b"v10".to_vec();
        let nonce = [9u8; 12];
        tampered.extend_from_slice(&nonce);
        let mut ct = gcm_encrypt(&key, &nonce, b"payload");
        let last = ct.len() - 1;
        ct[last] ^= 0xFF;
        tampered.extend(&ct);
        assert!(decrypt_os_crypt(&key, &tampered).is_err());
    }

    fn cookie_row(name: &str, value: &str, expires_utc: i64) -> (String, Vec<u8>, i64, String) {
        let key = [7u8; 32];
        let nonce = [5u8; 12];
        let plain = [&[0xCD; PLAIN_PREFIX_LEN][..], value.as_bytes()].concat();
        let mut blob = OS_CRYPT_TAG.to_vec();
        blob.extend_from_slice(&nonce);
        blob.extend(gcm_encrypt(&key, &nonce, &plain));
        (name.into(), blob, expires_utc, ".doubao.com".into())
    }

    /// 「本机装过并登录了豆包」的测试夹具：Local State 放 DPAPI 包好的数据密钥，
    /// Cookies 库放 v10 密文。DPAPI 是当前用户的密钥，只有 Windows 造得出来。
    #[cfg(windows)]
    struct LocalLogin(tempfile::TempDir);

    #[cfg(windows)]
    impl LocalLogin {
        fn with_cookies(rows: Vec<(String, Vec<u8>, i64, String)>) -> Self {
            let dir = tempfile::tempdir().unwrap();
            let key = [7u8; 32];
            let encoded = base64::engine::general_purpose::STANDARD
                .encode([DPAPI_PREFIX, dpapi_protect(&key).as_slice()].concat());
            std::fs::write(
                dir.path().join(LOCAL_STATE_FILE),
                json!({"os_crypt": {"encrypted_key": encoded}}).to_string(),
            )
            .unwrap();
            let db = dir.path().join(COOKIES_REL);
            std::fs::create_dir_all(db.parent().unwrap()).unwrap();
            let con = rusqlite::Connection::open(&db).unwrap();
            con.execute_batch(
                "CREATE TABLE cookies(name TEXT, encrypted_value BLOB, host_key TEXT, expires_utc INTEGER);",
            )
            .unwrap();
            for (name, blob, exp, host) in rows {
                con.execute(
                    "INSERT INTO cookies VALUES(?1, ?2, ?3, ?4)",
                    rusqlite::params![name, blob, host, exp],
                )
                .unwrap();
            }
            Self(dir)
        }
        fn with_session() -> Self {
            // 远未来过期：2020 WebKit 时间戳之外随便给个大数
            Self::with_cookies(vec![
                cookie_row("sessionid", "SESS-1", 0),
                cookie_row("ttwid", "TT-1", 0),
            ])
        }
        fn adapter(&self, s: &MockServer) -> DoubaoAdapter {
            DoubaoAdapter::new()
                .with_base_url(s.uri())
                .with_data_dir(self.0.path().to_path_buf())
                .with_cache_path(self.0.path().join("cache.json"))
        }
    }

    #[cfg(windows)]
    fn dpapi_protect(data: &[u8]) -> Vec<u8> {
        use std::ffi::c_void;
        use windows::Win32::Foundation::{LocalFree, HLOCAL};
        use windows::Win32::Security::Cryptography::{CryptProtectData, CRYPT_INTEGER_BLOB};
        let input = CRYPT_INTEGER_BLOB {
            cbData: data.len() as u32,
            pbData: data.as_ptr() as *mut u8,
        };
        let mut out = CRYPT_INTEGER_BLOB::default();
        unsafe {
            CryptProtectData(
                &input,
                None,
                None,
                Some(std::ptr::null()),
                None,
                0,
                &mut out,
            )
            .expect("DPAPI 加密失败");
        }
        let v = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) }.to_vec();
        unsafe {
            let _ = LocalFree(Some(HLOCAL(out.pbData as *mut c_void)));
        }
        v
    }

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    fn now_s() -> i64 {
        chrono::Utc::now().timestamp()
    }

    #[test]
    fn cookie_cred_requires_a_session_cookie() {
        let e = cookie_cred_of(vec![("ttwid".into(), "t".into())], 1).unwrap_err();
        assert!(
            matches!(e, AdapterError::AuthExpiredMsg(_)),
            "无会话主体应报未登录: {e:?}"
        );
        let cred = cookie_cred_of(
            vec![
                ("ttwid".into(), "t".into()),
                ("sessionid".into(), "s".into()),
            ],
            1,
        )
        .unwrap();
        assert_eq!(cred.cookie_header, "ttwid=t; sessionid=s");
    }

    #[test]
    fn unwrap_code_maps_business_error_and_missing_code() {
        let data = unwrap_code(json!({"code": 0, "data": {"checked_in": true}})).unwrap();
        assert_eq!(data["checked_in"], true);
        let e = unwrap_code(json!({"code": 1100, "message": "系统错误"})).unwrap_err();
        assert!(
            matches!(&e, AdapterError::Business(m) if m == "系统错误"),
            "{e:?}"
        );
        let e = unwrap_code(json!({"code": 5})).unwrap_err();
        assert!(
            matches!(&e, AdapterError::Business(m) if m.contains("code=5")),
            "{e:?}"
        );
        let e = unwrap_code(json!({"data": {}})).unwrap_err();
        assert!(
            matches!(e, AdapterError::SchemaChanged(_)),
            "缺 code 应报接口变更: {e:?}"
        );
    }

    #[test]
    fn outcome_reports_amount_and_idempotent_replay() {
        assert_eq!(
            outcome_of(&json!({"checked_in": true, "idempotent": false, "reward_amount": 200})),
            SignOutcome::Success("+200 额度".into())
        );
        assert_eq!(
            outcome_of(&json!({"checked_in": true, "idempotent": true, "reward_amount": 200})),
            SignOutcome::AlreadySigned
        );
        assert_eq!(
            outcome_of(&json!({"checked_in": false})),
            SignOutcome::Failed("服务端未确认签到成功".into())
        );
    }

    fn record(task_ms: i64) -> Value {
        json!({"task_code": "daily_signin", "task_name": "每日签到",
               "amount": "200", "unit": "POINT", "create_time_ms": task_ms,
               "record_id": "r1"})
    }

    #[test]
    fn signed_today_requires_task_code_inside_beijing_day() {
        let now_ms = 1_791_721_229_000i64; // 2026-10-11 北京时间午后（实测样本）
        let (start, tomorrow) = beijing_today(now_ms);
        assert!(signed_today(&[record(start + 3_600_000)], start, tomorrow));
        assert!(
            !signed_today(&[record(start - 1)], start, tomorrow),
            "昨天那条不算"
        );
        assert!(
            !signed_today(&[record(now_ms + 86_400_000)], start, tomorrow),
            "明天的不算"
        );
        // 其它任务的流水不能冒充签到
        let mut other = record(start + 60_000);
        other["task_code"] = json!("other_task");
        assert!(!signed_today(&[other], start, tomorrow));
    }

    #[test]
    fn beijing_day_window_is_fixed_at_utc8() {
        // 2026-10-11 08:00 UTC = 北京 16:00 → 当日 0 点 = 北京 2026-10-11 00:00 = 前日 16:00 UTC
        let (start, _) = beijing_today(1_791_705_600_000);
        assert_eq!(start, 1_791_648_000_000);
    }

    #[cfg(windows)]
    #[test]
    fn extract_reads_session_cookies_and_skips_expired_and_foreign_hosts() {
        let far_future = (now_s() + 86_400 * 30) * 1_000_000 + 11_644_473_600_000_000;
        let expired = (now_s() - 86_400) * 1_000_000 + 11_644_473_600_000_000;
        let mut foreign = cookie_row("promo", "x", far_future);
        foreign.3 = "soda.doubao.com".into(); // 子域 cookie 浏览器不会带给 www.doubao.com
        let dir = LocalLogin::with_cookies(vec![
            cookie_row("sessionid", "SESS", far_future),
            cookie_row("msToken", "TOK", expired), // 已过期
            foreign,
        ]);
        let pairs = extract_cookie_pairs(dir.0.path(), now_s()).unwrap();
        let sess = pairs
            .iter()
            .find(|(n, _)| n == "sessionid")
            .expect("sessionid 应解出");
        assert_eq!(sess.1, "SESS");
        assert!(
            !pairs.iter().any(|(n, _)| n == "msToken"),
            "过期 cookie 不能带"
        );
        assert!(
            !pairs.iter().any(|(n, _)| n == "promo"),
            "子域 cookie 不能带"
        );
    }

    #[test]
    fn version_code_encodes_major_minor_patch() {
        assert_eq!(version_to_code("2.32.6"), Some(23206));
        assert_eq!(version_to_code("2.32.5"), Some(23205));
        assert_eq!(version_to_code("2.31.4"), Some(23104));
        assert_eq!(version_to_code("2.3.0"), Some(20300));
        assert_eq!(version_to_code("2"), Some(20000));
        assert_eq!(version_to_code("2.31"), Some(23100));
    }

    #[test]
    fn version_code_rejects_overflow_and_junk() {
        // 段值 ≥1000 会串号（2.32.6.1 与 2.326.1 会算出同一个 code），必须拒收
        assert_eq!(version_to_code("2.32.6.1"), None);
        assert_eq!(version_to_code("147.0.7727.149"), None);
        assert_eq!(version_to_code("1.2.1000"), None);
        assert_eq!(version_to_code(""), None);
        assert_eq!(version_to_code("v2.32.6"), None);
        assert_eq!(version_to_code("2.32.x"), None);
    }

    #[test]
    fn query_and_user_agent_are_derived_not_hardcoded() {
        assert_eq!(
            query_for("2.32.6"),
            "?version_code=23206&language=zh&device_platform=web&doubao_device_platform=web&aid=497858&real_aid=497858"
        );
        assert_eq!(
            user_agent_for("2.32.6"),
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 SamanthaDoubao/2.32.6"
        );
        // 派生失败也不该发出空 version_code
        assert!(query_for("garbage").contains("version_code="));
    }

    #[cfg(windows)]
    #[test]
    fn client_version_always_yields_a_valid_semver() {
        let v = client_version();
        assert!(
            parse_version_parts(&v).is_some(),
            "客户端版本应能解析为 semver，实际 {v:?}"
        );
        assert!(version_to_code(&v).is_some());
    }

    /// 本机装了豆包就必须真的从启动器读出版本，而不是静默落回写死的兜底值
    #[cfg(windows)]
    #[test]
    fn client_version_comes_from_the_installed_exe_when_present() {
        let Some(exe) = doubao_exe_path() else { return }; // 没装就跳过
        let Some(disk) = pe_product_version(&exe) else {
            panic!("启动器版本资源应可读: {}", exe.display());
        };
        assert_eq!(
            client_version(),
            disk,
            "客户端版本应来自启动器 PE 资源，而不是 FALLBACK_VERSION"
        );
        assert!(
            version_to_code(&disk).is_some(),
            "读到的版本号必须能派生出 version_code: {disk}"
        );
    }

    async fn mount_records(s: &MockServer, body: Value) {
        // 只匹配固定契约参数；version_code 由本机客户端版本派生，值不写死
        Mock::given(method("POST"))
            .and(path("/alice/ug/campaign/quota/records/"))
            .and(query_param("language", "zh"))
            .and(query_param("device_platform", "web"))
            .and(query_param("aid", "497858"))
            .and(query_param("real_aid", "497858"))
            .and(header("agw-js-conv", "str"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(s)
            .await;
    }

    #[cfg(windows)]
    #[test]
    fn query_status_sends_local_cookies_with_required_query_and_headers() {
        let s = rt().block_on(MockServer::start());
        let now_ms = chrono::Utc::now().timestamp_millis();
        let (start, _) = beijing_today(now_ms);
        rt().block_on(mount_records(
            &s,
            json!({"code": 0, "data": {"has_more": false, "records": [record(start + 60_000)]}}),
        ));
        let login = LocalLogin::with_session();
        let status = rt()
            .block_on(login.adapter(&s).query_sign_status())
            .unwrap();
        assert_eq!(status, SignStatus::SignedToday);
        // 缓存件随成功提取回写
        let cache = login.0.path().join("cache.json");
        assert!(cache.exists(), "live 提取成功后应回写缓存件");
        let raw = std::fs::read_to_string(&cache).unwrap();
        assert!(!raw.contains("SESS-1"), "缓存件必须是封存件");
    }

    #[cfg(windows)]
    #[test]
    fn query_status_without_todays_record_is_not_signed() {
        let s = rt().block_on(MockServer::start());
        let now_ms = chrono::Utc::now().timestamp_millis();
        let (start, _) = beijing_today(now_ms);
        rt().block_on(mount_records(
            &s,
            json!({"code": 0, "data": {"has_more": false, "records": [record(start - 86_400_000)]}}),
        ));
        let login = LocalLogin::with_session();
        let status = rt()
            .block_on(login.adapter(&s).query_sign_status())
            .unwrap();
        assert_eq!(status, SignStatus::NotSigned);
    }

    /// 从磁盘读出的客户端版本必须真的出现在发出的请求上（而不是只改了内存里的值）
    #[cfg(windows)]
    #[test]
    fn query_sends_version_code_derived_from_installed_client() {
        let s = rt().block_on(MockServer::start());
        let now_ms = chrono::Utc::now().timestamp_millis();
        let (start, _) = beijing_today(now_ms);
        rt().block_on(mount_records(
            &s,
            json!({"code": 0, "data": {"has_more": false, "records": [record(start + 60_000)]}}),
        ));
        let login = LocalLogin::with_session();
        rt().block_on(login.adapter(&s).query_sign_status()).unwrap();
        let reqs = rt().block_on(s.received_requests()).unwrap();
        let q = reqs[0].url.query().unwrap_or("");
        let want = format!(
            "version_code={}",
            version_to_code(&client_version()).unwrap_or_default()
        );
        assert!(
            q.contains(&want),
            "query 应带按客户端版本派生的 version_code，实际: {q}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn missing_records_key_reports_schema_changed() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_records(
            &s,
            json!({"code": 0, "data": {"whatever": 1}}),
        ));
        let login = LocalLogin::with_session();
        let e = rt()
            .block_on(login.adapter(&s).query_sign_status())
            .unwrap_err();
        assert!(matches!(e, AdapterError::SchemaChanged(_)), "{e:?}");
    }

    async fn mount_checkin(s: &MockServer, body: Value) {
        Mock::given(method("POST"))
            .and(path("/alice/ug/campaign/landing/checkin"))
            .and(wiremock::matchers::body_json(
                json!({"scene": "pc_incentive_task_mvp_v3"}),
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .mount(s)
            .await;
    }

    #[cfg(windows)]
    #[test]
    fn sign_in_posts_checkin_with_scene_and_reports_reward() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_checkin(
            &s,
            json!({"code": 0, "data": {"checked_in": true, "idempotent": false,
                                        "reward_amount": 200, "reward_type": "quota_grant"}}),
        ));
        let login = LocalLogin::with_session();
        let out = rt().block_on(login.adapter(&s).sign_in()).unwrap();
        assert_eq!(out, SignOutcome::Success("+200 额度".into()));
    }

    #[cfg(windows)]
    #[test]
    fn idempotent_replay_reports_already_signed() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_checkin(
            &s,
            json!({"code": 0, "data": {"checked_in": true, "idempotent": true}}),
        ));
        let login = LocalLogin::with_session();
        let out = rt().block_on(login.adapter(&s).sign_in()).unwrap();
        assert_eq!(out, SignOutcome::AlreadySigned);
    }

    #[cfg(windows)]
    #[test]
    fn auth_failure_and_business_error_are_not_success() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(401))
                .mount(&s),
        );
        let login = LocalLogin::with_session();
        let e = rt().block_on(login.adapter(&s).sign_in()).unwrap_err();
        assert!(
            matches!(e, AdapterError::AuthExpiredMsg(_)),
            "401 应提示登录态失效: {e:?}"
        );

        let s2 = rt().block_on(MockServer::start());
        rt().block_on(mount_checkin(
            &s2,
            json!({"code": 1100, "message": "系统错误"}),
        ));
        let e = rt().block_on(login.adapter(&s2).sign_in()).unwrap_err();
        assert!(
            matches!(&e, AdapterError::Business(m) if m == "系统错误"),
            "{e:?}"
        );
    }

    /// 客户端开着 → Cookies 拷贝失败 → 回落缓存件，请求照常发出
    #[cfg(windows)]
    #[test]
    fn locked_cookies_fall_back_to_cached_cred() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_checkin(
            &s,
            json!({"code": 0, "data": {"checked_in": true, "idempotent": false,
                                        "reward_amount": 200}}),
        ));
        // 缓存件先写好；data_dir 指向不存在的地方模拟「读不到本机库」
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache.json");
        crate::secret::write_json(
            &cache,
            &json!({"cookieHeader": "sessionid=CACHED; ttwid=T", "savedAtMs": 1}),
        )
        .unwrap();
        let adapter = DoubaoAdapter::new()
            .with_base_url(s.uri())
            .with_data_dir(dir.path().join("no-such-user-data"))
            .with_cache_path(cache);
        let out = rt().block_on(adapter.sign_in()).unwrap();
        assert_eq!(out, SignOutcome::Success("+200 额度".into()));
    }

    /// 没有本机库也没有缓存：报错要发生在发请求之前，且文案指向豆包
    #[test]
    fn missing_login_is_reported_before_any_request() {
        let s = rt().block_on(MockServer::start());
        rt().block_on(mount_records(
            &s,
            json!({"code": 0, "data": {"records": []}}),
        ));
        let dir = tempfile::tempdir().unwrap();
        let adapter = DoubaoAdapter::new()
            .with_base_url(s.uri())
            .with_data_dir(dir.path().to_path_buf())
            .with_cache_path(dir.path().join("none.json"));
        let e = rt().block_on(adapter.query_sign_status()).unwrap_err();
        assert!(e.to_string().contains("豆包"), "文案应指向豆包: {e}");
        assert!(matches!(e, AdapterError::AuthExpiredMsg(_)), "{e:?}");
    }

    #[cfg(windows)]
    #[test]
    fn logged_out_local_state_reports_login_hint_without_falling_back() {
        // 本机库里没有 sessionid（用户已登出）：不许回落缓存——缓存也必然是死的
        let dir = LocalLogin::with_cookies(vec![cookie_row("ttwid", "T", 0)]);
        let cache = dir.0.path().join("cache.json");
        crate::secret::write_json(
            &cache,
            &json!({"cookieHeader": "sessionid=CACHED", "savedAtMs": 1}),
        )
        .unwrap();
        let adapter = DoubaoAdapter::new()
            .with_base_url("http://127.0.0.1:9")
            .with_data_dir(dir.0.path().to_path_buf())
            .with_cache_path(cache);
        let e = rt().block_on(adapter.query_sign_status()).unwrap_err();
        assert!(e.to_string().contains("登录"), "应指向重新登录: {e}");
    }

    #[test]
    fn uid_from_cookie_header_trusts_segment_matching_session() {
        let h = "multi_sids=111:AAAA|222:BBBB; sessionid=AAAA; sessionid_ss=AAAA";
        assert_eq!(super::uid_from_cookie_header(h).as_deref(), Some("111"));
    }

    #[test]
    fn uid_from_cookie_header_decodes_percent_colon() {
        let h = "multi_sids=111%3AAAAA; sessionid_ss=AAAA";
        assert_eq!(super::uid_from_cookie_header(h).as_deref(), Some("111"));
    }

    #[test]
    fn uid_from_cookie_header_decodes_fully_encoded_multi_sids() {
        // 2026-10-15 实测形态：整段 cookie 值全 URL 编码（%3A 冒号、%7C 分隔竖线），
        // 先按裸 | 分段会切不开 → 必须先整体解码（本 bug 的漏网用例）
        let h = "multi_sids=111%3AAAAA%7C222%3ABBBB; sessionid=AAAA; sessionid_ss=AAAA";
        assert_eq!(super::uid_from_cookie_header(h).as_deref(), Some("111"));
        // 小写百分号编码同样要认
        let h = "multi_sids=111%3aAAAA%7c222%3aBBBB; sessionid=AAAA";
        assert_eq!(super::uid_from_cookie_header(h).as_deref(), Some("111"));
    }

    #[test]
    fn uid_from_cookie_header_rejects_mismatch_and_absence() {
        // 会话主体对不上：不采信
        assert_eq!(
            super::uid_from_cookie_header("multi_sids=111:AAAA; sessionid=ZZZZ"),
            None
        );
        // 没有会话主体 / 没有 multi_sids：无从解析
        assert_eq!(super::uid_from_cookie_header("multi_sids=111:AAAA"), None);
        assert_eq!(super::uid_from_cookie_header("sessionid=AAAA"), None);
        // 空 uid / 非数字 uid：不采信
        assert_eq!(
            super::uid_from_cookie_header("multi_sids=:AAAA; sessionid=AAAA"),
            None
        );
        assert_eq!(
            super::uid_from_cookie_header("multi_sids=abc:AAAA; sessionid=AAAA"),
            None
        );
    }

    /// account_label 端到端：本机库读不到（豆包开着）回落缓存，uid 从实际 cookie 解析
    #[test]
    fn account_label_uses_uid_from_resolved_cookies() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("cache.json");
        crate::secret::write_json(
            &cache,
            &json!({
                "cookieHeader": "multi_sids=111:AAAA|222:BBBB; sessionid_ss=AAAA",
                "savedAtMs": 1
            }),
        )
        .unwrap();
        let adapter = DoubaoAdapter::new()
            .with_data_dir(dir.path().join("no-such-user-data"))
            .with_cache_path(cache);
        let label = rt().block_on(adapter.account_label()).unwrap();
        assert_eq!(label, "豆包账号 111");
    }

    #[test]
    #[ignore = "只读诊断：读本机豆包 Cookies 并打印 cookie 名单，不联网"]
    fn diag_local_cookie_names() {
        let dir = super::doubao_user_data_dir().expect("本机无豆包数据目录");
        let pairs = extract_cookie_pairs(&dir, now_s()).unwrap();
        println!(
            "cookies={} 名单={:?}",
            pairs.len(),
            pairs.iter().map(|(n, _)| n).collect::<Vec<_>>()
        );
    }

    #[test]
    #[ignore = "只读诊断：真实接口查状态，不签到"]
    fn diag_live_status() {
        let rt = rt();
        let adapter = DoubaoAdapter::new();
        let status = rt.block_on(adapter.query_sign_status()).unwrap();
        println!("status={status:?}");
    }

    #[test]
    #[ignore = "诊断：走生产路径真实签到（幂等，今日已签只会返回 AlreadySigned）"]
    fn diag_live_sign_in() {
        let rt = rt();
        let adapter = DoubaoAdapter::new();
        let out = rt.block_on(adapter.sign_in()).unwrap();
        println!("sign_in -> {out:?}");
    }
}
