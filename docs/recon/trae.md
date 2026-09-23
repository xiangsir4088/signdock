# Trae 签到侦察

- 日期 / 客户端版本：2026-09-23 / TRAE SOLO CN（本机实测；源码依据 trae-mate-main，Tauri+Vue 同类工具）
- 侦察人：Qoder（trae-mate 源码精读 + 本机解密验证 + 只读在线探测，未执行 claim）

## 签到接口
- [x] Method + URL：`POST {host}/trae/api/v2/ug/checkin_credits/status` 与 `POST {host}/trae/api/v2/ug/checkin_credits/claim`，Body `{}`
  - host 取自凭据 `auth.host`（本机实测 `https://api.trae.cn`）；空则默认 `https://api.trae.cn`
- [x] 必需 Headers（`trae-mate checkin.rs:36-69`，本机探测服务端接受）：
  - `authorization: Cloud-IDE-JWT <jwt>`（**前缀不是 Bearer**；已带前缀则原样）
  - `x-device-id`（15 位数字）、`x-market-user-id`（UUIDv4 形态）、`vscode-sessionid`（64 hex）——三者由 userId **确定性派生**（SHA-256 流，见下），无需持久化：同 userId 永远同值
  - `user-agent: VSCode 1.107.1 (TRAE SOLO CN)`、`x-market-client-id: VSCode 1.107.1`、`x-user-region: CN`、`x-lgw-req-sdk-type: 3`、`package-type: stable_cn`、`x-lscbd-aid: 787976`、`x-lscbd-platform: windows`、`app-version: 0.1.45`、`accept: */*`、`accept-language: zh-CN`、`content-type: application/json`
  - `x-tt-trace-id: 00-<16hex>-01`、`x-request-id: <uuid形态>` 每次请求刷新（值只需格式合理，无校验证据）
- [x] 请求 Body 与签名参数：**无签名**，空对象 `{}`
- [x] 成功响应样例（脱敏，2026-09-23 实测 status 200）：
  `{"checked_in":true,"code":0,"credits":150,"did_checked_in":true,"enable":true,"extra_credits":50,"message":"success"}`
- 判定（trae-mate `api_succeeded`，checkin.rs:26-31）：`code∈{0,200}`（数字或字符串）或 `success==true` 或 `status=="success"`
- 错误分类参考（cooldown.rs:24-45）：HTTP 401=SessionDead（永久）；200+code 1005=PlanLimit；429=SoftRate；**SignDock 决策：不引入冷却状态机**，401/403→AuthExpired，其余按 SchemaChanged/Http 走引擎既有语义

## 设备身份派生（device_map.rs:18-65，golden 测试向量来自其 Python 对照测试）
```
seeded_stream(seed, salt, n): 第 i 块 = SHA256("<salt>:<seed>" ‖ u32be(i))，拼接截断
x-device-id      = rand_digits(15, uid)   salt="devid"  每位 = b % 10
vscode-sessionid = rand_hex(64, uid)      salt="sess"
x-market-user-id = gen_market_uuid(uid)   salt="market" 16 字节 → v4/variant 位修正 → 8-4-4-4-12
```
向量：uid=`1234567890123456` → devid `413174708280782`、sess `fbabf7aa1e173b90385c623e1ec49157860cc07a4b01f53f7ca141b17d876eae`、market `746608f9-7f37-4960-b4c7-8553cec6d366`；uid=`9876543210` → devid `630512735296035`、sess `78e8868abb83fdf82f3852e9563b07184ef8deeb462568d852affd8d59a4bbd9`、market `c2aec55c-c9bb-4f68-b068-b8fa183b958b`

## 本机登录态
- [x] 存储路径：`%APPDATA%\<变体>\User\globalStorage\storage.json`，键 `iCubeAuthInfo://icube.cloudide`（base64 信封）
  - 本机变体实测：**TRAE SOLO CN**（CN 账号，token 有效）✓可解密；**Trae**（SG 账号，token 已过期但 refresh 在）✓可解密；**Trae CN** ✗信封头不匹配（新版换加密格式，暂不支持）
  - SignDock 决策：按 `["TRAE SOLO CN","Trae CN","Trae"]` 顺序取第一个**存在且可解密**的目录 = 当前活动账号；不做多账号选择
