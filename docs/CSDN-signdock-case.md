# 我写了个 Tauri 2 + Rust 的 Windows 托盘小工具，替我每天自动领 4 家 AI 产品的免费额度（开源）

> 项目地址：`https://github.com/xiangsir4088/signdock` ｜ License：MIT ｜ 绿色版约 4.6MB

作为每天要用好几家 AI 产品（WorkBuddy、Trae、Qoder、秒哒）的人，我最烦的一件事就是每天记着去领免费额度。忘一次少一天，一个月下来少几百积分，心疼。

于是我自己写了个小工具 **SignDock**：一个 Tauri 2 + Rust 的 Windows 托盘常驻程序，按我配置的时间自动把这几家的每日免费额度领了，领完系统通知告诉我，历史存本地 SQLite。全程不复制粘贴任何 token，免安装，解压即用。

这篇文章不讲工具怎么用，讲**我在这个过程中踩过的 Tauri 2 / Rust / Windows 的几个坑**，以及为什么我坚持把它做成「单账号、单机器、不碰多开批量」的边界克制型工具。都是真实踩过的，希望对你有帮助。

## 一、为什么选 Tauri 2 + Rust 而不是 Electron

写桌面工具，Electron 是大多数人的第一选择，但 SignDock 是个托盘常驻小工具，我反复权衡后选了 Tauri 2 + Rust，原因很直接：

| 维度 | Electron | Tauri 2 + Rust |
|---|---|---|
| 体积 | 80-150MB（带 Chromium） | 绿色版 4.6MB（用系统 WebView2） |
| 内存 | 每个窗口一个 Chromium 进程，常驻 200MB+ | 单进程 + 系统 WebView，常驻 ~80MB |
| 安全模型 | Node 直接暴露，攻击面大 | Rust 后端，前端通过 IPC 调命令 |
| 发布 | 单 .exe 几百 MB | 单 .exe ~13MB + 前端资源 zip |

SignDock 要 7×24 跑在托盘里，体积和内存是硬指标。Electron 常驻 200MB+ 内存对一台常年开着的开发机来说太浪费了，Tauri 用系统自带的 WebView2（Win10 2004+ / Win11 内置）能把这块砍掉。

另外 Rust 的 `tokio` 调度器天然适合做「每 60 秒 tick 一次、每个产品独立并发」的调度循环，比 Node 的事件循环更可控——这是后文要重点讲的。

## 二、整体架构：4 个产品共用一套引擎

SignDock 支持 4 款产品（WorkBuddy / Trae / Qoder / 秒哒），但我不想为每款产品写一套调度逻辑。核心设计是把「怎么领」抽象成 trait，调度引擎只认 trait：

```rust
// adapter/mod.rs
#[async_trait]
pub trait ProductAdapter: Send + Sync {
    fn id(&self) -> String;
    /// 查签到状态。Err = 网络/临时错误或凭证问题（可重试性由上层按变体判断）
    async fn query_sign_status(&self) -> Result<SignStatus, AdapterError>;
    /// 执行签到。Ok = 签到系统明确应答
    async fn sign_in(&self) -> Result<SignOutcome, AdapterError>;
    /// 积分快照（可选能力）
    async fn fetch_credits(&self) -> Result<CreditsSnapshot, AdapterError>;
}
```

四个适配器（`adapter/trae.rs` / `workbuddy.rs` / `qoder.rs` / `miaoda.rs`）各自实现这个 trait，调度引擎（`engine.rs`）只调 `query_sign_status` + `sign_in` 两个方法，**不关心产品差异**。加第 5 款产品只需要写一个新 adapter 文件，引擎、调度、存储、通知一行不用动。

调度循环在 `lib.rs` 里用 `tokio::time::interval` 实现，每 60 秒 tick 一次：

