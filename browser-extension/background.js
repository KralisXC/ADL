// ADL Download Bridge —— 后台 Service Worker（MV3）
//
// 职责：
//   1. 浏览器产生下载时（downloads.onCreated），尝试交给 ADL 的本地 API；
//      成功则取消浏览器原生下载；失败（ADL 未运行/没开接管/出错）则原样放行。
//   2. 右键链接菜单「使用 ADL 下载」。
//   3. 与本地 API 的鉴权：/auto-token 拿 token（Origin 校验），之后每个请求带 x-adl-token。
//
// 注意：MV3 Service Worker 不能读本地文件；token 通过 HTTP 接口获取并缓存到 storage。

const DEFAULT_PORT = 17800;

async function getConfig() {
  const o = await chrome.storage.local.get({ port: DEFAULT_PORT, enabled: true });
  return o;
}

let tokenCache = null; // { token, at }

async function getPort() {
  const { port } = await getConfig();
  return port || DEFAULT_PORT;
}

async function fetchToken(force) {
  const now = Date.now();
  if (!force && tokenCache && now - tokenCache.at < 30 * 60 * 1000) {
    return tokenCache.token;
  }
  const port = await getPort();
  const res = await fetch(`http://127.0.0.1:${port}/auto-token`, { method: "GET" });
  if (!res.ok) throw new Error("auto-token 失败: " + res.status);
  const j = await res.json();
  tokenCache = { token: j.token, at: now };
  return j.token;
}

async function api(path, method, body) {
  const port = await getPort();
  let token = await fetchToken(false);
  const doFetch = (tk) =>
    fetch(`http://127.0.0.1:${port}${path}`, {
      method,
      headers: {
        "Content-Type": "application/json",
        "x-adl-token": tk,
      },
      body: body == null ? undefined : JSON.stringify(body),
    });
  let res = await doFetch(token);
  if (res.status === 401) {
    token = await fetchToken(true);
    res = await doFetch(token);
  }
  const text = await res.text();
  let json = null;
  try {
    json = text ? JSON.parse(text) : null;
  } catch (_) {
    /* ignore */
  }
  return { ok: res.ok, status: res.status, json, text };
}

async function cookiesFor(url) {
  try {
    const list = await chrome.cookies.getAll({ url });
    return list.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch (_) {
    return "";
  }
}

function baseName(p) {
  if (!p) return null;
  const s = String(p).split(/[\\/]/).pop();
  return s || null;
}

// 送达 ADL；返回 true 表示 ADL 已接受（可以取消浏览器下载）
async function deliver(url, extra) {
  if (!/^https?:/i.test(url || "")) return false;
  const cfg = await getConfig();
  if (!cfg.enabled) return false;

  let cookies = "";
  try {
    cookies = await cookiesFor(url);
  } catch (_) {
    cookies = "";
  }
  const body = {
    url,
    cookies,
    user_agent: navigator.userAgent,
  };
  if (extra) {
    if (extra.referer) body.referer = extra.referer;
    if (extra.out) body.out = extra.out;
    if (extra.headers) body.headers = extra.headers;
  }
  try {
    const r = await api("/add", "POST", body);
    return !!(r.ok && r.json && r.json.ok);
  } catch (e) {
    console.warn("[ADL] 转交失败（保留浏览器下载）:", e);
    return false;
  }
}

// —— 拦截浏览器下载 ——
chrome.downloads.onCreated.addListener(async (item) => {
  try {
    const cfg = await getConfig();
    if (!cfg.enabled) return;
    if (item.byExtensionId && item.byExtensionId === chrome.runtime.id) return;
    if (!/^https?:/i.test(item.url || "")) return;

    const out = baseName(item.filename);
    const ok = await deliver(item.url, { referer: item.referrer, out });
    if (ok) {
      try {
        await chrome.downloads.cancel(item.id);
      } catch (_) {}
      try {
        await chrome.downloads.erase({ id: item.id });
      } catch (_) {}
      console.log("[ADL] 已接管并取消浏览器下载:", item.url);
    }
  } catch (e) {
    console.warn("[ADL] onCreated 处理异常:", e);
  }
});

// —— 右键菜单 ——
function createMenus() {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: "adl-download-link",
      title: "使用 ADL 下载",
      contexts: ["link"],
    });
  });
}

chrome.runtime.onInstalled.addListener(createMenus);
chrome.runtime.onStartup.addListener(createMenus);

chrome.contextMenus.onClicked.addListener(async (info) => {
  if (info.menuItemId !== "adl-download-link" || !info.linkUrl) return;
  const ok = await deliver(info.linkUrl, { referer: info.pageUrl });
  if (!ok) {
    // 兜底：让浏览器自己下载
    try {
      chrome.downloads.download({ url: info.linkUrl });
    } catch (_) {}
  }
});

// —— 弹窗/选项页发来的消息 ——
chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  (async () => {
    if (msg && msg.type === "status") {
      try {
        const r = await api("/status", "GET", null);
        sendResponse({ ok: true, status: r.json });
      } catch (e) {
        sendResponse({ ok: false, error: String(e) });
      }
    } else if (msg && msg.type === "test") {
      try {
        const r = await api("/check", "GET", null);
        sendResponse({ ok: r.ok, detail: r.json });
      } catch (e) {
        sendResponse({ ok: false, error: String(e) });
      }
    } else {
      sendResponse({ ok: false, error: "unknown" });
    }
  })();
  return true; // 异步响应
});
