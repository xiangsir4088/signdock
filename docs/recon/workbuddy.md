# workbuddy 签到侦察

- 日期 / 客户端版本：2026-09-23 / WorkBuddy 桌面版（本机实测通过）
- 侦察人：Qoder（源码分析 workbuddy-switch-main + 本机真实凭证在线探测，只查状态未执行签到）

## 签到接口
- [x] Method + URL：`POST https://www.codebuddy.cn/v2/billing/meter/daily-checkin`（Body `{}`）
- [x] 必需 Headers（含鉴权头格式）：
  - `Authorization: Bearer <accessToken>`
  - `X-User-Id: <account.uid>`、`X-Domain: <auth.domain>`（实测取自 auth 文件）
  - `Accept: application/json`、`Content-Type: application/json`
  - `User-Agent`：桌面 Chrome UA（workbuddy-switch 用固定 Mac Chrome/152 UA，实测无 UA 也通过）
  - 参考来源：`account.rs:278-300`（workbuddy-switch）
- [x] 请求 Body 与签名参数：**无签名**，空对象 `{}` 即可
- [x] 成功响应样例（脱敏）：`{"code":0,"msg":"OK","data":{...}}`；幂等：已签到时 message 含「已签到」或 "repeat"，仍按成功处理（`checkin.rs:261-263`）
- 备用路径：国际版档位无签到接口（直接跳过）；CN 版另有旧路径 `/v2/plugin/...` 候选，仅 404 才回落（`checkin.rs:163-175`）

## 状态查询接口
- [x] 存在：`POST https://www.codebuddy.cn/v2/billing/meter/checkin-activity-status`（Body `{}`）
  - 2026-09-23 本机实测 200：
    `{"code":0,"msg":"OK","data":{"active":true,"today_checked_in":true,"streak_days":8,"daily_credit":100,"today_credit":100,"checkin_dates":["2026-09-23",...]}}`
  - 判定字段：`data.today_checked_in`（bool）；`data.active`=false 表示活动未开启
  - 旧路径回落候选：`/billing/meter/checkin-activity-status`（同样 200）

## 本机登录态
- [x] token/session 存储路径：`%LOCALAPPDATA%\CodeBuddyExtension\Data\Public\auth\workbuddy-desktop.info`（明文 JSON；同目录带时间戳后缀的文件是历史备份）
- [x] 加密方式：**明文**。`auth.accessToken` 为 JWT（约 1.3KB），`auth.refreshToken` 同样明文。（workbuddy 5.6+ 有 `$wbEncrypted` 信封变体，本机版本不涉及）
- [x] 提取步骤（可复述）：
  1. 读上述路径 JSON
  2. 取 `auth.accessToken`、`auth.domain`、`auth.expiresAt`（毫秒）、`account.uid`
  3. `expiresAt` 过期时用 refresh 流程：`POST https://www.codebuddy.cn/v2/plugin/auth/token/refresh`，Body `{}`，鉴权头 + `X-Refresh-Token: <refreshToken>` + `X-Auth-Refresh-Source: plugin`（缺此头返回 invalid_grant 12153），响应 `data.{accessToken, refreshToken, expiresAt}`（`refresh.rs:16-36,76-134`）——SignDock 只在新内存副本中使用，**不回写**产品文件
  4. 手动兜底：用户粘贴 accessToken 亦可直接使用（同 header）

## 积分/资源接口（2026-09-23 实测，全部 200）
- 域名规则（credits.rs:470-488）：billing 资源接口挂在**账号 domain 站点**（本机 domain=www.workbuddy.cn → `https://www.workbuddy.cn/billing/meter/...`，无 /v2）；签到/刷新接口在 www.codebuddy.cn `/v2/...` 照常可用。codebuddy.cn 域上无 /v2 的资源路径 404。
- 余额 `POST {web}/billing/meter/get-user-resource-summary`，Body `{}`
  → `data.Packages[]: {PackageCode, CycleTotalCapacity:"7341"(字符串数), CycleRemainCapacity, CycleUsedCapacity, CapacityUnit:"credits", TotalCount}`；当前积分=Σ CycleRemainCapacity（实测 7731.57=7231.57+500 ✓）
- 当日消耗 `POST {web}/billing/meter/get-user-request-usage`
  → Body `{"startTime":"YYYY-MM-DD 00:00:00","endTime":"YYYY-MM-DD HH:MM:SS","pageNum":1,"pageSize":N}`（official_usage.rs:485-490，官方用 3000）
  → `data:{total, data:[{credit, model, client, requestTime,...}]}`；当日消耗=Σ credit（可分页，total>已采数时 pageNum+1）
- 到期包 `POST {web}/billing/meter/get-user-resource-{paid|free}-packages`
  → Body `{PageNumber:1, PageSize:200, Status:[0,3], PackageCodes:[...], free 另带 SlicePeriodStartTime/EndTime 当日窗口}`；**PackageCodes 必填**（缺→ code 10001）；码表硬编码自 credits.rs:38-60（付费 8 码/免费 11 码）
  → `data.Accounts[]:{PackageName, CycleEndTime:"2026-09-24 10:32:43", DeductionEndTime:<ms 数字>, ExpiredTime, CycleCapacityRemainPrecise:"..."}`
  - 到期时间解析规则（credits.rs:194-215）：deduction(DeductionEndTime|ExpiredTime) 与 cycle(CycleEndTime) 并存且 deduction-cycle>365 天 → 用 cycle（长期占位如 2049/2033）；结果距今>730 天视为长期有效（None）
  - 时间值兼容毫秒数/秒数/RFC3339/"YYYY-MM-DD HH:MM:SS"（本地时区）（credits.rs:78-114）
  - 当日到期 = resolve 后日期==今天，剩余量取 CycleCapacityRemainPrecise|CycleCapacityRemain 求和
- 数值字段既有 number 也有数字字符串 → 统一 parse_f64 兜底

## 风控观察
- [x] 无验证码、无设备绑定、无请求签名；鉴权仅 Bearer。签到接口幂等，重复提交返回「已签到」
- 随机延迟仍有必要（避免每日固定秒级特征）；刷新失败应停止重试（workbuddy-switch 用 `needs_relogin` 死标记，`refresh.rs`）

## 结论
- [x] 可行模式：**自动签到**（凭证自动提取 + 无签名接口 + 幂等）
- 未决问题：
  - `$wbEncrypted` 信封 token 的客户端版本出现后，本地无法直接取明文 JWT → 届时或需借道产品本地代理，或降级「仅提醒」
  - 产品重装/登出后 auth 文件消失 → AuthExpired 路径已覆盖（提示需登录产品）
