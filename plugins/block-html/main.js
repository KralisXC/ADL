// Block HTML
// 服务器返回 200 但内容是 text/html —— 多半是错误页 / 登录页 / "文件不存在"，不是真文件。
// 直接拒绝，避免落一个几 KB 的假文件。
//
// 注意：如果你确实要下载 .html 网页，请关掉这个插件。
adl.onResponse((r) => {
  const ct = String(r.contentType || "").toLowerCase();
  if ((r.status === 200 || r.status === 206) && ct.indexOf("text/html") >= 0) {
    return { action: "deny", reason: "服务器返回的是 HTML 页面（疑似错误页/登录页），已拒绝写入" };
  }
  return { action: "pass" };
});