```rust
tauri::async_runtime::spawn(async move {
    let mut ticker = tokio::time::interval(Duration::from_secs(60));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Skip);
    loop {
        ticker.tick().await;
        for id in PRODUCT_IDS {
            let cfg = store.config(id)?;
            let today = store.today_summary(id, day_start)?;
            if !scheduler::is_due(now, id, &cfg, &today) { continue; }
            let Some(guard) = locks.acquire(id) else { continue };
            tauri::async_runtime::spawn(async move {
                let _guard = guard;
                let _ = engine::run_once(adapter, &cfg, &store, &notifier).await;
            });
        }
    }
});
```

这里有个关键细节：`ticker.set_missed_tick_behavior(MissedTickBehavior::Skip)`。如果某次 tick 里一个产品挂了 5 分钟（比如请求超时），ticker 默认会疯狂补发之前错过的 tick，把后面所有产品都拖住。`Skip` 行为确保「一次挂起的请求只影响它自己，不拖住整个调度循环」——这是无人值守工具必须守住的底线。


## 三、按产品的「执行权」互斥：为什么用原子标记而不是 Mutex

三路触发会同时打向同一个产品：
1. 调度循环每 60s tick 一次
2. 托盘菜单「立即签到（全部）」
3. 界面每个产品卡片的「立即执行一次」

三路如果都去查签到状态，同一个产品会被并发请求多次——重复签到、重复通知、数据竞态。我用了一把**按产品的原子执行权**：

```rust
// engine.rs
pub struct RunLocks {
    slots: Vec<(&'static str, Arc<AtomicBool>)>,
}

impl RunLocks {
    /// 取执行权；None = 该产品正在执行，调用方本轮直接跳过（绝不排队）。
    pub fn acquire(&self, id: &str) -> Option<RunGuard> {
        let flag = self.slots.iter()
            .find(|(name, _)| *name == id)
            .map(|(_, f)| f.clone())
            .unwrap_or_else(|| Arc::new(AtomicBool::new(false)));
        flag.compare_exchange(false, true,
            Ordering::AcqRel, Ordering::Acquire)
            .ok()
            .map(|_| RunGuard { flag })
    }
}

/// 持有即代表该产品正在执行；Drop 自动复位标记
impl Drop for RunGuard {
    fn drop(&mut self) {
        self.flag.store(false, Ordering::Release);
    }
}
```

为什么用 `AtomicBool` + `compare_exchange` 而不是 `Mutex`？因为**我宁可同一产品并发跑一次被跳过，也绝不让调度循环卡在一个挂死的请求后面**。如果用 `Mutex::lock()`，一旦 `run_once` 里某个产品的 HTTP 请求挂住（虽然 reqwest 有超时，但极端情况仍可能），整个调度循环就死了。原子标记 + `acquire` 返回 `None` 的设计意味着：抢不到就走开，下一轮 60s 后再看，**绝不阻塞别的产品的执行**。

`RunGuard` 的 `Drop` 自动复位是关键——哪怕 `run_once` 中途 panic，标记也会释放，不会留死锁。这就是为什么 `Cargo.toml` 里特意注释了「不用 `panic = "abort"`」：

```toml
# 不用 panic = "abort"：这个进程的目标是无人值守跑几天不被发现有问题。
# abort 下 RunGuard 的 Drop 与 store::lock 的中毒恢复都成了死代码，
# 任何一个后台任务 panic 都会让整个托盘当场消失（签到悄悄停摆）。
# 保留 unwind：坏掉的那个任务丢掉自己的结果，托盘继续跑。
```

## 四、调度器：临时失败 vs 终态失败，必须分开

签到请求失败有几种完全不同的语义，混在一起处理会出事故：

```rust
// adapter/mod.rs
pub enum AdapterError {
    #[error("网络错误: {0}")]
    Http(String),          // 传输层错误：连接失败、超时、5xx、限流 → 可重试
    #[error("凭证已过期")]
    AuthExpired,          // 凭证失效 → 重试无意义，转人工
    #[error("接口可能已变更: {0}")]
    SchemaChanged(String), // 接口改了 → 重试无意义
    #[error("{0}")]
    Business(String),      // 业务拒绝（额度、活动开关）→ 不重试
}
```

