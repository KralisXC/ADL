// ADL 下载桥 —— MV3 service worker
//
// 设计分工（用户 m24075）：
//   浏览器里只保留「接管开/关」这一个开关，其余全部由 ADL 桌面端的
//   「浏览器接管」页下发。所以这里的 getBridge() 是**唯一**的配置来源，
//   storage 里只存两个东西：enabled（本地开关）和 lastPort（上次问到的端口，
//   只是省一次探测，不作数）。
//
// 端口发现：ADL 永远会在 17800 上应一声（配置端口不是 17800 时它会在 17800
// 再绑一个别名监听器），`GET /bridge` 回的是真实端口 + 全部策略。所以
// 扩展不需要用户填端口，改端口也不会失联。

const DISCOVER_PORT = 17800;
const LAUNCH_SCHEME = "adl://open";

/** 只有接管开关存在本地；其余配置一律问桌面端。 */
async function getEnabled() {
  const { enabled = true } = await chrome.storage.local.get({ enabled: true });
  return enabled !== false;
}

async function setEnabled(on) {
  await chrome.storage.local.set({ enabled: on !== false });
}

let bridgeCache = null; // { at, base, cfg }
let tokenCache = null; // { at, token, base }

const BRIDGE_TTL = 30_000; // 30 秒问一次策略就够，改设置不必立刻生效
const TOKEN_TTL = 30 * 60 * 1000;

function nowMs() {
  return Date.now();
}

/** 在给定端口上问一次 /bridge；连不上返回 null。 */
async function probeBridge(port) {
  const base = `http://127.0.0.1:${port}`;
  try {
    const r = await fetch(`${base}/bridge`, {
      method: "GET",
      headers: { "X-ADL-Client": "extension" },
      cache: "no-store",
    });
    if (!r.ok) return null;
    const cfg = await r.json();
    if (!cfg || cfg.ok !== true) return null;
    // 接口版本不兼容就认输：宁可退回浏览器原生下载，也不要瞎投递。
    if (typeof cfg.api === "number" && cfg.api > 1) {
      console.warn("[ADL] 桌面端接口版本过新，停止接管", cfg.api);
      return null;
    }
    return { base: `http://127.0.0.1:${cfg.port || port}`, cfg };
  } catch (_) {
    return null;
  }
}

/** 拿桌面端能力 + 真实端口。缓存 30 秒。 */
async function getBridge({ force = false } = {}) {
  if (!force && bridgeCache && nowMs() - bridgeCache.at < BRIDGE_TTL) {
    return bridgeCache;
  }
  const { lastPort } = await chrome.storage.local.get({ lastPort: 0 });

  // 先试已知端口，再试发现端口 —— 两者相同时只试一次。
  const ports = [];
  if (lastPort) ports.push(lastPort);
  if (!ports.includes(DISCOVER_PORT)) ports.push(DISCOVER_PORT);

  for (const p of ports) {
    const hit = await probeBridge(p);
    if (hit) {
      bridgeCache = { at: nowMs(), base: hit.base, cfg: hit.cfg };
      tokenCache = null; // 端口可能变了，旧 token 不再可信
      await chrome.storage.local.set({ lastPort: hit.cfg.port || p });
      return bridgeCache;
    }
  }
  bridgeCache = null;
  return null;
}

/**
 * ADL 没在跑的时候把它拉起来（用户 m24107）。
 *
 * 走自注册的 adl:// 协议 —— 只有 ADL 自己能处理，不会把用户带到网页上。
 * 拉起后要等它把本地接口绑好才能投递，所以这里轮询最多 12 秒。
 */
async function launchAdl() {
  try {
    await chrome.tabs.create({ url: LAUNCH_SCHEME, active: false });
  } catch (e) {
    console.warn("[ADL] 唤起 adl:// 失败", e);
    return null;
  }
  for (let i = 0; i < 24; i++) {
    await new Promise((r) => setTimeout(r, 500));
    const hit = await getBridge({ force: true });
    if (hit) return hit;
  }
  return null;
}

async function fetchToken() {
  const hit = await getBridge();
  if (!hit) return null;
  if (tokenCache && tokenCache.base === hit.base && nowMs() - tokenCache.at < TOKEN_TTL) {
    return tokenCache.token;
  }
  try {
    const r = await fetch(`${hit.base}/auto-token`, {
      headers: { "X-ADL-Client": "extension" },
      cache: "no-store",
    });
    if (!r.ok) return null;
    const j = await r.json();
    if (!j || !j.ok || !j.token) return null;
    tokenCache = { at: nowMs(), token: j.token, base: hit.base };
    return j.token;
  } catch (_) {
    return null;
  }
}

/** 带 token 调一次接口；401 就换新 token 重试一次。 */
async function api(path, { method = "GET", body = null, retry = true } = {}) {
  const hit = await getBridge();
  if (!hit) return { ok: false, offline: true };
  const token = await fetchToken();
  const headers = { "X-ADL-Client": "extension" };
  if (token) headers["x-adl-token"] = token;
  if (body) headers["Content-Type"] = "application/json";
  try {
    const r = await fetch(hit.base + path, {
      method,
      headers,
      body: body ? JSON.stringify(body) : undefined,
      cache: "no-store",
    });
    const text = await r.text();
    let json = null;
    try {
      json = text ? JSON.parse(text) : null;
    } catch (_) {}
    if (r.status === 401 && retry) {
      tokenCache = null;
      return api(path, { method, body, retry: false });
    }
    return { ok: r.ok, status: r.status, json, text };
  } catch (e) {
    return { ok: false, error: String(e) };
  }
}

