import { invoke } from "@tauri-apps/api/core";

interface ProductConfig { productId: string; mode: "off" | "remind" | "auto"; timeOfDay: string; token: string | null; }
interface RunRow { id: number; productId: string; at: string; outcome: string; detail: string; }
interface Overview { productId: string; config: ProductConfig; runs: RunRow[]; }
interface CreditsSnapshot { balance: number; todayUsed: number; expiringToday: number; fetchedAtMs: number; }

const fmtCredits = (n: number) => n.toLocaleString("zh-CN", { minimumFractionDigits: 2, maximumFractionDigits: 2 });
const LOG_LIMIT = 5;

const OUTCOME_LABEL: Record<string, string> = {
  success: "成功", alreadySigned: "已签", reminded: "提醒", needManual: "需人工", failed: "失败",
};
const OUTCOME_CLASS: Record<string, string> = {
  success: "ok", alreadySigned: "ok", reminded: "warn", needManual: "warn", failed: "bad",
};
const PRODUCT_LABEL: Record<string, string> = { workbuddy: "WorkBuddy", trae: "Trae", qoder: "Qoder" };
const PRODUCT_HINT: Record<string, string> = {
  workbuddy: "WorkBuddy 5.6 起把本机登录凭据加密存储，无法自动读取。请在此粘贴 accessToken（有效期约 55 天），粘贴后点「保存」。",
  trae: "自动读取本机 TRAE 登录态，一般无需填写。token 过期时请打开 TRAE 让它自行刷新，或在上方「立即执行」前手动粘贴。",
  qoder: "Qoder 接口尚未侦察，当前为占位实现。",
};

// 防注入：token/detail 为用户粘贴或服务端字符串，插入 innerHTML 前必须转义。
function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

const creditsCache = new Map<string, CreditsSnapshot>();
let activeId: string | null = null;

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
    <span><span class="k">查询于</span><br><span class="num muted">${new Date(c.fetchedAtMs).toLocaleTimeString()}</span></span>`;
}

function creditsHtml(o: Overview): string {
  const c = creditsCache.get(o.productId);
  if (!c) return `<div class="credits" id="credits-${o.productId}"><span class="muted">积分未查询 · 点「刷新积分」</span></div>`;
  return `<div class="credits" id="credits-${o.productId}">${creditsInner(c)}</div>`;
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
    ${creditsHtml(o)}
    <div class="row">
      <button class="btn primary" data-act="save" data-id="${o.productId}">保存</button>
      <button class="btn outline" data-act="now" data-id="${o.productId}">立即执行一次</button>
      <button class="btn ghost" data-act="credits" data-id="${o.productId}">刷新积分</button>
    </div>
    <details class="adv" ${o.config.token ? "open" : ""}><summary>高级：手动粘贴 token</summary>
      <div class="field"><label for="token-${o.productId}">Token</label>
        <input type="text" id="token-${o.productId}" placeholder="留空则自动读取本机登录态" value="${esc(o.config.token ?? "")}" /></div>
      <p class="hint">${esc(PRODUCT_HINT[o.productId] ?? "留空则自动读取本机登录态。")}</p>
    </details>
    ${logsHtml(o)}
  </div>`;
}

function tabsHtml(list: Overview[]): string {
  return `<div class="tabs" role="tablist">${list.map(o => {
    const r = todayRun(o.runs);
    const st = r && (r.outcome === "success" || r.outcome === "alreadySigned") ? "auto"
      : r && (r.outcome === "failed" || r.outcome === "needManual") ? "remind" : "";
    return `<button class="tab" role="tab" data-id="${o.productId}" aria-selected="${o.productId === activeId}">
      <span class="state" ${st ? `data-state="${st}"` : ""}></span>${esc(PRODUCT_LABEL[o.productId] ?? o.productId)}</button>`;
  }).join("")}</div>`;
}

async function render() {
  const list = await invoke<Overview[]>("get_overview");
  if (!activeId || !list.some(o => o.productId === activeId)) activeId = list[0]?.productId ?? null;
  const root = document.getElementById("app")!;
  root.innerHTML = `${tabsHtml(list)}<div id="panel"></div>`;
  const active = list.find(o => o.productId === activeId);
  const panel = document.getElementById("panel")!;
  if (!active) { panel.innerHTML = `<div class="empty">没有可用的产品适配器</div>`; return; }
  panel.innerHTML = panelHtml(active);
  (panel.querySelector(`#mode-${active.productId}`) as HTMLSelectElement).value = active.config.mode;
  if (!creditsCache.has(active.productId)) await loadCredits(active.productId, false);
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

document.addEventListener("click", async (e) => {
  const el = e.target as HTMLElement;
  const tab = el.closest(".tab") as HTMLElement | null;
  if (tab) { activeId = tab.dataset.id!; await render(); return; }
  const btn = el.closest("button") as HTMLElement | null;
  if (!btn?.dataset.act) return;
  const id = btn.dataset.id!;
  if (btn.dataset.act === "save") {
    btn.textContent = "保存中…";
    await invoke("set_config", { cfg: {
      productId: id,
      mode: (document.getElementById(`mode-${id}`) as HTMLSelectElement).value,
      timeOfDay: (document.getElementById(`time-${id}`) as HTMLInputElement).value || "09:00",
      token: (document.getElementById(`token-${id}`) as HTMLInputElement).value.trim() || null,
    }});
  } else if (btn.dataset.act === "now") {
    btn.textContent = "执行中…";
    await invoke("sign_now", { productId: id });
  } else {
    btn.textContent = "查询中…";
    await loadCredits(id, true);
  }
  await render();
});

render();
