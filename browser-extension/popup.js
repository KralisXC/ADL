// popup：只做三件事 —— 看状态、开关接管、把当前页交给 ADL。
// 其它设置一律引导到 ADL 桌面端（用户 m24075 要的就是这个分工）。

const dot = document.getElementById("dot");
const statusEl = document.getElementById("status");
const track = document.getElementById("track");
const toggle = document.getElementById("toggle");
const toggleHint = document.getElementById("toggleHint");
const dlPage = document.getElementById("dlPage");
const openAdl = document.getElementById("openAdl");

function paint({ enabled, online, port }) {
  track.classList.toggle("on", !!enabled);
  if (!enabled) {
    dot.className = "dot";
    statusEl.textContent = "接管已关闭，浏览器下载走原生。";
    toggleHint.textContent = "关掉之后，浏览器下载不再交给 ADL。";
    dlPage.disabled = true;
    openAdl.textContent = "打开 ADL";
    return;
  }
  if (online) {
    dot.className = "dot on";
    statusEl.textContent = `已连上 ADL（127.0.0.1:${port}）`;
    dlPage.disabled = false;
    openAdl.textContent = "打开 ADL";
  } else {
    dot.className = "dot off";
    statusEl.textContent = "没连上 ADL，点下面把它拉起来。";
    dlPage.disabled = true;
    openAdl.textContent = "启动 ADL";
  }
  toggleHint.textContent = "关掉之后，浏览器下载不再交给 ADL。";
}

async function send(msg) {
  return new Promise((resolve) => {
    chrome.runtime.sendMessage(msg, (resp) => resolve(resp || { ok: false }));
  });
}

async function refresh(force) {
  const st = await send({ type: "state", force: !!force });
  if (st && st.ok) {
    paint(st);
  } else {
    dot.className = "dot";
    statusEl.textContent = "加载状态失败";
  }
  return st;
}

toggle.addEventListener("click", async () => {
  const cur = track.classList.contains("on");
  const r = await send({ type: "setEnabled", enabled: !cur });
  if (r && r.ok) await refresh(true);
});

dlPage.addEventListener("click", async () => {
  const tabs = await chrome.tabs.query({ active: true, currentWindow: true });
  const tab = tabs && tabs[0];
  if (!tab || !tab.url) return;
  dlPage.disabled = true;
  const r = await send({ type: "downloadCurrent", url: tab.url, referer: tab.url });
  dlPage.disabled = false;
  if (r && r.ok) {
    statusEl.textContent = "已交给 ADL 下载。";
  } else {
    statusEl.textContent = "交给 ADL 失败了（见 ADL 消息中心）。";
  }
});

openAdl.addEventListener("click", async () => {
  openAdl.disabled = true;
  const r = await send({ type: "openAdl" });
  openAdl.disabled = false;
  if (r && r.ok) {
    statusEl.textContent = `ADL 已就绪（127.0.0.1:${r.port}）`;
    await refresh(true);
  } else {
    statusEl.textContent = "没等到 ADL 起来，确认它装好了吗？";
  }
});

refresh(true);