/** 页面的 Cookie 拼成一条 Cookie 头。 */
async function cookiesFor(url) {
  try {
    const list = await chrome.cookies.getAll({ url });
    if (!list || !list.length) return "";
    return list.map((c) => `${c.name}=${c.value}`).join("; ");
  } catch (_) {
    return "";
  }
}

function baseName(p) {
  if (!p) return "";
  const clean = p.split(/[?#]/)[0];
  const seg = clean.split("/").pop() || "";
  try {
    return decodeURIComponent(seg);
  } catch (_) {
    return seg;
  }
}

/** 失败要让用户在 ADL 里看见 —— 扩展控制台的 warn 没人会翻。 */
async function reportFailure(url, why) {
  const hit = await getBridge();
  if (!hit || hit.cfg.notify_fail === false) return;
  await api("/notify", {
    method: "POST",
    body: { message: `接管失败：${why}${url ? `（${baseName(url)}）` : ""}`, error: true },
  });
}

/**
 * 把一条下载交给 ADL。
 * 返回 true = 已成功交给 ADL（调用方应当取消浏览器自己的下载）。
 */
async function deliver(url, extra = {}) {
  if (!url || !/^https?:/i.test(url)) return false;
  if (!(await getEnabled())) return false;

  let hit = await getBridge();
  if (!hit) {
    // 桌面端没在跑。只有它允许时才试着拉起来。
    const { lastPort } = await chrome.storage.local.get({ lastPort: 0 });
    const policy = await probeBridge(lastPort || DISCOVER_PORT);
    if (policy && policy.cfg.allow_launch === false) return false;
    hit = await launchAdl();
    if (!hit) return false;
  }

  const cfg = hit.cfg || {};
  // 小文件走浏览器原生更快：不起引擎、不建控制文件。
  if (cfg.min_mb > 0 && typeof extra.size === "number" && extra.size > 0) {
    const mb = extra.size / (1024 * 1024);
    if (mb < cfg.min_mb) return false;
  }

  const payload = { url };
  if (cfg.send_cookies !== false) {
    const ck = await cookiesFor(url);
    if (ck) payload.cookies = ck;
  }
  if (extra.referer) payload.referer = extra.referer;
  if (extra.out) payload.out = extra.out;
  if (extra.headers) payload.headers = extra.headers;
  try {
    payload.user_agent = navigator.userAgent;
  } catch (_) {}

  const r = await api("/add", { method: "POST", body: payload });
  if (r.ok && r.json && r.json.ok) return true;
  await reportFailure(url, (r.json && r.json.error) || r.text || r.error || "未知错误");
  return false;
}

// ── 下载拦截 ──
chrome.downloads.onCreated.addListener(async (item) => {
  // 自己用 chrome.downloads.download 兜底发的，不要再接管回来，否则死循环。
  if (item.byExtensionId && item.byExtensionId === chrome.runtime.id) return;
  if (!(await getEnabled())) return;
  const ok = await deliver(item.url, {
    size: item.fileSize,
    referer: item.referrer,
    out: baseName(item.filename) || undefined,
  });
  if (ok) {
    try {
      await chrome.downloads.cancel(item.id);
      await chrome.downloads.erase({ id: item.id });
    } catch (e) {
      console.warn("[ADL] 取消浏览器下载失败", e);
    }
  }
});

// ── 右键菜单：只给链接和页面本身 ──
const MENU_LINK = "adl-download-link";
const MENU_PAGE = "adl-download-page";

function createMenus() {
  chrome.contextMenus.removeAll(() => {
    chrome.contextMenus.create({
      id: MENU_LINK,
      title: "用 ADL 下载这个链接",
      contexts: ["link"],
    });
    chrome.contextMenus.create({
      id: MENU_PAGE,
      title: "用 ADL 下载当前页面的地址",
      contexts: ["page", "selection"],
    });
  });
}

chrome.runtime.onInstalled.addListener(createMenus);
chrome.runtime.onStartup.addListener(createMenus);

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  const url = info.menuItemId === MENU_LINK ? info.linkUrl : tab && tab.url;
  if (!url) return;
  const ok = await deliver(url, {
    referer: info.pageUrl,
    out: baseName(url) || undefined,
  });
  if (!ok) {
    // 交给 ADL 失败就退回浏览器自己的下载，别让用户点了没反应。
    try {
      await chrome.downloads.download({ url });
    } catch (e) {
      console.warn("[ADL] 兜底下载也失败", e);
    }
  }
});

// ── popup / 其它页面来的消息 ──
chrome.runtime.onMessage.addListener((msg, _sender, sendResponse) => {
  (async () => {
    if (!msg || !msg.type) {
      sendResponse({ ok: false });
      return;
    }
    if (msg.type === "state") {
      const enabled = await getEnabled();
      const hit = await getBridge({ force: true });
      sendResponse({
        ok: true,
        enabled,
        online: !!hit,
        port: hit ? hit.cfg.port : 0,
        api: hit ? hit.cfg.api : 0,
      });
      return;
    }
    if (msg.type === "setEnabled") {
      await setEnabled(msg.enabled);
      sendResponse({ ok: true, enabled: await getEnabled() });
      return;
    }
    if (msg.type === "downloadCurrent") {
      const ok = await deliver(msg.url, { referer: msg.referer, out: baseName(msg.url) });
      sendResponse({ ok });
      return;
    }
    if (msg.type === "openAdl") {
      const hit = await launchAdl();
      sendResponse({ ok: !!hit, port: hit ? hit.cfg.port : 0 });
      return;
    }
    sendResponse({ ok: false, error: "unknown type" });
  })();
  return true; // 异步响应
});
