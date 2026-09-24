# SignDock 设计文档

日期：2026-09-22
状态：已与用户逐节确认

## 1. 目标与定位

SignDock 是一个 Windows 本地托盘工具，统一管理多个 AI Agent 产品（workbuddy、Trae、Qoder、秒哒）的每日签到：

- 每产品可独立配置为 **自动签到** 或 **仅提醒** 两种模式
- 凭证只走"自动获取"：本机登录态直读（workbuddy/trae/qoder）、产品官方登录拿一次并自动续期（WorkBuddy OAuth），或 SignDock 自有登录窗口取一次会话（秒哒）。手工粘贴 token 这条路径已整体删除——它会压住会自动续期的凭据，界面上也没有入口，等于一份无人管理的过期凭证
- 仅操作本机已登录的个人账号；不绕过验证码与风控，触发即转人工
- 自用起步，形态与文档按可开源标准维护

与 traemate、workbuddy-switch 等单产品工具的差异：**多产品整合 + 统一调度/通知/历史**。

## 2. 技术选型

| 决策 | 选择 | 理由 |
|---|---|---|
| 应用框架 | Tauri 2（Rust 核心 + Web UI） | 体积小、原生托盘/通知、开源认可度高 |
| 形态 | 系统托盘常驻 + 简单设置窗口 | 签到场景是"无人值守 + 偶尔看一眼" |
| 适配器架构 | Rust 内置代码适配器（trait 实现） | 签到接口需逆向，必然逐家写代码；代码方案可处理签名/多步流程/DPAPI，最直接 |
| 存储 | SQLite | 签到历史 + 配置 + token 缓存，单文件免部署 |
| HTTP | reqwest（客户端可注入） | 异步生态标准，便于 mock |

## 3. 总体架构

```
SignDock (Tauri 2, Windows)
├── Rust 后端（src-tauri/）
│   ├── scheduler    — tokio 定时任务：每产品独立触发时间 + 随机延迟 ±10min + 失败重试
│   ├── adapters/    — ProductAdapter trait 及各产品实现（含秒哒：无 claim 接口，改为只读访问 + 流水校验）
│   ├── secret       — SignDock 自有凭据的封存层：明文只在内存，落盘走 DPAPI（当前用户）
│   ├── store        — SQLite：签到历史、产品配置
│   └── tray         — 托盘图标 + 右键菜单（立即签到/打开设置/退出）+ Windows toast 通知
└── Web 前端（src/）
    └── 设置窗口：每产品一张卡片（启用开关、模式、时间、凭证状态、今日结果、历史）
```

前后端通过 Tauri command 通信；所有网络请求与凭证处理只发生在 Rust 侧。

### 核心接口

```rust
#[async_trait]
trait ProductAdapter {
    fn id(&self) -> String;                        // "workbuddy" | "trae" | "qoder" | "miaoda"
    async fn query_sign_status(&self) -> Result<SignStatus, AdapterError>;
    async fn sign_in(&self) -> Result<SignOutcome, AdapterError>;
    async fn fetch_credits(&self) -> Result<CreditsSnapshot, AdapterError>;  // 默认：暂不支持
    async fn account_label(&self) -> Result<String, AdapterError>;           // 默认：暂不支持
}
```

凭证由适配器自备（本机登录态 / SignDock 自有的封存凭据 / 产品官方登录），命令签名里**没有 token 参数**：
一旦外部能传进来，手工粘贴的过期 token 就会压住会自动续期的凭据。原设计的 `CredentialSource` trait 因此不存在——
取凭证是每个适配器自己的私有函数，不是一层公共抽象。

`MockAdapter`（wiremock 假服务）实现同一 trait，用于全链路测试。

## 4. 数据流：单次签到

```
到点触发 → 查 store：今天该模式已成功？→ 是：跳过
  → adapter.query_sign_status()（支持则先查再签，防重复）
  → adapter.get_credential()：本机登录态优先，缺失则走产品官方登录取一次并自动续期
  → 模式=仅提醒 → 弹 toast"该签到了"（附打开产品入口），结束
  → 模式=自动   → adapter.sign_in() → 结果写 store → toast 通知结果
  → 失败按错误类别处理（见下表）
```

## 5. 错误处理

| 错误 | 处理 |
|---|---|
| 网络失败 | 重试 2 次、间隔 5min，静默 |
| 401 / token 过期 | 先尝试重新本机提取；仍失败 → 通知提醒去产品登录/走一次官方登录，不重试 |
| 接口结构变更（厂商更新） | 原始响应写日志，通知"适配器可能失效" |
| 风控 / 验证码 | **不绕过**，通知转人工签到 |

