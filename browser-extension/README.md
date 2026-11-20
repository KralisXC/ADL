# ADL Download Bridge（浏览器扩展）

把浏览器里的下载转交给 ADL 下载器。支持 Edge / Chrome（都是 Chromium MV3）。

## 原理

```
浏览器下载链接
   │  chrome.downloads.onCreated
   ▼
Service Worker ──POST http://127.0.0.1:17800/add──▶ ADL 桌面程序
   │                                                   │
   │◀───────────── 200 {"ok":true} ────────────────────┘
   ▼
chrome.downloads.cancel() + erase()   // 取消浏览器原生下载
```

- 若 ADL 没运行 / 接管开关关闭 / 请求失败 → **不取消**，浏览器照常下载（不会丢链接）。
- 需要登录的站：扩展会把该 URL 的 Cookie 一起转发给 ADL。

## 安全（三道防护，缺一即漏洞）

1. ADL 的本地 API **只绑 127.0.0.1**，外网不可达。
2. 会改状态的接口（`/add` `/probe` `/check`）必须带**随机 token**（启动时生成，经 `/auto-token` 下发给扩展）。
3. 校验 **Origin**：只接受空 Origin（MV3 后台直发）或 `chrome-extension://` / `moz-extension://`；普通网页无法省略 Origin，因此拿不到 token、也调不动。

## 安装（Edge / Chrome）

1. 先启动 ADL 桌面程序（默认监听 `127.0.0.1:17800`）。
2. 打开 `edge://extensions`（Edge）或 `chrome://extensions`（Chrome）。
3. 打开右上角「开发人员模式 / Developer mode」。
4. 点「加载解压缩的扩展 / Load unpacked」，选择本目录 `browser-extension/`。
5. 点扩展图标 → 勾选「接管浏览器下载」→「测试连接」应显示 ✅。

## 设置

- **接管开关**：关掉后浏览器完全自己下载。
- **端口**：默认 17800，需与 ADL 一致（ADL 侧可用环境变量 `ADL_API_PORT` 修改）。

## 注意

- ADL 必须处于运行状态（当前版本关闭窗口即退出，未做「关闭到托盘」）。
- 文件权限仅 `downloads / cookies / storage / contextMenus` + `host_permissions`，不注入页面脚本。
