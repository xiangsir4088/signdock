# SignDock

基于 Tauri 2 + Rust 的 Windows 托盘小工具，用于每日自动领取本机关注的多款开发者产品的免费额度（WorkBuddy / Trae / Qoder / 秒哒）。后台按配置时间触发，结果通过系统通知送达，历史记录保存在本地 SQLite。**单用户、单账号、单机**。

[![License: MIT](https://img.shields.io/badge/License-MIT-blue.svg)](./LICENSE)
[![CI](https://github.com/SignDock/signdock/actions/workflows/test.yml/badge.svg)](https://github.com/SignDock/signdock/actions/workflows/test.yml)
[![platform](https://img.shields.io/badge/platform-Windows%2010%2F11-lightgrey.svg)](#)

## 文档

- 📖 [使用指南](./docs/GUIDE.md) — 完整操作手册（安装、四款产品配置、故障排查、卸载）
- 🎬 [交互式教程](./docs/tutorial.html) — 分步骤可视化演示，可录屏成视频教程

## 功能

- 托盘常驻，右键菜单：打开设置 / 立即签到（全部）/ 退出
- 每个产品独立配置：模式（关闭 / 仅提醒 / 自动签到）、每日触发时间、失败补偿次数与间隔
- 凭证全部自动获取，无需粘贴 token：Trae / Qoder 直接读本机登录态；WorkBuddy 5.6 起本机凭据被厂商加密，改由设置页「登录并获取 token」走一次官方 OAuth，之后自动续期；秒哒不读任何浏览器，由设置页开一个 SignDock 自有的登录窗口，登一次把那次会话的 cookie 封存下来
- 触发时间带 0~+10 分钟确定性 jitter（只往后推，避免整点集中请求，也不会把贴着窗口下界的时间拉回窗口外）
- 当日跑到「成功 / 已签 / 已提醒 / 需人工 / 终态失败」就不再重复触发；临时网络失败按补偿设置再试；滚动窗口产品（Qoder）在今日窗口开放前看到的「已领取」属于上一轮，只记一条「窗口未开」，不会把当天的执行堵掉
- 临时网络错误自动重试（出厂默认 2 次、间隔 5 分钟，界面可调）；凭证过期 / 接口变更不重试
- 设置窗口：按产品切换的 Tab 页、积分余额、最近 5 条签到日志、手动触发一次

## 构建

```bash
npm install          # 前端依赖（Vite + TypeScript）
npm run tauri dev    # 开发运行
npm run tauri build  # 打包安装包
```

需先安装 Rust 工具链与 [Tauri 2 prerequisites](https://tauri.app/start/prerequisites/)（Windows）。

## 配置说明

- **模式**（三态）：
  - `关闭`：不参与调度与批量签到；
  - `仅提醒`：到点只发通知，不请求签到接口；
  - `自动签到`：到点查询状态并自动执行签到。
- **时间**：每日触发时刻（HH:MM），实际触发叠加 0~+10 分钟 jitter（只推后，不提前）。
- **补偿重试**：临时网络失败后的补试次数与间隔，界面可配（出厂默认 2 次 / 5 分钟）；「今日窗口未开」只按这个间隔回头复查，不消耗补偿次数。

## 开发

```bash
npm run build                  # TypeScript 严格检查 + 打包
cd src-tauri && cargo test     # adapter + scheduler + store 单测（wiremock 驱动）
```

PR 清单见 [CONTRIBUTING](./CONTRIBUTING.md)；漏洞上报见 [SECURITY](./SECURITY.md)。

## 声明

**个人使用工具。使用后果由使用者自行承担。**

1. **非公开接口。** SignDock 调用的均为厂商**非公开**接口，厂商随时可能变更、下线或封禁，功能不保证持续可用。
2. **本地凭据。** SignDock 自有的登录凭据写在 `%APPDATA%\com.signdock.app\` 下的 `workbuddy-cred.json`（官方 OAuth 的 access / refresh token）与 `miaoda-cred.json`（秒哒会话 cookie），内容均为 **DPAPI（当前 Windows 用户）封存件**，不是明文；换用户或换机器都解不开，需要重新登录一次。Trae / Qoder 只读本机登录态，SignDock 不另存副本，也不主动轮换 refreshToken。
3. **秒哒没有「领取」接口。** 当天的 100 秒点由服务端记录的「当日登录」事件触发，SignDock 每天发的是产品自己也在用的一次只读请求。判定成功靠回查积分流水里那条 `channel=62` 的记录，而不是请求回执。**免费版每月最多领 7 天**，领完之后当天没有发放属正常现象。
4. **仅用于当前登录账号。** 自动请求可能触发厂商风控；一旦出现风控信号请立即停用。SignDock 不提供多账号、会话复制、账号切换、多开实例、批量签到功能，也不会在未来版本加入。
5. **厂商独立。** SignDock 为第三方项目，与 WorkBuddy（腾讯）、Trae（字节跳动）、Qoder、秒哒（百度）**无任何关联或背书**。相关商标归各自权利人所有。
6. **不继承社区同类工具的特性。** 生态内已存在若干同类开源项目；SignDock 不继承它们的特性、边界或声誉，也不构成对其的替代。

任何超出「个人、单账号自动化」范围的使用场景，本项目均不支持。相关 issue 与 PR 会被直接关闭。

## License

[MIT](./LICENSE)
