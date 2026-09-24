# miaoda（百度秒哒）签到侦察

- 日期 / 形态：2026-09-24 → 09-25，`https://www.miaoda.cn`，**纯 web 无客户端**，货币叫「秒点」
- 侦察人：SignDock 会话内实测（真账号、全程只读；探测脚本留在 `Temp/`，gitignore）
- 官方口径：<https://cloud.baidu.com/doc/MIAODA/s/Pmih6khzs>「**每日登录**获得 100 秒点（仅属个人空间）」

## 为什么它和前三个产品不是一类
- [x] **没有签到/领取接口**：公开入口 bundle（`App.*.js`，2.2MB，CDN 可直取）里 `签到` 0 命中、`积分` 0 命中，`领取` 只出现在一次性「秒点资源礼包」。侧栏的「通关任务 领秒点！」是新手任务（`GET /api/v1/newbie_task/list`），**不是每日发放**
- [x] 日常发放由**服务端记录的「当日登录」事件**触发 → 「POST 一个签到接口」这条路不存在，别再找

## 每日发放的判据（比 status 接口更硬：流水本身就是凭证）
- [x] 收入流水 `POST /api/miaoda/score/log_list`，Body 带 `logType`：**2=收入，1=支出**（0/3 该账号 count=0）
- [x] 「登录赠送」= **`channel == 62`、`scoreCnt = +100`、每天最多一条**
  - 实测轨迹：09-18 注册(ch63,+200) → 09-22 11:41:59 → 09-24 11:14:51，**09-23 无记录**（那天没登录）⇒ 不登录就整天没有这 100 秒点，发放时刻 = 当天首次登录时刻，不是零点批量发
  - **幂等**：09-24 用户 11:14 自己浏览器登录过，SignDock 20:0x 再次登录 ⇒ 没有第二条 ch62。重复访问无害
- [x] 分页：Body `{"logType":2,"pageNum":<从 1 起>,"pageSize":50}`（适配器最多翻 6 页，短页即停）
- [x] 响应统一 `{status, msg, data}`，**`status == 0` 才是成功**；非 0 是业务失败（报 `Business`），不要误判成接口变更
- [x] 时间字段两种形态共存：`createdAt` 是 naive 本地（`2026-09-25T02:36:31`），`expirationTime` 带偏移（`2026-10-01T23:59:59.999+08:00`）→ 归日期时两种都要能吃

## 判定「带 cookie 的只读 GET 即算当日登录」（2026-09-25 02:36 时序实测）
- [x] 02:36:31 用前一天 20:0x 登录留下的会话打一次**纯只读** `GET /api/miaoda/score/query_recharge` → 200；紧接着 `log_list(logType=2)` 出现全新一条 `登录赠送 ch=62 +100 createdAt=2026-09-25T02:36:31`
- [x] 余额同步互证：`monRemainingScore` 209 → **309**，`dayRemainingScore = 100.0`
- [x] ⇒ **不需要**每天真起一个 WebView2 去加载页面，后台一个 GET 就够（原退路作废）

## 余额/积分接口
- [x] `GET /api/miaoda/score/query_recharge` → `data`：`monRemainingScore`（**月剩余**，就是界面侧栏那个数）、`dayRemainingScore`（当日剩余）、`monLimitScore`、`dayLimitScore`、`expiryDetail{available,expirationTime}`（最先到期的一批）
  - 别把 `dayRemainingScore` 与 `monRemainingScore` 当同一个数（口径互证：侧栏显示 209 == `monRemainingScore`）
  - 免费档对发放有上限 → 访问成功但当天没有 ch62 时，报 `Failed` 并给「可能本月登录赠送已达上限，或接口已变更」的文案，不要谎报成功
- [x] **当日消耗无汇总字段** → 从 `logType=1` 的流水按当天聚合（`today_consumed`）
- [x] 辅助（未采用）：`/api/miaoda/score/query_channel`、导出 `score/export_log_list`

## 凭据形态与取得方式
- [x] 登录链：首页「登录」→ `login.bce.baidu.com` → 百度智能云 IAM OAuth2 code flow（`client_id=db7e162f…`，`redirect_uri=https://www.miaoda.cn/oauth2/callback/iam`）→ 回跳落 cookie。**绝不代填账号密码，遇验证码转人工**
- [x] 会话 cookie 是 **HttpOnly**：`document.cookie` 只能看到 `md_device_token` 与统计类 cookie → 适配器**整包转发 cookie jar**，不区分哪个名字是凭据，也**绝不**把 cookie 值读进前端/日志
- [x] 唯一合法来源：SignDock 自有的 WebView2 窗口里用户**一次性**登录，由用户点「我登录好了，取会话」时才 harvest 一次 → 封存到 `%APPDATA%\com.signdock.app\miaoda-cred.json`（`secret.rs` 的 DPAPI `SDP1:` 信封）
  - **不做轮询式自动 harvest**：没有理由轮询——登录是否完成只有用户自己知道，定时器只会把一次宿主消息泵往返挂在后台反复戳。实测（09-25）harvest 本身是 async 命令，单次调用 0.2s 返回，不存在"一调就锁死"。（真正锁死过进程的是**建窗口**那条同步命令，见 §8 勘误）
  - 产品的 cookie 存储（别的浏览器 profile）从不读取
