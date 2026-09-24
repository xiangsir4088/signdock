# SignDock 设计文档

日期：2026-09-22
状态：已与用户逐节确认

## 1. 目标与定位

SignDock 是一个 Windows 本地托盘工具，统一管理多个 AI Agent 产品（首批：workbuddy、Trae、Qoder）的每日签到：

- 每产品可独立配置为 **自动签到** 或 **仅提醒** 两种模式
- 凭证只走"自动获取"：本机登录态直读，或产品官方登录（WorkBuddy OAuth）拿一次并自动续期。手工粘贴 token 这条路径已整体删除——它会压住会自动续期的凭据，界面上也没有入口，等于一份无人管理的过期凭证
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
│   ├── adapters/    — ProductAdapter trait 及各产品实现
│   ├── credential   — CredentialSource trait：本机登录态 / 产品官方登录，两级自动获取
│   ├── store        — SQLite：签到历史、产品配置、token 缓存
│   └── tray         — 托盘图标 + 右键菜单（立即签到/打开设置/退出）+ Windows toast 通知
└── Web 前端（src/）
    └── 设置窗口：每产品一张卡片（启用开关、模式、时间、凭证状态、今日结果、历史）
```

前后端通过 Tauri command 通信；所有网络请求与凭证处理只发生在 Rust 侧。

### 核心接口

```rust
#[async_trait]
trait ProductAdapter {
    fn id(&self) -> &'static str;              // "workbuddy" | "trae" | "qoder"
    async fn extract_credential(&self) -> Result<Credential>; // 从本机登录态提取
    async fn query_sign_status(&self, c: &Credential) -> Result<SignStatus>; // 已签到查询
    async fn sign_in(&self, c: &Credential) -> Result<SignOutcome>; // 执行签到
}

#[async_trait]
trait CredentialSource {
    async fn load(&self) -> Option<Credential>; // 本机自动提取
    async fn refresh(&self) -> Result<Credential>; // 官方登录/续期后的写回
}
```

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
- 本机凭证提取无法单测：以 `CredentialSource` 隔离，阶段 2 真机冒烟验证

## 7. 分阶段实施

1. **阶段 0：抓包侦察（spike，产出文档）** — 对三家各抓一次完整签到流程：签到接口（URL/headers/body/签名参数）、状态查询接口、token 本机存储位置与加密方式。产出 `docs/recon/<product>.md`。某家无法完成侦察（如强制验证码）→ 该家降级为"仅提醒"。
2. **阶段 1：骨架** — 托盘 + 调度器 + SQLite + 通知 + MockAdapter 全链路跑通。
3. **阶段 2：首个真实适配器** — workbuddy（参考资料最多）+ 凭证提取，真机验证。
4. **阶段 3：Trae、Qoder 适配器 + 设置界面完善**。

每阶段结束有可运行产物；阶段 0 的侦察结论可能修订本 spec 的适配器接口细节。

## 8. 风险与边界（开源时写入 README）

- 签到接口非公开 API，厂商更新即失效，本项目不做反爬对抗
- 仅用于本人本机本人账号；多账号批量刷取不在设计范围内
- 触发风控即停止自动行为，转人工
- 开源协议建议 MIT，附免责条款
