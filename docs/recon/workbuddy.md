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

## WorkBuddy 5.6 凭据加密（2026-09-23 实测，破坏性变更）
- 本机 15:21 自动升级 5.6 → 16:18 重写 `workbuddy-desktop.info`：`auth.accessToken`/`refreshToken`/`account.nickname`/`phoneNumber` 全部变成
  `{"$wbEncrypted":1,"envelope":"<base64(JSON)>"}`，明文 JSON = `{suite:1,keyId,nonce(12B b64),authTag(b64),ciphertext(b64)}` → AES-256-GCM 字段信封。
  仍明文：`expiresAt`/`refreshExpiresAt`/`domain`/`tokenType`/`account.uid`。
- 密钥链路（读 `resources/app.asar` + `app.asar.unpacked/cli/dist/codebuddy-headless.js` 得到）：
  字段信封 keyId = **构建期对称密钥**（本机实测 `9127dea1b44020a7`，`sha256(key)[0..16]`），由产品**原生模块**在运行时提供
  （JS 里只有一个 `getKey()` 回调，全 bundle 无 44 字符 base64 密钥字面量）；`~/.workbuddy/keyblob` 里的 32B 主密钥也是被同一把 key 包裹（slot `static-v1`）。
  帧格式常量：`WB-AAD\0` / `WB-WRAP\0`，framing magic `WBEF1`(file)/`WBEV1`(field)。
- 竞品同样被打断：`.wb-switch/accounts.json` 的明文 `access_token` 停在升级前 09:00，其 `hook.cmd` 走的是往 `.codebuddy/settings.json` 注入 Stop hook（**写产品文件**，SignDock 不做）。
- **SignDock 决策：不逆向厂商凭据保护。** 抠出的密钥对全量安装通用，开源后即等于公开发布凭据解密工具（用户在意 ToS 合规）。
  改为：检测到 `$wbEncrypted` → `AuthExpiredMsg`，文案指向「登录并获取 token」按钮。
  ~~手动粘贴 + 55 天一次重贴可接受~~ —— 该判断已被下一节推翻：竞品根本不走本机文件，正确解法是官方 OAuth 登录。

## 「抓一次 token」代理路线（2026-09-23 实测，结论：不可行）
- 手段：本地 CONNECT 隧道代理（纯 TCP、不解 TLS）+ 临时系统代理；`--proxy-server` 只作用于 WorkBuddy 单进程；以及「系统代理开启期间重启 WorkBuddy」逼它重新解析代理。
- 实测：
  1. 系统代理窗口内抓到 11+ 条 `CONNECT`（deepseek / `openapi.qoder.com.cn` / volces / google / xunlei…）→ Chromium 系应用**确实**遵循系统代理，但**没有一条来自 WorkBuddy**。
  2. 用 `--proxy-server=http://127.0.0.1:<port> --ignore-certificate-errors` 冷启动 WorkBuddy，用户在其中发一条消息 → **0 条 `CONNECT`**；同时刻 `Get-NetTCPConnection` 显示 WorkBuddy **直连** `111.2.133.71:443`（正是 `www.codebuddy.cn` 的解析地址之一）。
  3. 系统代理 + 临时根 CA 全开时重启 WorkBuddy 并发消息 → 窗口内 84 条 `CONNECT` 全部来自别的应用，WorkBuddy **0 条**（唯一那条 codebuddy 是自测客户端）。窗口内出现的 `m.uumit.com` 经查不属于 WorkBuddy（`app.asar` 与 headless bundle 内 0 命中）。
  4. 证书链本身不是障碍：临时自签 CA 经 `certutil -addstore -user Root` 静默导入（无 UI 提示），其签出的叶子证书被 Windows 校验器接受。坑：`Import-Certificate` 在 `-NonInteractive` 下被拒（「此操作不允许使用 UI」）；`certutil -delstore` 删除时会弹交互确认并卡死，删除只能走 `Remove-Item Cert:\CurrentUser\Root\<thumb>`；叶子必须带 AKI、CA 必须置 `cert_sign`，否则 `verify_code=85 Missing Authority Key Identifier`。
