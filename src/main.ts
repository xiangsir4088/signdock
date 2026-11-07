import { invoke } from "@tauri-apps/api/core";
import { openUrl } from "@tauri-apps/plugin-opener";

interface ProductConfig { productId: string; mode: "off" | "remind" | "auto"; timeOfDay: string; retryTimes: number; retryIntervalMin: number; }
interface RunRow { id: number; productId: string; at: string; outcome: string; detail: string; }
interface Overview { productId: string; config: ProductConfig; runs: RunRow[]; }
interface CreditsSnapshot { balance: number; todayUsed: number; expiringToday: number; expiringTomorrow: number; fetchedAtMs: number; }
interface OAuthSession { loginId: string; authUrl: string; expiresIn: number; }
interface OAuthStatus { done: boolean; nickname: string; }
interface MdProbe { done: boolean; windowGone: boolean; }
/** 豆包多账号：单个 profile 的今日状态（status: signed/notSigned/locked/notLoggedIn/error） */
interface DbAccountView { uid: string; label: string; status: string; detail: string; }
/** 豆包多账号：单个账号的签到结果（outcome: success/alreadySigned/failed/skipped） */
interface DbAccountResult { uid: string; label: string; outcome: string; detail: string; }

const DB_STATUS_LABEL: Record<string, string> = {
  signed: "已签", notSigned: "未签", locked: "锁定(退出客户端可读)", notLoggedIn: "未登录", error: "异常",
};
const DB_STATUS_CLASS: Record<string, string> = {
  signed: "ok", notSigned: "warn", locked: "warn", notLoggedIn: "bad", error: "bad",
};
const DB_OUTCOME_LABEL: Record<string, string> = {
  success: "成功", alreadySigned: "已签", failed: "失败", skipped: "跳过",
};

/** 与 Rust 的 RunResult 对应：无载荷的变体序列化成字符串，带载荷的序列化成单键对象。 */
type RunResult = "skipped" | "alreadySigned" | "reminded" | "windowPending"
  | { success: string } | { needManual: string } | { failed: string } | { failedRetryable: string };

