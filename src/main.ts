import { invoke } from "@tauri-apps/api/core";

interface ProductConfig { productId: string; mode: "off" | "remind" | "auto"; timeOfDay: string; token: string | null; }
interface RunRow { id: number; productId: string; at: string; outcome: string; detail: string; }
interface Overview { productId: string; config: ProductConfig; runs: RunRow[]; }

const OUTCOME_LABEL: Record<string, string> = {
  success: "成功", alreadySigned: "已签", reminded: "提醒", needManual: "需人工", failed: "失败",
};

// 防注入：token/detail 为用户粘贴或服务端字符串，插入 innerHTML 前必须转义。
function esc(s: string): string {
  return s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}

async function refresh() {
  const list = await invoke<Overview[]>("get_overview");
  const root = document.getElementById("cards")!;
  root.innerHTML = "";
  for (const o of list) {
    const card = document.createElement("section");
    card.className = "card";
    card.innerHTML = `<h2>${o.productId}</h2>
      <label>模式
        <select id="mode-${o.productId}">
          <option value="off">关闭</option><option value="remind">仅提醒</option><option value="auto">自动签到</option>
        </select></label>
      <label>时间 <input type="time" id="time-${o.productId}" value="${esc(o.config.timeOfDay)}" /></label>
      <label>Token <input type="text" id="token-${o.productId}" placeholder="手动粘贴 token" value="${esc(o.config.token ?? "")}" /></label>
      <div class="row">
        <button data-act="save" data-id="${o.productId}">保存</button>
        <button data-act="now" data-id="${o.productId}">立即执行一次</button>
        <span id="status-${o.productId}" class="muted">今日：${todayStatus(o.runs)}</span>
      </div>
      <table><thead><tr><th>时间</th><th>结果</th><th>详情</th></tr></thead>
      <tbody>${o.runs.slice(0, 10).map(r =>
        `<tr><td>${new Date(r.at).toLocaleString()}</td><td>${OUTCOME_LABEL[r.outcome] ?? r.outcome}</td><td>${esc(r.detail)}</td></tr>`
      ).join("")}</tbody></table>`;
    (card.querySelector(`#mode-${o.productId}`) as HTMLSelectElement).value = o.config.mode;
    root.appendChild(card);
  }
}

function todayStatus(runs: RunRow[]): string {
  const d = new Date().toDateString();
  const today = runs.find(r => new Date(r.at).toDateString() === d);
  return today ? `${OUTCOME_LABEL[today.outcome] ?? today.outcome} · ${esc(today.detail)}` : "未执行";
}

document.addEventListener("click", async (e) => {
  const btn = (e.target as HTMLElement).closest("button");
  if (!btn) return;
  const id = btn.dataset.id!;
  if (btn.dataset.act === "save") {
    await invoke("set_config", { cfg: {
      productId: id,
      mode: (document.getElementById(`mode-${id}`) as HTMLSelectElement).value,
      timeOfDay: (document.getElementById(`time-${id}`) as HTMLInputElement).value || "09:00",
      token: (document.getElementById(`token-${id}`) as HTMLInputElement).value || null,
    }});
    await refresh();
  } else if (btn.dataset.act === "now") {
    btn.textContent = "执行中…";
    const result = await invoke<Record<string, unknown>>("sign_now", { productId: id });
    btn.textContent = "立即执行一次";
    await refresh();
    void result;
  }
});

refresh();