落库时，**只有 `Http` 类错误才记为 `retryable`**，其余记为 `failed`（终态）。这个区分直接决定了调度器下一轮是否补试：

```rust
// scheduler.rs
pub fn is_due(now, product_id, cfg, today) -> bool {
    // ...
    if last_outcome == OUTCOME_WINDOW_PENDING {
        // 「今日窗口未开」只等间隔，不消耗补偿额度
        return now >= last_at + cfg.retry_interval_min * 60;
    }
    if last_outcome != OUTCOME_RETRYABLE { return false; }
    // 已失败 retryable：看补偿额度和间隔
    today.retry_failures <= cfg.retry_times
        && now >= last_at + cfg.retry_interval_min * 60
}
```

出厂默认 2 次补偿、间隔 5 分钟。`retry_times=0` 就是「跑一次失败就停手，等用户处理」。

## 五、滚动窗口产品：「窗口未开」≠「今天处理完」

Qoder 的每日福利窗口**北京时间 10:00 才开**。如果你在 9:30 手动点了一次「立即执行」，此时看到的「已领取」其实是**上一轮**（昨晚到今早）的，不是今天的。

这是一个非常隐蔽的坑：如果调度器把「看到已领」直接记为 `alreadySigned`，那当天 10:05 的定时执行就会因为「今天已处理」而跳过——用户永远领不到今天的额度。

我引入了一个独立状态 `WindowPending`：

```rust
// engine.rs
if status == SignStatus::WindowPending {
    // 不通知、不落终态：到点以后调度器还会来看一次
    store.record_run(id, OUTCOME_WINDOW_PENDING, "今日窗口尚未开启").ok();
    return RunResult::WindowPending;
}
```

落库的 `outcome = "windowPending"` 是非终态。调度器看到它，**不消耗补偿额度、不阻止后续补试**，只是按间隔回头再看一眼。窗口开了之后（10:00 后），状态变成 `NotSigned`，调度器正常执行签到。

这个状态对应的单测也专门写了回归：

```rust
#[test]
fn manual_run_before_window_keeps_scheduled_run_alive() {
    let cfg = ProductConfig::base("qoder", Mode::Auto, "10:05");
    let s = summary(at(2026, 9, 22, 9, 30).timestamp(), OUTCOME_WINDOW_PENDING, 0);
    assert!(is_due(at(2026, 9, 22, 10, 20), "qoder", &cfg, &s),
        "窗口开了就得去领");
}
```

## 六、触发时间的确定性 jitter：只往后推，绝不拉回

四个产品如果都设 09:00，那每天 09:00 会同时打四家接口，容易被风控识别为「同一账号短时间密集请求」。我加了一个 **0~+10 分钟的确定性 jitter**：

```rust
// scheduler.rs
pub fn jitter_minutes(product_id: &str, day: NaiveDate) -> i64 {
    let mut h = DefaultHasher::new();
    product_id.hash(&mut h);
    day.hash(&mut h);
    (h.finish() % 11) as i64
}
```

注意两个设计决定：

1. **确定性，不是随机数**。同一个产品同一天 jitter 永远一样——方便调试（你设置 09:00，能准确算出它实际几点触发），也方便写单测。
2. **只取正向（0~+10），不做负向**。如果 jitter 是 -5 到 +10，那 Qoder 默认的 10:05 可能被拉到 10:00 之前，正好落在窗口开启前，命中的是上一轮。负向 jitter 在「窗口贴着下界」的场景下是个隐性杀手。

对应的回归测试覆盖 30 天：

```rust
#[test]
fn qoder_default_time_never_fires_before_window_opens() {
    let cfg = ProductConfig::base("qoder", Mode::Auto, "10:05");
    for d in 0..30 {
        let day = NaiveDate::from_ymd_opt(2026, 9, 1).unwrap() + Duration::days(d);
        let at_1000 = day.and_hms_opt(10, 0, 0).unwrap().and_local_timezone(Local).unwrap();
        assert!(!is_due(at_1000, "qoder", &cfg, &TodaySummary::default()),
            "{day} 10:00 不该到期");
    }
}
```

