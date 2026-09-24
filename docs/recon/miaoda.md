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
  - **不做轮询式自动 harvest**：`cookies_for_url` 在 wry 里是主线程嵌套消息泵，回调不返回就死锁整个进程（wry#583，实测白屏且窗口无法关闭）。见 `docs/superpowers/specs/2026-09-22-signdock-design.md` §8 勘误
  - 产品的 cookie 存储（别的浏览器 profile）从不读取
- [x] 曾评估并否决的官方设备码路线：`GET /api/v1/access/cli-authorization/{userCode}` 轮询 + `POST .../approve`（`@lark-apaas/miaoda-cli` 在用），但 npm tarball 里没有设备码**创建**端点，需真机跑一次 CLI 才能抓到 → 未取得，走 web 会话

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
