// ADL 下载桥 —— MV3 service worker
//
// 设计分工（用户 m24075）：
//   浏览器里只保留「接管开/关」这一个开关，其余全部由 ADL 桌面端的
//   「浏览器接管」页下发。所以这里的 getBridge() 是**唯一**的配置来源，
//   storage 里只存两个东西：enabled（本地开关）和 lastPort（上次问到的端口，
//   只是省一次探测，不作数）。
//
// 交互（用户 m24756）：**点图标就直接切换接管开关**，不开弹窗；
// 图标右下角用角标显示 ON / OFF。其余动作（下载当前页、打开 ADL）放右键菜单。
//
// 端口发现：ADL 永远会在 17800 上应一声（配置端口不是 17800 时它会在 17800
// 再绑一个别名监听器），`GET /bridge` 回的是真实端口 + 全部策略。所以
// 扩展不需要用户填端口，改端口也不会失联。

const DISCOVER_PORT = 17800;
const LAUNCH_SCHEME = "adl://open";

const BADGE_ON = "ON";
const BADGE_OFF = "OFF";
const BADGE_ON_COLOR = "#2E7D6B"; // 与 ADL 主色一致
const BADGE_OFF_COLOR = "#8A9A98";

/** 只有接管开关存在本地；其余配置一律问桌面端。 */
async function getEnabled() {
  const { enabled = true } = await chrome.storage.local.get({ enabled: true });
  return enabled !== false;
}

/** 把开关状态画到图标角标上 —— 这是用户唯一能一眼看到的"现在管不管"。 */
async function paintBadge(enabled) {
  try {
    await chrome.action.setBadgeText({ text: enabled ? BADGE_ON : BADGE_OFF });
    await chrome.action.setBadgeBackgroundColor({
      color: enabled ? BADGE_ON_COLOR : BADGE_OFF_COLOR,
    });
    // 角标默认字号偏小，ON/OFF 两个字母还能看清；给个白字提高对比度。
    if (chrome.action.setBadgeTextColor) {
      await chrome.action.setBadgeTextColor({ color: "#FFFFFF" });
    }
    await chrome.action.setTitle({
      title: enabled ? "ADL 下载桥：正在接管（点击关闭）" : "ADL 下载桥：已关闭（点击开启）",
    });
  } catch (e) {
    console.warn("[ADL] 画角标失败", e);
  }
}