const fmtCredits = (n: number) => n.toLocaleString("zh-CN", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const LOG_LIMIT = 5;

const OUTCOME_LABEL: Record<string, string> = {
  success: "成功", alreadySigned: "已签", reminded: "提醒", needManual: "需人工", failed: "失败",
  retryable: "临时失败", windowPending: "窗口未开",
};
const OUTCOME_CLASS: Record<string, string> = {
  success: "ok", alreadySigned: "ok", reminded: "warn", needManual: "warn", failed: "bad",
  retryable: "warn", windowPending: "warn",
};
const PRODUCT_LABEL: Record<string, string> = { workbuddy: "WorkBuddy", trae: "Trae", qoder: "Qoder", miaoda: "秒哒", doubao: "豆包", lingjing: "灵镜" };
const PRODUCT_HINT: Record<string, string> = {
  workbuddy: "点上方「登录并获取 token」，在弹出的官方登录页里登录一次即可；此后自动续期，无需再管。",
  trae: "自动读取本机 TRAE 登录态。凭证过期时打开 TRAE 让它自行刷新，再点「立即执行一次」。",
  qoder: "自动读取本机 Qoder 登录态（只读解密，不回写）。福利每天北京时间 10:00 刷新，签到时间请设在 10:00 之后。",
  miaoda: "秒哒没有领取接口：当天访问一次就发 100 秒点。点「打开登录窗口」在 SignDock 自己的窗口里登一次、再点「取会话」（不碰你的浏览器），之后每天由 SignDock 代为访问。免费版每月最多领 7 天，领完当天不会有发放，属正常。",
  doubao: "自动读取本机豆包登录态（只读解密，不回写），每天在豆包任务中心签到领 200 额度。首次提取需完全退出一次豆包客户端（运行中它的会话库被锁定）；之后客户端开着也能签。若提示登录失效，打开豆包确认登录后再完全退出一次即可。",
  lingjing: "灵镜用 JWT（token 在登录页的 localStorage，不是 cookie）。点「打开登录窗口」在 SignDock 自己的窗口里登一次、再点「取会话」（不碰你的浏览器），之后每天由 SignDock 代为签到领积分；token 过期会自动刷新续期。",
};

// 防注入：detail 来自服务端，插入 innerHTML 前必须转义。
function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

const creditsCache = new Map<string, CreditsSnapshot>();
type Account = { label: string } | { error: string };
const accountCache = new Map<string, Account>();
const dbAccountsCache = new Map<string, DbAccountView[]>();
let activeId: string | null = null;
let lastList: Overview[] = [];

// WorkBuddy 官方 OAuth 登录：状态存在模块作用域，render() 重绘后仍可恢复提示文本
const oauthNotes = new Map<string, string>();
const oauthUrls = new Map<string, string>();
const oauthRunning = new Set<string>();
// 秒哒登录窗口是否已经开过（决定「取会话」按钮出现与否）
const mdOpen = new Set<string>();
const sleep = (ms: number) => new Promise(r => setTimeout(r, ms));
/** oauth = 借道系统浏览器的官方登录；session = SignDock 自有的 WebView2 登录窗口 */
const LOGIN_KIND: Record<string, "oauth" | "session"> = { workbuddy: "oauth", miaoda: "session", lingjing: "session" };
/** session 型登录窗口对应的后端命令（灵镜读 localStorage，秒哒读 cookie，命令不同） */
const SESSION_LOGIN: Record<string, { open: string; probe: string }> = {
  miaoda: { open: "md_login_open", probe: "md_login_probe" },
  lingjing: { open: "lj_login_open", probe: "lj_login_probe" },
};

// 操作失败必须可见：invoke 抛错若只落在控制台，用户看到的只是按钮自己变了回来。
const errorNotes = new Map<string, string>();
function setErr(id: string, text: string) { errorNotes.set(id, text); }
function clearErr(id: string) { errorNotes.delete(id); }

// 执行结果：点「立即执行一次」之后必须有一句话说清发生了什么，否则界面只会自己变回原样
const runNotes = new Map<string, string>();
function runNoteHtml(id: string): string {
  const t = runNotes.get(id);
  return t ? `<p class="hint">${esc(t)}</p>` : "";
}
function runResultText(r: RunResult): string {
  if (typeof r === "string") {
    return r === "alreadySigned" ? "今日已签到，没有重复领取。"
      : r === "reminded" ? "只提醒模式：提醒已发出，未执行签到。"
      : r === "windowPending" ? "今日窗口还没开放，到点会自动再试一次。"
      : "该产品已关闭自动执行，本次跳过。";
  }
  if ("success" in r) return `签到成功：${r.success}`;
  if ("needManual" in r) return r.needManual;
  if ("failedRetryable" in r) return `临时失败，稍后按设置重试：${r.failedRetryable}`;
  return `失败：${r.failed}`;
}

function dbSignResultText(results: DbAccountResult[]): string {
  if (results.length === 0) return "未发现豆包登录账号。";
  return results.map(r => {
    const label = DB_OUTCOME_LABEL[r.outcome] ?? r.outcome;
    return `${r.label}：${label}${r.detail ? "（" + r.detail + "）" : ""}`;
  }).join("；");
}
function errHtml(id: string): string {
  const t = errorNotes.get(id);
  return t ? `<p class="error">${esc(t)}</p>` : "";
}

function setNote(id: string, text: string) {
  oauthNotes.set(id, text);
  const el = document.getElementById(`oauth-note-${id}`);
  if (el) el.textContent = text;
}

function oauthHtml(id: string): string {
  const kind = LOGIN_KIND[id];
  if (!kind) return "";
  const running = oauthRunning.has(id);
  const session = kind === "session";
  const opened = mdOpen.has(id);
  const btn = session
    ? `<button class="btn primary" data-act="md-open" data-id="${id}"
      ${running ? "disabled" : ""}>${opened ? "重新打开登录窗口" : "打开登录窗口"}</button>`
    : `<button class="btn primary" data-act="oauth" data-id="${id}"
      ${running ? "disabled" : ""}>${running ? "等待浏览器登录…" : "登录并获取 token"}</button>`;
  // 取会话是一次显式动作：读 WebView2 的 cookie 罐会把主线程卡住（见 src-tauri 里的注释），
  // 绝不能再拿定时器去反复戳它。
  const harvest = session && opened
    ? `<button class="btn ghost" data-act="md-probe" data-id="${id}"
      ${running ? "disabled" : ""}>${running ? "读取会话…" : "我登录好了，取会话"}</button>` : "";
  const reopen = !session && oauthUrls.has(id)
    ? `<button class="btn ghost" data-act="oauth-open" data-id="${id}">重新打开登录页</button>` : "";
  const idle = session
    ? "点左侧按钮：SignDock 会开一个自己的登录窗口，在里面登一次，回来点「取会话」封存。"
    : "无需粘贴 token：点左侧按钮，用官方登录页授权一次即可。";
  return `<div class="row">${btn}${harvest}${reopen}</div>
  <p class="hint" id="oauth-note-${id}">${esc(oauthNotes.get(id) ?? idle)}</p>`;
}

function applyTheme() {
  document.documentElement.classList.toggle("dark", matchMedia("(prefers-color-scheme: dark)").matches);
}
applyTheme();
matchMedia("(prefers-color-scheme: dark)").addEventListener("change", applyTheme);

function todayRun(runs: RunRow[]): RunRow | undefined {
  const d = new Date().toDateString();
  return runs.find(r => new Date(r.at).toDateString() === d);
}

function statusBadge(o: Overview): string {
  const r = todayRun(o.runs);
  if (!r) return `<span class="badge">今日未执行</span>`;
  return `<span class="badge ${OUTCOME_CLASS[r.outcome] ?? ""}">今日 ${OUTCOME_LABEL[r.outcome] ?? esc(r.outcome)}</span>`;
}

function creditsInner(c: CreditsSnapshot): string {
  return `<span><span class="k">余额</span><br><span class="v">${fmtCredits(c.balance)}</span></span>
    <span><span class="k">今日消耗</span><br><span class="v">${fmtCredits(c.todayUsed)}</span></span>
    <span><span class="k">今日到期</span><br><span class="v">${fmtCredits(c.expiringToday)}</span></span>
    <span><span class="k">明日过期</span><br><span class="v">${fmtCredits(c.expiringTomorrow)}</span></span>
    <span><span class="k">查询于</span><br><span class="num muted">${new Date(c.fetchedAtMs).toLocaleTimeString()}</span></span>`;
}

function creditsHtml(o: Overview): string {
  const c = creditsCache.get(o.productId);
  if (!c) return `<div class="credits" id="credits-${o.productId}"><span class="muted">积分未查询 · 点「刷新积分」</span></div>`;
  return `<div class="credits" id="credits-${o.productId}">${creditsInner(c)}</div>`;
}

function accountInner(id: string, pending?: string): string {
  const a = accountCache.get(id);
  if (!a) return `<span class="muted">登录账号：${esc(pending ?? "未读取")}</span>`;
  return "error" in a
    ? `<span class="muted">登录账号：${esc(a.error)}</span>`
    : `登录账号：<b>${esc(a.label)}</b>`;
}

function accountHtml(o: Overview): string {
  if (o.productId === "doubao") return dbAccountsHtml(o);
  return `<div class="acct" id="acct-${o.productId}">${accountInner(o.productId, "读取本机登录态…")}</div>`;
}

function dbAccountsInner(accts: DbAccountView[]): string {
  if (accts.length === 0) return `<span class="muted">未发现豆包登录账号</span>`;
  return accts.map(a => {
    const cls = DB_STATUS_CLASS[a.status] ?? "";
    const st = a.status === "signed" ? "auto" : a.status === "notSigned" ? "remind" : cls === "bad" ? "bad" : "remind";
    const det = a.detail && a.status !== "signed" && a.status !== "notSigned" ? `（${esc(a.detail)}）` : "";
    return `<div class="db-acct"><span class="state" data-state="${st}"></span>${esc(a.label)} <span class="muted">· ${DB_STATUS_LABEL[a.status] ?? esc(a.status)}</span>${det}</div>`;
  }).join("");
}

function dbAccountsHtml(o: Overview): string {
  const accts = dbAccountsCache.get(o.productId);
  if (!accts) return `<div class="acct" id="acct-${o.productId}"><span class="muted">登录账号：读取本机登录态…</span></div>`;
  return `<div class="acct" id="acct-${o.productId}">${dbAccountsInner(accts)}</div>`;
}

function logsHtml(o: Overview): string {
  const runs = o.runs.slice(0, LOG_LIMIT);
  if (runs.length === 0) return `<div class="empty">暂无签到日志</div>`;
  return `<table><thead><tr><th>时间</th><th>结果</th><th>详情</th></tr></thead><tbody>${runs.map(r =>
    `<tr><td class="t">${new Date(r.at).toLocaleString()}</td>
     <td><span class="badge ${OUTCOME_CLASS[r.outcome] ?? ""}">${OUTCOME_LABEL[r.outcome] ?? esc(r.outcome)}</span></td>
     <td class="muted">${esc(r.detail)}</td></tr>`).join("")}</tbody></table>`;
}

function panelHtml(o: Overview): string {
  return `<div class="card">
    <div class="card-head"><h2>${esc(PRODUCT_LABEL[o.productId] ?? o.productId)}</h2>
      <span class="grow"></span>${statusBadge(o)}</div>
    <div class="field"><label for="mode-${o.productId}">模式</label>
      <select id="mode-${o.productId}">
        <option value="off">关闭</option><option value="remind">仅提醒</option><option value="auto">自动签到</option>
      </select>
      <label for="time-${o.productId}">时间</label>
      <input type="time" id="time-${o.productId}" value="${esc(o.config.timeOfDay)}" /></div>
    <div class="field"><label for="retry-times-${o.productId}">重试次数</label>
      <input type="number" id="retry-times-${o.productId}" min="0" max="10" step="1" value="${o.config.retryTimes}" />
      <label for="retry-interval-${o.productId}">间隔分钟</label>
      <input type="number" id="retry-interval-${o.productId}" min="1" max="120" step="1" value="${o.config.retryIntervalMin}" />
    </div>
    ${accountHtml(o)}
    ${creditsHtml(o)}    <div class="row">
      <button class="btn primary" data-act="save" data-id="${o.productId}">保存</button>
      <button class="btn outline" data-act="now" data-id="${o.productId}">立即执行一次</button>
      <button class="btn ghost" data-act="credits" data-id="${o.productId}">刷新积分</button>
    </div>
    ${errHtml(o.productId)}
    ${runNoteHtml(o.productId)}
    ${oauthHtml(o.productId)}
    <p class="hint">${esc(PRODUCT_HINT[o.productId] ?? "自动读取本机登录态，无需填写任何凭证。")}</p>
    ${logsHtml(o)}
  </div>`;
}

function tabsHtml(list: Overview[]): string {
  return `<div class="tabs" role="tablist">${list.map(o => {
    const r = todayRun(o.runs);
    // 点的颜色直接由 badge 的口径推导：两处各写一遍 if 一定会对不上
    // （"窗口未开"曾是红字配灰点，用户以为那一行什么都没发生）
    const cls = r ? OUTCOME_CLASS[r.outcome] ?? "" : "";
    // 提醒档沿用既有的 "remind" 取色规则，不新增样式
    const st = cls === "ok" ? "auto" : cls === "warn" ? "remind" : cls === "bad" ? "bad" : "";
    return `<button class="tab" role="tab" data-id="${o.productId}" aria-selected="${o.productId === activeId}">
      <span class="state" ${st ? `data-state="${st}"` : ""}></span>${esc(PRODUCT_LABEL[o.productId] ?? o.productId)}</button>`;
  }).join("")}</div>`;
}

async function render() {
  const root = document.getElementById("app")!;
  let list: Overview[];
  try {
    list = await invoke<Overview[]>("get_overview");
  } catch (err) {
    // 读不到配置就不要画空面板：没有面板，用户改不了任何东西，静默渲染只会让人以为程序坏了
    root.innerHTML = `<div class="card"><p class="error">读取配置失败：${esc(String(err))}</p></div>`;
    return;
  }
  lastList = list;
  if (!activeId || !list.some(o => o.productId === activeId)) activeId = list[0]?.productId ?? null;
  root.innerHTML = `${tabsHtml(list)}<div id="panel"></div>`;
  const active = list.find(o => o.productId === activeId);
  const panel = document.getElementById("panel")!;
  if (!active) { panel.innerHTML = `<div class="empty">没有可用的产品适配器</div>`; return; }
  panel.innerHTML = panelHtml(active);
  (panel.querySelector(`#mode-${active.productId}`) as HTMLSelectElement).value = active.config.mode;
  if (active.productId === "doubao") {
    if (!dbAccountsCache.has(active.productId)) await loadDbAccounts(active.productId);
  } else if (!accountCache.has(active.productId)) {
    await loadAccount(active.productId);
  }
  if (!creditsCache.has(active.productId)) await loadCredits(active.productId, false);
}

async function loadDbAccounts(id: string) {
  const box = () => document.getElementById(`acct-${id}`);
  try {
    const accts = await invoke<DbAccountView[]>("db_query_accounts", {});
    dbAccountsCache.set(id, accts);
    if (box()) box()!.innerHTML = dbAccountsInner(accts);
  } catch (err) {
    dbAccountsCache.delete(id);
    if (box()) box()!.innerHTML = `<span class="muted">账号读取失败：${esc(String(err))}</span>`;
  }
}

async function loadAccount(id: string) {
  const box = () => document.getElementById(`acct-${id}`);
  try {
    accountCache.set(id, { label: await invoke<string>("get_account", { productId: id }) });
  } catch (err) {
    // 失败不缓存：本机登录态是只读文件探测，下次重绘会自动重试（用户刚登录完即可看到）
    accountCache.delete(id);
    if (box()) box()!.innerHTML = accountInner(id, String(err));
    return;
  }
  if (box()) box()!.innerHTML = accountInner(id);
}

async function loadCredits(id: string, force: boolean) {
  if (!force && creditsCache.has(id)) return;
  const box = () => document.getElementById(`credits-${id}`);
  if (box()) box()!.innerHTML = `<span class="muted">积分查询中…</span>`;
  try {
    const c = await invoke<CreditsSnapshot>("get_credits", { productId: id });
    creditsCache.set(id, c);
    if (box()) box()!.innerHTML = creditsInner(c);
  } catch (err) {
    creditsCache.delete(id);
    if (box()) box()!.innerHTML = `<span class="muted">积分查询失败：${esc(String(err))}</span>`;
  }
}

// 发放到账是跨服务异步的：workbuddy 的签到打在 codebuddy.cn 的营销服务上，余额读的是
// www.workbuddy.cn/billing 的资源汇总，签到接口刚返回时汇总常常还是旧值。等 5 秒再读一次。
// 放在定时器里而不是 await：按钮此时已经从「执行中…」换回来了，不该为一次补读再冻住界面。
function rereadCreditsSoon(id: string) {
  setTimeout(() => { void loadCredits(id, true); }, 5000);
}

const clamp = (n: number, lo: number, hi: number) => Math.min(hi, Math.max(lo, n));

function numInput(id: string, fallback: number): number {
  const el = document.getElementById(id) as HTMLInputElement | null;
  const v = Number(el?.value);
  return Number.isFinite(v) ? Math.trunc(v) : fallback;
}

// 界面只提交调度配置：凭证全程不经过 webview，也没有粘贴 token 的入口。
// 时间为空时返回 null：悄悄替用户填一个 09:00 等于替他改策略，Qoder 还会因此整天错过窗口。
function readConfig(id: string): ProductConfig | null {
  const prev = lastList.find(o => o.productId === id)?.config;
  const time = (document.getElementById(`time-${id}`) as HTMLInputElement).value;
  if (!/^\d{1,2}:\d{2}$/.test(time)) return null;
  return {
    productId: id,
    mode: (document.getElementById(`mode-${id}`) as HTMLSelectElement).value as ProductConfig["mode"],
    timeOfDay: time,
    retryTimes: clamp(numInput(`retry-times-${id}`, prev?.retryTimes ?? 2), 0, 10),
    retryIntervalMin: clamp(numInput(`retry-interval-${id}`, prev?.retryIntervalMin ?? 5), 1, 120),
  };
}

document.addEventListener("click", async (e) => {
  const el = e.target as HTMLElement;
  const tab = el.closest(".tab") as HTMLElement | null;
  if (tab) { activeId = tab.dataset.id!; await render(); return; }
  const btn = el.closest("button") as HTMLElement | null;
  const act = btn?.dataset.act;
  if (!act) return;
  const id = btn.dataset.id!;
  clearErr(id);
  let rereadCredits = false;
  if (act === "save") {
    btn.textContent = "保存中…";
    const cfg = readConfig(id);
    if (!cfg) {
      setErr(id, "每日触发时间还没填，先选好时间再保存。");
    } else {
      try {
        await invoke("set_config", { config: cfg });
      } catch (err) {
        setErr(id, `保存失败：${String(err)}`);
      }
    }
  } else if (act === "now") {
    btn.textContent = "执行中…";
    runNotes.delete(id);
    try {
      if (id === "doubao") {
        // 豆包多账号：逐个 profile 签到，结果按账号汇总
        const results = await invoke<DbAccountResult[]>("db_sign_all", {});
        runNotes.set(id, dbSignResultText(results));
        await loadDbAccounts(id);
      } else {
        const r = await invoke<RunResult>("sign_now", { productId: id });
        runNotes.set(id, runResultText(r));
        // 签到会动余额，缓存里那份已经是执行之前的数字了
        if (typeof r === "object" && "success" in r) {
          creditsCache.delete(id);
          rereadCredits = true;
        }
      }
    } catch (err) {
      // 「该产品正在执行中」也走这里：调度器和托盘可能已经抢先跑了
      setErr(id, `执行失败：${String(err)}`);
    }
  } else if (act === "oauth") {
    await startOauth(id);
    return;
  } else if (act === "oauth-open") {
    const url = oauthUrls.get(id);
    if (url) await openUrl(url);
    return;
  } else if (act === "md-open") {
    await startMdLogin(id);
    return;
  } else if (act === "md-probe") {
    await harvestMdLogin(id);
    return;
  } else {
    btn.textContent = "查询中…";
    await loadCredits(id, true);
  }
  await render();
  if (rereadCredits) rereadCreditsSoon(id);
});

async function startOauth(id: string) {
  if (oauthRunning.has(id)) return;
  oauthRunning.add(id);
  try {
    const s = await invoke<OAuthSession>("wb_oauth_login");
    oauthUrls.set(id, s.authUrl);
    setNote(id, `浏览器已打开官方登录页，请在其中完成登录（${Math.round(s.expiresIn / 60)} 分钟内有效）。没看到页面就点「重新打开登录页」。`);
    await render();
    await pollOauth(id, s);
  } catch (err) {
    setNote(id, `登录失败：${String(err)}`);
  } finally {
    oauthRunning.delete(id);
    await render();
  }
}

async function pollOauth(id: string, s: OAuthSession) {  const until = Date.now() + (s.expiresIn + 15) * 1000;
  while (Date.now() < until) {
    await sleep(2000);
    const st = await invoke<OAuthStatus>("wb_oauth_status", { loginId: s.loginId });
    if (st.done) {
      setNote(id, st.nickname ? `登录成功：${st.nickname}。现在可直接点「立即执行一次」。`
        : "登录成功，之后自动续期，无需粘贴 token。");
      creditsCache.delete(id);
      accountCache.delete(id);
      await loadCredits(id, true);
      return;
    }
  }
  setNote(id, "登录超时：请在浏览器里完成登录后再点一次。");
}

// 秒哒/灵镜没有可轮询的官方登录会话：登录发生在 SignDock 自有的 WebView2 窗口里，
// 由后端读那个窗口的 cookie 罐（秒哒）或 localStorage（灵镜）并让服务端验一次货，
// 前端只知道"成了/还没成"。
//
// 这里刻意「不自动」：读 WebView2 存储/cookie 在 Windows 上会在主线程里跑一层嵌套消息泵，
// 一旦它等不到回调，整个 tauri 事件循环就再也不转了 —— 窗口白屏、点 X 没反应、
// 托盘菜单全部失效，只能强杀进程。定时器轮询等于每小时给这个赌注两百次机会，
// 所以取会话必须由用户点一下，只发生一次。
async function startMdLogin(id: string) {
  const cfg = SESSION_LOGIN[id];
  if (!cfg || oauthRunning.has(id)) return;
  oauthRunning.add(id);
  try {
    await invoke(cfg.open);
    mdOpen.add(id);
    setNote(id, "登录窗口已打开。请在里面完成登录，然后回来点「我登录好了，取会话」。");
  } catch (err) {
    mdOpen.delete(id);
    setNote(id, `打开登录窗口失败：${String(err)}`);
  } finally {
    oauthRunning.delete(id);
    await render();
  }
}

async function harvestMdLogin(id: string) {
  const cfg = SESSION_LOGIN[id];
  if (!cfg || oauthRunning.has(id)) return;
  oauthRunning.add(id);
  try {
    const st = await invoke<MdProbe>(cfg.probe);
    if (st.done) {
      mdOpen.delete(id);
      setNote(id, "会话已封存。现在可直接点「立即执行一次」。");
      creditsCache.delete(id);
      accountCache.delete(id);
      await loadCredits(id, true);
    } else if (st.windowGone) {
      mdOpen.delete(id);
      setNote(id, "登录窗口已经关掉了：请再点一次「打开登录窗口」重新登录。");
    } else {
      // 窗口还在、按钮也还在：这一次没成就再点一次，不该把用户推回去重开窗口
      setNote(id, "还不认这个会话：请确认窗口里已经登录完成，再点一次。");
    }
  } catch (err) {
    setNote(id, `取会话失败：${String(err)}`);
  } finally {
    oauthRunning.delete(id);
    await render();
  }
}

render();
