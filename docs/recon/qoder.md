# qoder 签到侦察

- 日期 / 客户端版本：2026-09-24，Qoder CN（安装 `D:\Program Files\Qoder CN`，Electron 非 VSCode fork）
- 侦察人：SignDock 会话内实测（探测脚本留在 `Temp/`，gitignore）

## 签到接口
- [x] `POST https://openapi.qoder.com.cn/sash/api/v1/me/campaigns/{campaignId}/claim`，Body `{}`（2026-09-24 实测 200）
- [x] 必需 Headers：`Authorization: Bearer <token>` + `Cosy-ClientType: 10` + `User-Agent: Qoder` + `Accept: application/json`
  - **`Cosy-ClientType` 是必需项**：只带 Bearer 时 campaigns 返回空列表（不是 401，是静默为空）→ 若漏掉会误判「没有活动」
- [x] 成功响应（已领项重放，长串已脱敏）：
  `{"grantId":"<uuid>","status":"CLAIMED","replayed":true,"benefit":{"kind":"CREDITS","amount":100,"modelScope":{"modelSeries":{"key":"ALL_MODELS"}},"validity":{"mode":"RELATIVE_DAYS","days":30}},"campaignId":"<uuid>","campaignKey":"act-20260923-159","campaignVersion":1,"claimedAt":"<RFC3339>","grantedAt":"<RFC3339>","expiresAt":"<RFC3339>"}`
  - **幂等标记 = `replayed`**：同窗口重复 POST → 200 + `replayed:true`，不重复发积分。SignDock 以此判 `AlreadySigned`
- [x] 接口路径来源：活动页前端 bundle `https://g.alicdn.com/qbase/qoder/0.0.709/growth-page/activity-iframe/activity-iframe.js`（枚举 `CLAIMABLE` / `CLAIMED` / `CLAIM_BENEFIT` / `VIEW_DETAILS` / `CREDITS` 均在其中）

## 状态查询接口
- [x] `GET /sash/api/v1/me/campaigns`（同上 headers）→ **顶层无包装**：`{uid, showCampaign, claimable, campaignUrl, campaigns[]}`
- [x] campaign 字段：`campaignId` `campaignKey` `actionType` `claimStatus` `startAt` `endAt`（**Unix 秒**） `placements` `benefit?{kind,amount,modelScope,validity}`
- [x] 本机实测（2026-09-24 12:03）两条：
  - `CLAIM_BENEFIT` / `act-20260923-159` / `CLAIMED` / CREDITS 100 / 窗口 **2026-09-24 10:00 → 2026-09-25 09:59（北京）**
  - `VIEW_DETAILS` / `act-20260901-922` / 无 benefit / 窗口 2026-09-01 14:20 → 09-30 23:59 → **必须按 `actionType==CLAIM_BENEFIT` 过滤**，否则会把静态推广当签到项
- [x] **日窗口 = 北京时间 10:00 滚动**，不是 0 点 → 计划时间必须 ≥10:00（默认给 10:05）；0:00–9:59 查询命中的是**昨日窗口**

## 积分接口
- [x] `GET /sash/api/v2/me/usage` → `{displayMode:"qoder", qoderUsage:{userId, userType:"personal_standard", usageType:"credits", totalUsagePercentage, isQuotaExceeded, expiresAt(毫秒，实测 253402214400000=永不), upgradeUrl, userQuota{total,used,remaining,percentage,unit}, addOnQuota{total,used,remaining,percentage,unit,detailUrl}, isPlanQuotaProrated}}`
  - 本机实测：`userQuota.remaining=0`、`addOnQuota.remaining=400` → **余额 = 两者 remaining 之和**
  - `qoderUsage.expiresAt` 是套餐到期（本机 = 公元 9999）→ 只有落在**北京今日**才算 `expiring_today`
- [x] 辅助（未采用）：`/sash/api/v1/ai-conversations/credits-summary` → `{totalCredits, peakCredits, peakDate, unit}` 是**历史消耗**非余额；`seat-activity` → `{cumulativeActiveDays, currentConsecutiveDays, lastActiveDate, maxConsecutiveDays}` 连续活跃天数
- [x] **当日消耗无 API 字段** → 与 trae 同决策：`today_used = 0.0`，不引入本地快照反推

## 本机登录态
- [x] 存储：`%APPDATA%\com.qodercn.app.stable\`
  - `Local State`（JSON）→ `os_crypt.encrypted_key`：base64 → 解出 283 字节，头 5 字节 ASCII `DPAPI`，余下 = **DPAPI（当前用户）包裹的 32 字节 AES key**
  - `auth.v1.dat`：`v10`(3) + nonce(12) + **AES-256-GCM** 密文（尾 16 字节 tag），无 AAD（Chromium os_crypt 标准形态）
  - 明文 JSON 字段：`token`(27 字符不透明串，**不是 JWT**)、`refreshToken`(28)、`expiresAt`(RFC3339，实测 2026-10-22 → 约 28 天)、`refreshTokenExpiresAt`(2027-09-17)、`schemaVersion`、`user{...}`
- [x] 提取步骤：读 Local State → base64 解码 → 剥 `DPAPI` 前缀 → `CryptUnprotectData` → 32B key → 解 auth.v1.dat → 取 `token`/`expiresAt`
- [x] **不刷新**：bundle 里存在 `POST /api/v1/deviceToken/refresh`，与 trae 同决策刻意不调用（会轮换 refreshToken，挤掉 Qoder 自己的登录态）。token 过期 → 提示「请打开 Qoder 让它自己刷新后重试」
- 用户裁定（2026-09-24）：**允许**本机 DPAPI 解密作为凭证来源。这是「不逆向厂商凭据保护」的既定例外——密钥由 Windows DPAPI 绑定当前用户保护，不含厂商构建期私有密钥（对比 WorkBuddy 5.6 那条已否决的路线）。全程**只读**，绝不回写产品目录

## 风控观察
- [x] 无验证码、无请求签名、无设备三件套；`sign_in` 幂等（`replayed`）
- [x] 401/403 → 视为登录态失效（bundle 内 401 即 `invalidateAccessToken`）

## 结论
- [x] 可行模式：**自动签到**（DPAPI 本机凭证 + 确定性窗口 + 无签名接口 + 幂等 claim）
- 新增依赖：`aes-gcm 0.10`（纯 Rust）、`windows 0.61`（与 tauri 已锁定版本同源，仅 `Win32_Security_Cryptography`/`Win32_Foundation`/`Win32_System_Memory`，target-cfg windows）
- 未决问题：
  - 未领取时 `claimStatus` 是否就是 `CLAIMABLE`（本机今日已领，只能从前端 bundle 枚举推断）→ 实现按「非 CLAIMED 且在窗口内 = 可领」宽容处理
  - `claim` 返回 200 但 `status != CLAIMED` 的形态未实测 → 按 `Failed` 兜底并打文案
  - 多个 `CLAIM_BENEFIT` 窗口并存（补签？）未观测 → 取窗口内的一条，退化时取 `endAt` 最大的一条

---
参考：探测脚本 `Temp/qp.py`（DPAPI+GCM 解密、header 组合对比）、`Temp/qk.py`（campaigns 结构）、`Temp/qc.py`（claim 幂等）、`Temp/qs.py`（usage/summary/seat-activity）——全部一次性仪器，只打印键名/长度/时间戳。