## 七、凭证管理：零输入 + DPAPI 封存

四款产品的凭证获取方式各不相同，我统一设计成「**调用方不传 token**」：

| 产品 | 凭证来源 | 机制 |
|---|---|---|
| Trae / Qoder | 读本机登录态 | 直接读产品自己存的 session，SignDock 不另存副本 |
| WorkBuddy | 官方 OAuth | 5.6 起厂商加密了本机凭据，改走一次官方 OAuth 登录轮询；token 存 DPAPI 封存文件 |
| 秒哒 | SignDock 自有登录窗口 | 开一个独立 WebView2 窗口，用户登一次，把 cookie 封存；不读用户浏览器 |

WorkBuddy 的 OAuth 这里踩过一个坑：轮询接口在**登录进行中**会返回 `code=11217 / msg="login ing..."`。我一开始把任何非零 code 都当硬失败，导致流程在拿到 token 前就中断了——用户看到的永远是「登录失败」，但网页端明明已经登了。

修法是把 11217 识别为「进行中」：

```rust
// adapter/workbuddy.rs
fn is_login_pending(code: i32, msg: &str) -> bool {
    code == 11217 || msg.contains("login ing") || msg.contains("pending")
}

// oauth_poll 非零 code 分支
Err(e) if is_login_pending(...) => Ok(None),  // 继续轮询，不当失败
```

前端收到 `Ok(None)` 就继续轮询，收到 `Ok(Some(cred))` 才把 token 写进 DPAPI 封存文件。整个链路没有一条路径能把用户粘贴的 token 送进请求——`get_credits` / `get_account` 两个命令连配置都不用读，全靠适配器自备凭证。

## 八、Tauri 单实例锁：任务栏点一下就开一排窗口的坑

这是最新修的（v0.1.2）。Tauri 2 **默认没有单实例锁**：Windows 下你每次点任务栏图标都会 `CreateProcess` 一个新进程，每个新进程跑一遍 `.setup()`、各建一个 `main` 窗口，于是任务栏就堆出一排 SignDock。

修法很简单，加官方插件：

```rust
// lib.rs
.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    } else {
        let _ = WebviewWindowBuilder::new(app, "main", WebviewUrl::default()).build();
    }
}))
```

第二次启动时插件检测到已有实例，新进程立刻退出，把事件转发给首个实例，由首个实例把 `main` 窗口拉回前台。

配合原来的 `CloseRequested → hide`（关窗=收进托盘，不销毁窗口）逻辑：窗口对象在进程里一直存在（`get_webview_window("main")` 查得到），只是不可见，单实例回调走的就是同一条路径。

## 九、SQLite 存储的几个设计决定

`store.rs` 里几个不太起眼但很关键的决定：

**1. 流水保留 30 天**：打开数据库时清一次老数据，窗口未开的复查每几分钟就落一行，没清理策略的表会一直长。

```rust
const RUN_RETENTION_SECS: i64 = 30 * 86_400;
// 打开时：
conn.execute("DELETE FROM sign_run WHERE at < ?1", params![cutoff])?;
```

**2. 按 (product_id, at) 建索引**：调度器每个 tick 都要查「今天这个产品跑过没有」，没索引就是每分钟一次全表扫。

**3. 老库迁移不静默吞错误**：

```rust
// config() 里
Err(rusqlite::Error::QueryReturnedNoRows) => {} // 查无此行 → 播种默认值
Err(e) => return Err(StoreError(e.to_string())), // 真读错 → 上报，绝不静默覆盖
```

如果吞掉真读错、回落到出厂默认值，等于把用户改过的配置**静默覆盖掉**。还有一个专门测试造出一行 `mode=NULL` 的坏数据，断言读不动时报错而不是播种覆盖。

**4. 老库的 `token` 列主动撤掉**：早期版本支持手工粘贴 token，后来整个功能删了。但数据库里那一列还躺着，里面是**真实凭证**。开源后等于别人库里留着你的明文 token，所以打开数据库时 `ALTER TABLE ... DROP COLUMN token` 一并清掉，配了回归测试。

