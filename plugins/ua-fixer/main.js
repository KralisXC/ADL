// UA Fixer
// 在 onResolve 阶段（探测之前）就把 UA 换掉 —— 这样连"探测请求"都用正确 UA，
// 需要特定 UA 才给访问的站点才能过第一关。
//
// 下面两个常量按需修改：
const UA = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36 Edg/131.0.0.0";

// 空数组 = 对所有站点生效；否则只在列出的域名（含子域）生效。
const ONLY_HOSTS = [];
// 例：const ONLY_HOSTS = ["pan.baidu.com", "yun.baidu.com"];

adl.onResolve((req) => {
  let host = "";
  try { host = new URL(req.url).hostname; } catch (e) { host = ""; }
  if (ONLY_HOSTS.length > 0) {
    const hit = ONLY_HOSTS.some((h) => host === h || host.endsWith("." + h));
    if (!hit) return { action: "pass" };
  }
  return { action: "rewrite", userAgent: UA };
});
