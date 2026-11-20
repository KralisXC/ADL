const DEFAULT_PORT = 17800;

async function load() {
  const o = await chrome.storage.local.get({ port: DEFAULT_PORT, enabled: true });
  document.getElementById("port").value = o.port;
  document.getElementById("enabled").checked = o.enabled;
}

document.getElementById("enabled").addEventListener("change", async (e) => {
  await chrome.storage.local.set({ enabled: e.target.checked });
});

document.getElementById("port").addEventListener("change", async (e) => {
  const p = parseInt(e.target.value, 10);
  if (p >= 1 && p <= 65535) await chrome.storage.local.set({ port: p });
});

document.getElementById("test").addEventListener("click", () => {
  const el = document.getElementById("status");
  el.textContent = "测试中…";
  chrome.runtime.sendMessage({ type: "test" }, (resp) => {
    if (resp && resp.ok) el.textContent = "✅ 已连上 ADL";
    else el.textContent = "❌ 连接失败：" + (resp ? resp.error || JSON.stringify(resp.detail) : "无响应");
  });
});

load();
