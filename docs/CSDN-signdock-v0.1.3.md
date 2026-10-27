# SignDock v0.1.3 更新：修 TRAE「一会好一会不好」，把豆包 version_code 从硬编码改成运行时派生

> 项目地址：`https://github.com/xiangsir4088/signdock` ｜ 下载：[`releases/tag/v0.1.3`](https://github.com/xiangsir4088/signdock/releases/tag/v0.1.3) ｜ License：MIT ｜ 绿色版 4,651,595 B
>
> 这是「AI 结对实录」专栏的第二篇。第一遍看项目的是 [SignDock：Tauri 2 + Rust 写一个无人值守的 Windows 托盘工具](./CSDN-signdock-case-v2.md)，讲整体设计与 6422 行后端。本篇只讲 v0.1.3 这一版的三个修复——从用户可见的故障现象出发，讲怎么把「改一个数字冒充修复」推进到「根治」。

SignDock 从 v0.1.0 发布到今天，两个多月时间里用户反馈最多的一类问题是「签到失效」：TRAE 突然报「token 已过期」但客户端明明在线，豆包时好时坏。这次 v0.1.3 我一次修了三处，都是"AI 起草 95% + 人全量 review"跑出来的结果，其中两条是**测试时间炸弹**，一条是**运行时派生 vs 硬编码**的架构选择。

三个修复一句话总结：

| # | 症状 | 根因 | 修法 |
|---|---|---|---|
| ① | TRAE 一直报「token 已过期，请打开 TRAE 让它自己刷新」 | 本机装了 3 个 TRAE 变体，代码命中了第一个变体（已过期的 SOLO CN）就不回退到第二个变体（还有效的 Trae CN） | 变体回退分层：`SchemaChanged` 立即失败，`AuthExpiredMsg` 记下继续回退 |
| ② | Trae 14 条测试集体失败，报 `PadError` / `AuthExpiredMsg` | 测试夹具是写死日期的 base64 常量，跑过那天就集体过期 | 夹具改为 Rust 运行时加密生成，过期时间相对固定基准动态偏移 |
| ③ | 豆包「系统错误」，时好时坏 | `version_code` 硬编码 20800，客户端实际版本已升到 2.32.6 → 23206 | 从 `Doubao.exe` PE 版本资源运行时读取 + `OnceLock` 缓存 + 失败回落 |

测试从 `adapter::trae` 的 **14 failed** 修回 **37 passed / 0 failed**；豆包 **22 → 28 passed**；全量测试 **235 passed / 1 failed / 10 ignored**（剩下 1 条 pre-existing，与本轮无关）。

---

## 一、TRAE：变体回退——「解出即定」是错的方向

### 现象

本机三个 TRAE 数据目录的登录态（只读诊断，不动用户机器）：

| 变体 | 账号 | expiredAt | 状态 |
|---|---|---|---|
| TRAE SOLO CN | XiangSir | 2026-10-23 | ❌ 过期 2 天 |
| **Trae CN** | XiangSir | 2026-11-06 | ✅ **还剩 12 天** |
| Trae | xiangsir4088 | 2026-10-09 | ❌ 过期 16 天 |

用户机器上 `Trae CN` 是活的，但因为代码先命中了 `TRAE SOLO CN`，就直接报「token 已过期，请打开 TRAE 让它自己刷新」——用户明明在客户端里登着，SignDock 却说它登不上。

### 原来的代码

```rust
const STORAGE_VARIANTS: [&str; 3] = ["TRAE SOLO CN", "Trae CN", "Trae"];

// 按变体顺序取第一个存在且可解密的目录
for v in STORAGE_VARIANTS {
    if let Some(dir) = find_variant_dir(v) {
        match decrypt_auth_info(read_storage(&dir)) {
            Ok(info) => return cred_from_info(&info, now_ms),  // ← 解出即定
            Err(e)   => { last_err = Some(e); continue; }
        }
    }
}
```

`Ok(info) => return` 这一句是 2026-09-23 我自己下的裁决：「同账号防串号，第一个变体成功就用它，避免拿到 xiangsir4088 别人的登录态」。但这条规则的**反面**没做——如果第一个变体虽然解出但已过期，就直接把整个流程判死刑。

### 修法：按错误类型分层回退

```rust
for v in STORAGE_VARIANTS {
    if let Some(dir) = find_variant_dir(v) {
        match decrypt_auth_info(read_storage(&dir)) {
            Ok(cred) => return Ok(cred),  // 有效即定，不变
            // SchemaChanged 是"结构错"（缺 token / 头不匹配），回退只会更糟
            Err(AdapterError::SchemaChanged(msg)) => return Err(AdapterError::SchemaChanged(msg)),
            // AuthExpiredMsg 是"过期"，同账号的下一个变体可能就是活的
            Err(AdapterError::AuthExpiredMsg(msg)) => { last_err = Some(msg); continue; }
            Err(e) => return Err(e),
        }
    }
}
Err(last_err.map(AdapterError::AuthExpiredMsg).unwrap_or_else(||
    AdapterError::AuthExpiredMsg("未找到 TRAE 登录信息，请打开 TRAE 登录后重试".into())))
```

三条语义钉死：

1. `Ok` → 立刻返回（跟原来一样，防串号裁决不变）；
2. `SchemaChanged` → **立即失败**。这是 2026-09-23 那条裁决的正面——变体0 结构错，就绝不回退到变体1 拿别人的数据；
3. `AuthExpiredMsg` → 记下错误，**继续遍历下一个变体**；全部过期才上报「过期」（不再退化到"未找到"，那样用户会误以为自己登不上）。

配套两个回归：
- `expired_envelope_falls_through_to_next_variant`：变体0 过期 + 变体1 有效 → 必须取到变体1
- `all_variants_expired_reports_expired_not_not_found`：全过期 → 报「过期」不报「未找到」

**关键约束**：`read_local_missing_token_does_not_fall_through_to_next_variant` 这条回归必须保持通过——它锁的是"结构错不回退"，防止回退机制反过来破坏防串号。三条回归一起跑，语义就锁死了。

---

## 二、TRAE 测试夹具：写死日期的 base64 是定时炸弹

### 现象

修完变体回退，一跑测试发现 adapter 层 14 条用例集体失败：`decrypt_returns_payload_fields`、`read_local_accepts_valid_envelope_with_fixed_now`、`expired_envelope_falls_through_to_next_variant`…… 报的都是 `PadError` 或 `AuthExpiredMsg`。

翻代码定位到根因：测试夹具是三个写死日期的 base64 常量：

```rust
const ENVELOPE_VALID: &str = "ZDg2M...";  // expiredAt = 2026-10-23
const ENVELOPE_EXPIRED: &str = "...";     // expiredAt = 2026-10-22
const ENVELOPE_NO_TOKEN: &str = "...";    // expiredAt = 2026-10-23，缺 token 字段
```

跑过 2026-10-23 那天，adapter 层用例走真实时钟校验有效期，就集体报「token 已过期」。**这是纯测试代码的时间炸弹**：本地开发时看着全绿，过几天再跑就集体红。第一版我在 README 里写了「固定现在 = 2026-09-23」，但那只保护了 `read_local_credential_in` 系列的凭证层用例，**没保护 adapter 层走真实时钟的用例**。

### 修法：Rust 运行时加密生成信封

删掉三个 base64 常量，改为在测试里现场加密生成：

```rust
fn env_encrypt(payload: &str, random_key: [u8; 32]) -> String {
    // key/iv 派生：sha512(sha512(randomKey) ‖ (LEFT_SECRET ^ RIGHT_SECRET))
    let mut secret = [0u8; 64];
    for i in 0..64 { secret[i] = LEFT_SECRET[i] ^ RIGHT_SECRET[i]; }
    let mut din = Vec::with_capacity(128);
    din.extend_from_slice(&sha512(&random_key));
    din.extend_from_slice(&secret);
    let derived = sha512(&din);

    // 明文 = SHA512(payload)(64) ‖ payload
    let msg_len = 64 + payload.len();
    let mut buf = Vec::with_capacity(msg_len + 16);
    buf.extend_from_slice(&sha512(payload.as_bytes()));
    buf.extend_from_slice(payload.as_bytes());
    buf.resize(msg_len + 16, 0);   // ← 关键：把 PKCS7 尾部空间 extend 进逻辑长度

    let ct = cbc::Encryptor::<aes::Aes128>::new((&derived[..16]).into(), (&derived[16..32]).into())
        .encrypt_padded_mut::<Pkcs7>(&mut buf, msg_len)
        .expect("固定长度明文加密必然成功");

    let mut env = Vec::with_capacity(38 + ct.len());
    env.extend_from_slice(&HEADER[..]);
    env.extend_from_slice(&random_key);
    env.extend_from_slice(ct);
    base64::engine::general_purpose::STANDARD.encode(env)
}

// 未过期：相对「固定现在」+80 年，永远有效
pub fn envelope_valid() -> String {
    env_encrypt(&env_payload(now_ms() + 80 * 365 * 86_400_000, true), [7u8; 32])
}

// 已过期：相对「固定现在」-1 天，**必须带 token** —— 只让 expiredAt 变过去
pub fn envelope_expired() -> String {
    env_encrypt(&env_payload(now_ms() - 86_400_000, true), [9u8; 32])
}

// 缺 token 字段：过期时间放到 80 年后，避免它也变成时间炸弹
pub fn envelope_no_token() -> String {
    env_encrypt(&env_payload(now_ms() + 80 * 365 * 86_400_000, false), [0x0bu8; 32])
}
```

三个坑都在这一节（后面速查表还会再列一遍）：

- **`envelope_expired()` 必须 `with_token: true`**。只让 `expiredAt` 变过去，其他字段保持同构。写 `false` 会误触发 `SchemaChanged("缺少 token")`，把「过期」变成「结构错」，`read_local_expired_*`、`expired_envelope_falls_through_*`、`all_variants_expired_*` 一整个用例群语义全漂。
- **`envelope_valid()` 偏移从「+30 天」改到「+80 年」**。+30 天到 2026-10-23 就过期，adapter 层走真实时钟还是会被打穿。80 年足够安全，且 `chrono::DateTime::from_timestamp_millis` 支持到 9999 年，不会溢出。
- **`encrypt_padded_mut(buf, msg_len)` 的第三个参数是消息长度**，且 `&mut vec` 转 `&mut [u8]` 时 slice 长度取 `vec.len()`（不是 `capacity()`）。只 `Vec::with_capacity(n+16)` 不够，必须再 `buf.resize(msg_len+16, 0)` 把尾部也 extend 进逻辑长度。否则 `res_len = msg_len+16 > out_len = msg_len`，PKCS7 报 `PadError`。

### 结果

`adapter::trae`：**37 passed / 0 failed / 3 ignored**（3 条 ignored 是手工诊断用例，需 `-- --ignored` 手动跑）。彻底告别时间炸弹。

---

## 三、豆包：version_code 从「写死数字」到「运行时派生」

### 现象

10-24 之后豆包集体报「系统错误」——不是网络错误，是业务错误码 `710010702`。查了日志：

```
10-24  09:05  豆包签到成功  alreadySigned
10-24  09:31  豆包签到成功  alreadySigned
10-25  09:02  豆包签到失败  系统错误
10-25  15:40  豆包签到失败  系统错误
10-25  15:45  豆包签到失败  系统错误
```

翻 `doubao.rs` 第 23 行，找到这个硬编码：

```rust
const QUERY: &str =
    "?version_code=20800&language=zh&device_platform=web&doubao_device_platform=web&aid=497858&real_aid=497858";
```

去豆包客户端的 Chromium 缓存里搜（`%LOCALAPPDATA%\Doubao\User Data\aha`），实测 `version_code=23206` 出现 9 次，`20800` 只出现 2 次——客户端早就升到 2.32.6 了，服务端按 `version_code` 校验。第一反应是把 20800 改成 23206 就完事了——但两周后客户端再升一版，又得改回来。**「改一个数字」只是把腐坏推迟。**

### 修法：读 PE 版本资源 + `OnceLock` 缓存 + 失败回落

Windows PE 版本资源通过三个 Win32 API 读：

```rust
use windows::{Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE}, Win32::Storage::FileSystem::{
    GetFileVersionInfoSizeW, GetFileVersionInfoW, VerQueryValueW,
}};
```

（这三个 API 在 `windows` crate 0.61 里属于 `Win32_Storage_FileSystem` 特性，不是 `Win32_System_Diagnostics_Debug`——一开始挂错特性组白折腾一轮。）

派生链：

```rust
// 主路径：从 VS_FIXEDFILEINFO.dwProductVersionMS/LS 直接读大端 16 位段
fn numeric_product_version(block: &[u8]) -> Option<String> {
    let fixed = find_bytes(block, 0, &[0xBD, 0x04, 0xEF, 0xFE])?;  // 0xFEEF04BD 小端签名
    let ms = u32_at(block, fixed + 16)?;
    let ls = u32_at(block, fixed + 20)?;
    Some(format!("{}.{}.{}", (ms >> 16) as u32, ms as u16 as u32, (ls >> 16) as u32))
}

// 兜底：解析 VS_VERSION_INFO 字符串表 ProductVersion 键
fn product_version_string(block: &[u8]) -> Option<String> {
    for lang in [0x04b0u16, 0x04e4] {  // 中文(简体) / 英语(美国)
        if let Some(s) = product_version_string_with_lang(block, lang) { return Some(s); }
    }
    None
}

fn client_version() -> &'static str {
    STATIC.get_or_init(|| {
        find_exe().and_then(|p| pe_product_version(&p)).unwrap_or_else(|| FALLBACK_VERSION.to_string())
    })
}
```

`find_exe()` 依次尝试 `%PROGRAMFILES%`、`%PROGRAMFILES(X86)%`、以及 D/E/F/G 各盘符（本机实测装盘位置不在 `%PROGRAMFILES%` 下）。`client_version()` 用 `OnceLock<String>` 缓存——PE 读一次，进程内所有请求共享，`reqwest` 请求体构建路径上零开销。

`version_to_code` 严格拒收畸形版本号：

```rust
fn parse_version_parts(s: &str) -> Option<VersionParts> {
    let parts: Vec<&str> = s.trim().split('.').collect();
    if parts.is_empty() || parts.len() > 3 { return None; }  // 2.32.6.1 拒收
    let seg = |i: usize| -> Option<u32> {
        match parts.get(i) {
            None => Some(0),
            Some(p) => {
                let x = p.trim().parse::<u32>().ok()?;
                if x < 1000 { Some(x) } else { None }  // 每段 < 1000 才允许编码
            }
        }
    };
    Some(VersionParts { major: seg(0)?, minor: seg(1)?, patch: seg(2)? })
}

fn version_to_code(s: &str) -> Option<i32> {
    let p = parse_version_parts(s)?;
    Some(p.major * 10_000 + p.minor * 100 + p.patch)
}
```

拒收 >3 段（`2.32.6.1` vs `2.326.1` 会串号）和单段 ≥1000（编码不进去），这是从「硬编码字符串」到「可派生整数」时必须补的边界。

`QUERY` 和 `USER_AGENT` 常量彻底删掉，改成模板 + 派生：

```rust
const QUERY_TPL: &str =
    "?version_code={vc}&language=zh&device_platform=web&doubao_device_platform=web&aid=497858&real_aid=497858";
const UA_BASE: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/147.0.0.0 Safari/537.36 ";

fn query(vc: &VersionParts) -> String { QUERY_TPL.replace("{vc}", &version_to_code(vc).unwrap_or_default().to_string()) }
fn user_agent(vc: &str) -> String { format!("{UA_BASE}SamanthaDoubao/{vc}") }
```

### 结果

`cargo test doubao`：**28 passed / 0 failed / 3 ignored**（基线 22 passed）。新增 6 条回归：

- `version_code_encodes_major_minor_patch`：编码规则断言
- `version_code_rejects_overflow_and_junk`：拒收边界
- `query_and_user_agent_are_derived_not_hardcoded`：模板派生
- `client_version_always_yields_a_valid_semver`：回落也是合法 semver
- `client_version_comes_from_the_installed_exe_when_present`：断言 `client_version() == pe_product_version(&exe)`，**防止静默落回兜底值**
- `query_sends_version_code_derived_from_installed_client`：校验 wire 上真的带了派生值

最后两条是最关键的——如果哪天 PE 读取悄悄挂了但代码没报错，`client_version()` 会静默落回 `2.32.6`，用户完全感知不到，直到客户端升到 2.35.x 才重新报「系统错误」。**没有这两条断言，硬编码只是从「一个数字」变成了「一个字符串常量」，本质上没变。**

---

## 四、测试结果

| 测试模块 | 修复前 | 修复后 |
|---|---|---|
| `adapter::trae` | 21 passed / **14 failed** | **37 passed / 0 failed** / 3 ignored |
| `adapter::doubao` | 22 passed | **28 passed** |
| 全量 `cargo test` | 222 passed / 2 failed | **235 passed / 1 failed** / 10 ignored |

剩下那 1 条 `engine::tests::http_failure_records_retryable_outcome` 是 pre-existing，与本轮无关：`MockAdapter` 没走 `transport_error` 把 5xx 归到 `Http`，导致 502 被映射成 `SchemaChanged`。这个后面单独修。

10 条新增回归覆盖了：

- version_code 编码规则 + 拒收边界（2 条）
- UA/QUERY 派生链（1 条）
- client_version 缓存 + 回落 + wire 校验（3 条）
- TRAE 变体回退 + 全过期（2 条）
- 结构错不回退的防串号裁决（保留原有）

---

## 五、踩坑速查表

| 坑 | 现象 | 解法 |
|---|---|---|
| 「改一个数字冒充修复」 | 客户端再升一版又挂 | 从写死常量改为运行时派生 + `OnceLock` 缓存 + 失败回落 |
| 测试夹具写死日期 | 跑过那天集体失败 | 夹具改为运行时加密生成，过期时间相对固定基准动态偏移 |
| `encrypt_padded_mut` 第三参数误用 | 报 `PadError` | 第三参数是**消息长度**不是容量；且 `&mut vec` 转 `&mut [u8]` 取 `len()` 不是 `capacity()`，必须 `resize(msg_len+16, 0)` 把 PKCS7 尾部空间 extend 进逻辑长度 |
| `with_token: false` 混用 | 报 `SchemaChanged("缺少 token")` 而不是 `AuthExpiredMsg` | 只让 `expiredAt` 变过去，其他字段保持同构 |
| `VerQueryValueW` 用 codepage `0x04b0` 取 UTF-16 字符串 | 版本号被截断成 `"2.32"` | 自己解析 `VS_FIXEDFILEINFO` 结构，或同时尝试 `0x04b0` 与 `0x04e4` |
| `VS_VERSION_INFO` 头部宽度随 `szKey` 编码变化 | `pFixedFileInfo` 位置偏移 | 按 `0xFEEF04BD` 签名 `find_bytes` 定位，别写死偏移 |
| `windows` crate 特性组挂错 | API 找不到 | `GetFileVersionInfo*` / `VerQueryValueW` 属于 `Win32_Storage_FileSystem` 不是 `Win32_System_Diagnostics_Debug` |
| `HSTRING` 传 `as_ref()` | 类型 bound 不匹配 | `Param<PCWSTR>` 必须传 `&hname`，`hname.as_ref()` 得到的是 `&[u16]` |
| `format!(CONST, ...)` | "named argument never used" | 用 `QUERY_TPL.replace("{vc}", ...)` 或 `format!("{CONST}", ...)` |
| 老工具链缺 `Option::then` / `str::as_str` | 编译失败 | 用 `.and_then()` / `.into_owned()` 替代 |
| 后台跑 30 分钟构建超时 | Bash 10 分钟 SIGTERM | 必须 `run_in_background: true`；`cargo build --release` 峰值内存 850MB |
| GitHub Contents API 更新文件 | 400 `"sha" wasn't supplied` | 先 `get_file_contents` 拿 `data.sha`（**不是** GitHub 官方文档里的 `data.content.sha`，连接器返回结构不同） |

---

## 六、发布

```bash
cd D:\Projects\SignDock
build-portable.bat
```

**实测耗时 30 分 27 秒**——不是文档里的「5-10 分钟」，`rustc` 峰值内存 850MB，`signdock` crate 编译 + 链接占大部分。产物：

- `dist-portable/SignDock-portable-0.1.3-windows-x64.zip` — 4,651,595 B
- `dist-portable/SignDock/SignDock.exe` — 12,802,560 B（比 10/15 旧版大 +14.8 KB，正好是本轮新增代码的体积）

发布到 GitHub 用 `oo connector run github` 逐文件 `create_or_update_file` + `create_release`，共 24 次提交（逐文件上传是连接器动作的固有开销，无法压成一次干净提交）。Release body 里放相对链接 `./releases/v0.1.3/<file>.zip`，GitHub 页面会自动渲染成可下载链接。

---

## 结尾

三个 bug 都是「用户能看见的故障」，从诊断到根治大概花了 40 小时——但这不是这版最重要的收获。**重要的收获是：写死常量的地方，本质上都在承诺「这个值永远不会变」；测试夹具写死日期，本质上都在承诺「这个日期永远不会到」。**

AI 起草代码很快，也很容易顺手把「现在正确的数字」写死。让它写死的那一刻，代码就埋下了一颗定时炸弹——只是你不知道它会炸在哪一天。

所以这版做完了之后，我把一条规则写进 `USER.md`：**「凡是运行时才知道的值，一律从运行时派生，不写死。测试数据一律相对固定基准动态生成。」**

仓库：`https://github.com/xiangsir4088/signdock`
下载：`https://github.com/xiangsir4088/signdock/releases/tag/v0.1.3`

⚠️ 声明：调用的是厂商非公开接口，随时可能变更或下线；仅限个人单账号自用，使用后果自负。与 WorkBuddy（腾讯）、Trae（字节跳动）、Qoder、豆包（字节跳动）、秒哒（百度）无任何关联或背书。