## 十、边界克制：为什么我不做多账号 / 批量 / 会话复制

这是被问得最多的问题。GitHub issue 里有人要「多账号批量签到」，有人要「把 A 账号会话复制到 B 账号」。我全部拒绝，并写进了 README 声明：

> 仅用于当前登录账号。自动请求可能触发厂商风控；一旦出现风控信号请立即停用。SignDock 不提供多账号、会话复制、账号切换、多开实例、批量签到功能，也不会在未来版本加入。

原因很实际：
- 多账号 / 批量 = 高频请求同一厂商接口 = 风控必中
- 会话复制 = 把 A 的 session 打到 B 的归属上，这是厂商明确禁止的，出了事是你自己的事
- 我用的是**非公开接口**，随时可能变更或下线，功能不保证持续可用

工具的价值是「帮你把**自己的**免费额度领到手」，不是「帮你薅羊毛」。单账号、单机器、单实例，边界守住了这个工具才能活很久。

## 十一、踩坑速查表（我真实遇到的）

| 坑 | 现象 | 解法 |
|---|---|---|
| `--bundles app` 报错 | Tauri 2 Windows 无 portable target | 用 `--no-bundle` 出 exe，自己 zip 包绿色版 |
| schemars + indexmap E0107 | release 构建报 `IndexMap` 3 泛型 | Cargo resolver 2 按 build-context 分层，`[build-dependencies]` 也要加 `indexmap features=["std"]` |
| `cmd.exe` 被沙箱拦 | `Command blocked for security` | 改用 `npm run tauri build` 直接在 Bash 跑 |
| 旧 exe 被锁 | `PermissionError` 删不掉 | 先 `Stop-Process -Name SignDock -Force` 再打包 |
| 任务栏开一排窗口 | 每次点图标新起进程 | `tauri-plugin-single-instance` |
| WorkBuddy 登录卡 11217 | 永远「登录失败」 | 把 11217 识别为 pending，继续轮询 |
| `oo connector run` 报缺 `--action` | 旧写法 `github.<action>` | 改成 `github -a <action> -d @file` |
| Contents API 更新报缺 `sha` | 更新已存在文件必须带 sha | 先 `get_file_contents` 拿 sha |

## 十二、构建与发布

开发：

```bash
npm install
npm run tauri dev          # 开发模式（HMR）
cd src-tauri && cargo test # 单测（wiremock 驱动）
```

发布绿色版（免安装）：

```bash
npm run tauri build -- --no-bundle
# 产物 src-tauri/target/release/signdock.exe (~13MB)
# 配一个 README.txt，zip 压缩 → 绿色版 4.6MB
```

发布到 GitHub Release 我写了一套自动化脚本（`Temp/upload_v012.py`），一个脚本处理源码更新 + zip 上传 + 建 Release，带 sha 自动 fetch。后续发版直接改版本号重跑就行。

## 结尾

SignDock 的代码不长（Rust 后端约 2000 行，前端 1 个 Vue 页面），但把「无人值守跑几天不出事」这个目标做扎实，靠的是上面这些**边界克制的设计决定**：按产品的原子执行权不阻塞、临时/终态失败分开落库、窗口未开不堵死当天、确定性 jitter 只往后推、凭证零输入 + DPAPI、单实例锁、SQLite 不静默吞错。

工具本身是开源的（MIT），代码随便看。你如果有自己的「每日要记得去做」的几件重复事，这套「trait 抽象 + 按产品互斥 + 终态/临时态分开」的调度思路应该能直接搬。

仓库：`https://github.com/xiangsir4088/signdock`
下载绿色版：`https://github.com/xiangsir4088/signdock/releases`

⚠️ 声明：调用的是厂商非公开接口，随时可能变更或下线；仅限个人单账号自用，使用后果自负。与 WorkBuddy（腾讯）、Trae（字节）、Qoder、秒哒（百度）无任何关联或背书。