产品级配置项：模式（自动/仅提醒/关闭）、签到时间、重试次数与间隔。凭证不在配置项里——适配器自备。

## 6. 测试策略

- 适配器 + MockAdapter + wiremock 本地假服务：覆盖成功 / 401 / 接口变更 / 风控四类响应
- 调度器：时钟注入，纯逻辑单元测试（不 sleep 真实时间）
- HTTP 客户端可注入 trait object，真实网络仅冒烟使用
- 本机凭证提取无法单测：适配器里以「路径注入」隔离（读凭据的函数收 `&Path`），真机冒烟在阶段 2 验证

## 7. 分阶段实施

1. **阶段 0：抓包侦察（spike，产出文档）** — 对三家各抓一次完整签到流程：签到接口（URL/headers/body/签名参数）、状态查询接口、token 本机存储位置与加密方式。产出 `docs/recon/<product>.md`。某家无法完成侦察（如强制验证码）→ 该家降级为"仅提醒"。
2. **阶段 1：骨架** — 托盘 + 调度器 + SQLite + 通知 + MockAdapter 全链路跑通。
3. **阶段 2：首个真实适配器** — workbuddy（参考资料最多）+ 凭证提取，真机验证。
4. **阶段 3：Trae、Qoder 适配器 + 设置界面完善**。
5. **阶段 4：秒哒适配器 + 凭据封存**（见 §8）——新增 `secret.rs`（DPAPI 信封），SignDock 自有登录窗口取一次会话。

每阶段结束有可运行产物；阶段 0 的侦察结论可能修订本 spec 的适配器接口细节。

## 8. 增补（2026-09-25）：秒哒 —— 第一个"没有领取接口"的产品

首批三家都有明确的 claim 端点，秒哒没有：它的「登录赠送 100 秒点/天」由服务端记录的**当日登录**事件触发。设计上由此分出三条与前三家不同的决定：

1. **「签到」= 一次带会话的只读 GET**。用产品自己前端也在用的 `score/query_recharge`，不伪造签名、不模拟点按钮。2026-09-25 02:36 实测判定：这样一次请求就算当日登录，当场发放，不需要走 SSO 回跳。
2. **成功与否只认流水，不认回执**。`score/log_list` 里今天有没有一条 `channel=62` 的收入记录，就是唯一凭证（比 status 接口更硬：回执成功而没发放，是真实存在的情形——免费版每月最多领 7 天）。判不出"今天到底发过没有"时宁可报失败，也不报成功。
3. **凭据来自 SignDock 自有的 WebView2 profile**。用户在设置页点一次「打开登录窗口」，SignDock 开一个自己的窗口装秒哒站点（专属 `data_directory`，绝不与用户自己的 Edge/Chrome 共用 profile）；登录完成后由用户**再点一次**「我登录好了，取会话」，此时才用 `WebviewWindow::cookies_for_url` 取走整包 cookie（含 HttpOnly），交给服务端验一次货再 DPAPI 封存。绝不读用户浏览器的 cookie 库，也不再需要 MITM 代理那条已否决的路。
   - **勘误（2026-09-25 真机验证）**：原先写的是「取 cookie 的命令改成 async 就能躲开 wry#583 的死锁」——**这条是错的**。async 只是把等待从主线程挪到工作线程，`wait_with_pump` 那层嵌套消息泵**仍然跑在主线程**；回调一旦不返回，tauri 的事件循环再也不回来，表现是登录窗口白屏、X 键与托盘全部失效，只能强杀进程。
   - 由此得到的设计约束不是"怎么调"而是**调几次**：harvest 必须是用户的一次显式点击，**绝不允许定时轮询**（轮询等于把命中率不确定的死锁挂在后台反复触发）。前端同时用 `windowGone` 区分"窗口已关，去重开"和"还没登录，再等等"。
   - 那个登录窗口跑的是厂商的页面，capabilities 里不给它任何 SignDock 命令权限。

产品适配器的接口没有为此改动——`query_sign_status` 内部换成读流水、`sign_in` 内部换成发只读请求，引擎与调度器一律照旧。

## 9. 风险与边界（开源时写入 README）

- 签到接口非公开 API，厂商更新即失效，本项目不做反爬对抗
- 仅用于本人本机本人账号；多账号批量刷取不在设计范围内
- 触发风控即停止自动行为，转人工
- 开源协议建议 MIT，附免责条款