async function setEnabled(on) {
  const v = on !== false;
  await chrome.storage.local.set({ enabled: v });
  await paintBadge(v);
  return v;
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
 *
 * `confirm === true` 时 ADL 只是把参数记下来、弹「新建下载」等人点确认，
 * 不会立刻开跑（用户 m24756 要求的"别静默下，让我看得到"）。
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
  // 「先弹新建下载确认」由桌面端决定（默认开）。开着的话桌面端只填表不开跑。
  payload.confirm = cfg.confirm_first !== false;
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
//
// 🔴 只接管"用户此刻主动发起"的下载。
//
// 早先这里是"凡是 onCreated 就投递"，结果关掉浏览器再打开（或打开下载历史
// 点「重新下载」）时，Chrome 会把上次没下完的任务自动续传，每续一条都触发
// 一次 onCreated —— 一屏老记录被当成新下载全塞进了 ADL（用户 m24756 报的 bug）。
//
// 续传/恢复的条目有几个共同特征，任意一条命中就说明不是新下载：
//   · `exists === true`：目标文件已经躺在磁盘上了（原生下载没删掉才会这样）
//   · `bytesReceived > 0`：事件到达时已经在收数据了，说明不是刚创建
//   · `startTime` 早于几秒前：这是历史条目，不是刚刚点的
const FRESH_MS = 3000;

// 上面三条还不够稳：Chrome 续传历史下载时，有时候 `startTime` 会被刷成
// "现在"、`bytesReceived` 也可能还是 0、`exists` 也未必为真 —— 于是又被当成
// 新下载（用户 m25107 第二次踩到，一屏 E_downloadxxxxx 涌进来）。
//
// 所以再加一道"本次浏览器 session"闸：service worker 启动时记一个时刻，
// 只有 `startTime` **晚于**这个时刻的才算"这次开机之后新建的"。浏览器重启后
// 自动续传的老条目，`startTime` 一定落在上一个 session 里，直接拦掉。
//
// 用 `chrome.storage.session`（MV3 内存级，关浏览器就没了），不能用户本地存储：
// 那个是持久的，读到的会是"三天前那次开机"的时间，等于这道闸永远不生效。
const SESSION_KEY = "adlSessionBootAt";

async function sessionBootAt() {
  try {
    const got = await chrome.storage.session.get({ [SESSION_KEY]: 0 });
    let at = Number(got[SESSION_KEY]) || 0;
    if (!at) {
      at = nowMs();
      await chrome.storage.session.set({ [SESSION_KEY]: at });
    }
    return at;
  } catch (e) {
    // 老版本 Chrome 没有 storage.session：退回 0，等于只靠上面三条判据。
    console.debug("[ADL] storage.session 不可用", e);
    return 0;
  }
}

function looksResumed(item, bootAt) {
  if (item.exists) return true;
  if (typeof item.bytesReceived === "number" && item.bytesReceived > 0) return true;
  if (item.startTime) {
    const started = Date.parse(item.startTime);
    if (!Number.isNaN(started)) {
      if (nowMs() - started > FRESH_MS) return true;
      // 比本次 session 还早 → 上一个 session 留下的历史条目。
      // 留 5 秒余量：worker 被唤醒的时刻可能比事件的 startTime 稍晚一点点。
      if (bootAt && started < bootAt - 5000) return true;
    }
  }
  return false;
}

chrome.downloads.onCreated.addListener(async (item) => {
  // 自己用 chrome.downloads.download 兜底发的，不要再接管回来，否则死循环。
  if (item.byExtensionId && item.byExtensionId === chrome.runtime.id) return;
  if (item.state && item.state !== "in_progress") return;
  const bootAt = await sessionBootAt();
  if (looksResumed(item, bootAt)) {
    console.debug("[ADL] 跳过续传/历史条目", item.url, item.id);
    return;
  }
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

// ── 点图标 = 切换接管开关（用户 m24756：不用打开那个页面）──
chrome.action.onClicked.addListener(async () => {
  const next = !(await getEnabled());
  await setEnabled(next);
  // give the user a hint without a popup: the badge flips, plus a title update.
  console.log("[ADL] 接管已" + (next ? "开启" : "关闭"));
});

// ── 右键菜单：链接 / 页面 / 图标本身 ──
const MENU_LINK = "adl-download-link";
const MENU_PAGE = "adl-download-page";
const MENU_ACTION_DL = "adl-action-download-page";
const MENU_ACTION_OPEN = "adl-action-open";

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
    // 右键扩展图标：原来弹窗里的那两个动作搬到这里。
    chrome.contextMenus.create({
      id: MENU_ACTION_DL,
      title: "用 ADL 下载当前页",
      contexts: ["action"],
    });
    chrome.contextMenus.create({
      id: MENU_ACTION_OPEN,
      title: "打开 ADL",
      contexts: ["action"],
    });
  });
}

chrome.runtime.onInstalled.addListener(async () => {
  createMenus();
  await paintBadge(await getEnabled());
});
chrome.runtime.onStartup.addListener(async () => {
  createMenus();
  await paintBadge(await getEnabled());
});

async function activeTabUrl() {
  try {
    const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });
    return tab || null;
  } catch (_) {
    return null;
  }
}

chrome.contextMenus.onClicked.addListener(async (info, tab) => {
  if (info.menuItemId === MENU_ACTION_OPEN) {
    await getBridge({ force: true }) || (await launchAdl());
    return;
  }
  let url = null;
  if (info.menuItemId === MENU_LINK) url = info.linkUrl;
  else if (info.menuItemId === MENU_PAGE) url = tab && tab.url;
  else if (info.menuItemId === MENU_ACTION_DL) {
    const t = tab || (await activeTabUrl());
    url = t && t.url;
  }
  if (!url) return;
  const ok = await deliver(url, {
    referer: info.pageUrl || (tab && tab.url),
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

// ── 其它页面/诊断来的消息 ──
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
      const v = await setEnabled(msg.enabled);
      sendResponse({ ok: true, enabled: v });
      return;
    }
    if (msg.type === "downloadCurrent") {
      const ok = await deliver(msg.url, { referer: msg.referer, out: baseName(msg.url) });
      sendResponse({ ok });
      return;
    }
    if (msg.type === "openAdl") {
      const hit = (await getBridge({ force: true })) || (await launchAdl());
      sendResponse({ ok: !!hit, port: hit ? hit.cfg.port : 0 });
      return;
    }
    sendResponse({ ok: false, error: "unknown type" });
  })();
  return true; // 异步响应
});

// service worker 每次醒来都对一次角标（浏览器重启后角标状态可能丢）
paintBadge(getEnabled());