- [x] **官方设备授权流（OAuth2 Device Authorization Grant）已实机走通到拿票，但业务接口不认 → 路线否决**。09-25 05:39–05:45 完整实测：
  - `POST /oauth2/device/authorization`（form `client_id=workbuddy-cli&scope=profile:read apps:read apps:write`）→ 200，回 `device_code` / `user_code=MDAO-xxxx-xxxx` / `verification_uri=https://www.miaoda.cn/device/activate` / `expires_in=600`。`client_id=miaoda_cli` 回 `invalid_client`，只有 `workbuddy-cli` 登记在册（秒哒与 WorkBuddy 共用一套 OAuth 基建）
  - 用户在**自己的浏览器**里打开 `verification_uri_complete` 点确认 → `POST /oauth2/token`（`grant_type=urn:ietf:params:oauth:grant-type:device_code`）→ `Bearer` access_token（3600s）+ **refresh_token**，scope 原样回显。轮询侧 `authorization_pending` 正常，`slow_down` 需自行加间隔
  - **但这张票在业务端点上一律 401**：`/api/miaoda/score/query_recharge`、`/api/v1/score/query_recharge`、`/api/v1/app/list` 全拒，回话 `未经许可的请求`。判别依据：故意塞一个垃圾 Bearer 得到的是 `IAM accesskey 校验失败`，真票得到的是 `未经许可的请求` —— 说明票本身被百度智能云 IAM 认下来了（响应带 `x-bce-request-id`），是路由侧不给权限，不是票无效
  - 排除"我们姿势不对"：直接 import 厂商包自己的 `src/api.js`（只依赖 `node:crypto`，无需 install）调 `queryPoints` / `listApps`，带上它写死的 `x-space-id: personal` 与 `User-Agent: miaoda-cli/workbuddy-1.0.0`，**同样 401**。补 `Origin`/`Referer`/浏览器 UA 也一样
  - 厂商 CLI 里 `request-device-code.js` 的注释自己就写着「设备授权端点（占位，需与平台确认实际路径）」，且沙箱内走的是 WorkBuddy 连接器另发的一张票（`WORKBUDDY_TOKEN_URL_MIAODA_APP`）—— 独立客户端这条设备码路上服务端没开权限
  - **结论：秒哒没有 WorkBuddy 那种零输入路。** web 会话 cookie（SignDock 自有 WebView2 profile 里一次性登录）仍是唯一可用来源
- [x] 订正：本条此前写作「`@lark-apaas/miaoda-cli` 里没有创建设备码端点」——**那个包是飞书「妙搭」（`duanlikang@bytedance.com`），与百度「秒哒」只是撞名**，整个包里 `cli-authorization` 一次都没出现过。真正的官方 CLI 是 npm **`miaoda-cli`**（发布者 `fex@baidu.com`，描述直写「秒哒 (Miaoda) Node.js CLI」）。当时的否决建立在一个错误的包上，这次换正确的包重做才有上面那条实测

## 风控观察
- [x] 401 体是 `{"code":"server.error","msg":"…未知的鉴权方式。。"}` —— **这就是会话失效**，映射 `AuthExpired` 转人工重登；不要误判成 `SchemaChanged`，也不要自动重放凭据
  - 反面证据：干净无凭据 profile 的导航 + `credentials:'include'` fetch 全被同一个 401 挡掉 → 401 与「会话失效」同一签名
- [x] 会话寿命：09-24 20:0x 登录 → 09-25 02:36 仍有效（≥6.5h），**未证明永久续期** → 必须按会过期来设计
- [x] 无验证码、无请求签名（bundle 里除 axios 默认值外没有任何 bearer 注入）

## 结论
- [x] 可行模式：**自动登录（只读 GET）+ 流水校验**，SignDock 自有凭据，零日常手工
- 未决问题：
  - 免费档「登录赠送」的月上限具体数值未观测 → 只能靠 `Failed` 文案提示，不猜阈值
  - 会话是否被访问自动续期未证明 → 若长期不需要重登则更省事，但不作为设计前提

---
参考：探测脚本与真账号流水只留在 `Temp/`（一次性仪器，只打印键名/日期/数量级）。