- **判定：WorkBuddy 的 API 请求不走任何代理层**（Electron 主进程 / 原生网络栈，实测对系统代理与 `--proxy-server` 双双无感；`resources/qimei.dll` 等原生组件亦在位）。要拦只能上网络层驱动（WinDivert/LSP）——SignDock 不做驱动级拦截，此路终止，保持手动粘贴。
- 留存仪器：`scripts/probe_proxy.py`（判定某产品是否走系统代理，纯隧道、自动还原）、`scripts/spike_mitm.py`（白名单域解 TLS，只打印头名与长度/sha256 前 8 位，绝不打印取值）。**Qoder 值得复用**：两次窗口都出现过 `CONNECT openapi.qoder.com.cn:443`。
- 每次探测后均复核：`ProxyEnable=0`、`Cert:\CurrentUser\Root` 无 SignDock 残留、WorkBuddy 以无开关方式重新拉起。

## 官方 OAuth 登录（2026-09-24 实测，这才是竞品的真实取 token 方式）
- 前面两节的弯路（读本机文件 / 代理抓包）都不是竞品的做法。**workbuddy-switch 从来不读产品登录态，它自己走官方 OAuth 登录**
  （`crates/wb-switch-core/src/modules/oauth.rs`），所以 5.6 的字段加密对它毫无影响，也不需要用户粘贴任何东西。
- 三接口（国内版；`platform=workbuddy`，前缀 `/v2/plugin`，无需 client_id/client_secret）：
  1. `POST {base}/v2/plugin/auth/state?platform=workbuddy`（body `{}`）→ `data.state` + `data.authUrl`
     （实测 authUrl 域名就是 `www.codebuddy.cn`，即官方登录页；60 分钟有效）。state 缺失即视为接口变更。
  2. 用户在浏览器里登录 → `GET {base}/v2/plugin/auth/token?state=` → `data.accessToken`/`refreshToken`/`expiresAt|expiresIn`/`domain`；
     未登录完成时 `data` 为空 → 继续轮询（竞品轮询间隔 2s、超时 600s）。
  3. `GET {base}/v2/plugin/login/account?state=`（`Authorization: Bearer <accessToken>`，有 domain 时带 `X-Domain`）→ `uid`/`nickname`/`email`。
- **SignDock 已按此实现**（`adapter/workbuddy.rs` 的 `oauth_start`/`oauth_poll` + 命令 `wb_oauth_login`/`wb_oauth_status`）：
  前端一个「登录并获取 token」按钮，Rust 侧用 `tauri-plugin-opener` 打开官方登录页，前端 2s 轮询，600s 超时。
  取到的凭据写入 **SignDock 自有文件** `%APPDATA%\com.signdock.app\workbuddy-cred.json`（与 signdock.db 同目录），
  `resolve_cred` 优先级：手动 token > 自有文件 > 产品 auth 文件；自有文件临期刷新后**回写自身**（回写自有文件不违反「绝不回写产品文件」），
  所以 55 天重贴的问题一并消失。token 只落盘，绝不经过 IPC 回传（命令只回 `done`/`nickname`）。
- 合规口径：这是产品**公开提供**的登录接口（竞品在用、URL 就是官方登录页），用户登录自己的账号，不涉及逆向、不涉及绕过风控 ——
  比代理 MITM 干净得多。代理路线（上一节）保留为已否决的历史记录，Qoder 侦察时仪器仍可复用。

## 结论
- [x] 可行模式：**自动签到**（官方 OAuth 登录取凭证 + 无签名接口 + 幂等）；5.6 加密对本工具无影响（见上节）
- 未决问题：
  - 自有凭据的 `expiresAt` 目前只在临期 24h 内被动刷新；到期前无任何主动预警 → 待做：读 JWT/字段 `exp` 提前提示
  - 产品重装/登出后 auth 文件消失 → AuthExpired 路径已覆盖（文案指向「登录并获取 token」按钮）