- [x] 加密方式：AES-128-CBC 信封（`trae_auth.rs:25-102`）
  - 信封 = `HEADER[116,99,5,16,0,0]` + randomKey(32) + 密文；secret = LEFT_SECRET⊕RIGHT_SECRET（两张 64 字节硬编码表）
  - derived = SHA512( SHA512(randomKey) ‖ secret )；key=derived[0..16]，iv=derived[16..32]，PKCS7
  - 明文 = SHA512(payload)(64) ‖ payload(JSON)；校验 digest
  - payload 字段：`token`（JWT≈1KB）、`refreshToken`、`userId`、`expiredAt`/`refreshExpiredAt`（RFC3339 字符串）、`host`、`account.username`、`userRegion.region`
- [x] 提取步骤：读 storage.json → 解出信封 → 取 token/userId/host/expiredAt；**JWT exp 双保险**（`jwt.rs parse`：payload `data.id`/`exp`，base64url 无填充）
- [x] **不做主动刷新**（用户决策 2026-09-23）：token 过期 → AuthExpired，提示「token 已过期，请打开 TRAE 让它自己刷新后重试」；ExchangeToken 接口存在（`POST https://api.trae.com.cn/cloudide/api/v3/trae/oauth/ExchangeToken`，ClientID `en1oxy7wnw8j9n`，ClientSecret `"-"`）但**刻意不调用**（避免轮换 refreshToken 挤掉 trae-mate 已存凭证）
- [x] 手动兜底：粘贴裸 JWT 亦可（自动补 `Cloud-IDE-JWT ` 前缀）

## 积分/资源接口（2026-09-23 实测 200）
- `POST {host}/trae/api/v2/pay/ide_user_ent_usage`，Body `{"require_usage":true,"req_source":2}`（headers 同签到全套）
- 响应顶层：`user_entitlement_pack_list[]`，包字段 `expire_time`(Unix 秒)、`usage.credits_amount`、`entitlement_base_info.{quota.credits_limit, start_time, charge_amount}`
- 余额 = Σ max(0, credits_limit − credits_amount)（仅统计有 credits_limit 的包）——本机实测 5444.19（32 包）
- 今日到期 = 上述剩余中 `expire_time` 落在**北京时间** [今日0点, 明日0点) 的包（cst 固定 +08:00，不依赖系统时区）——本机实测今日为 0，最早过期 2026-09-24 10:33
- **当日消耗无 API 字段**（trae-mate 靠本地昨日快照反推）→ SignDock 决策：`today_used` 返回 0.0，UI 文案对 trae 不展示该项或展示为 0（YAGNI，不引入本地快照推算）

## 风控观察
- [x] 无验证码/无请求签名；设备三件套只需格式合理+稳定（确定性派生即可，无需持久化）
- 直连 api.trae.cn，注意 reqwest 默认不读系统代理（本机如有代理软件不影响，trae-mate 同源结论）

## 结论
- [x] 可行模式：**自动签到**（解密本机登录态 + 确定性设备身份 + 无签名接口）
- 新增依赖（决策，突破 workbuddy 计划的"不新增依赖"约束）：`aes 0.8`、`cbc 0.1`、`sha2 0.10`、`base64 0.22`（纯 Rust；trace/request id 用 sha256(纳秒) 手工造，不引 uuid crate）
- 未决问题：
  - Trae CN 变体新信封格式未知（本机不活跃，出现时再看）
  - claim 成功后返回体是否含积分数值未实测（今日已签，只探测了 status）→ 实现按 message 兜底

---
参考源码索引：trae-mate `checkin.rs`（引擎/headers/判定）、`trae_auth.rs`（信封解密）、`device_map.rs`（派生+测试向量）、`jwt.rs`（JWT 解析/ExchangeToken）、`credits.rs:26-115`（积分计算）、`cooldown.rs`（错误分类参考，不移植）
