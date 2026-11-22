# ADL 设计文档 v2

> **ADL** = A Downloader, Lightweight.
> 定位：**小、快、插件为大**。个人 + 小圈子自用，开源但不追求生态。
> 本文是前期方案。第 9 节的技术假设已全部实测通过，第 10 节是真实风险清单。
>
> **v2 相对 v1 的改动**（2026-11-20）：前端从"Vue 运行时 + 零构建"改为 **Vue SFC + vite**（用户已批准，§2）；
> 引擎从 Rhai/WASM 改为 **只有 QuickJS**（§4.1）；TLS 从 `rustls-tls` 改为 **`rustls-platform-verifier`**（§7.1，实测 webpki-roots 在本机不可用）；
> 单进程（砍掉提权服务，§1.1）；预分配只用 `trunc`（§3.6）；分片表用 `(offset, len, owner)` 而非 aria2 的 piece 位图（§3.3）。

---

## 0. 定位与范围

### 0.1 从"向大了做"转向"够用就行"

上一版（Aria2-Harness）是**向大了做**的产物：三进程、双引擎、aria2 全量源码、266 MB 安装包、11k 行前端。
实测用下来结论是：**现在除了重装系统，基本没有多任务下载场景**。所以 ADL 反过来做。

| 项 | Aria2-Harness | ADL |
|---|---|---|
| 进程 | 3（UI / 宿主 / 提权服务） | **1** |
| 引擎 | aria2c.exe + 自研 engine-net | **只有自研** |
| 安装包 | 266 MB（Full）/ 13.7 MB（Lite） | **目标单 exe ≤ 3 MB** |
| 前端 | Vue 3 SFC + vite + TS + naive-ui（11k 行） | **Vue 3 SFC + vite**（无组件库，产物 63 KB） |
| 插件 | QuickJS + Wasmtime 双后端 | **只有 QuickJS** |
| 依赖 | VCRUNTIME140.dll 等 | **crt-static，零运行时依赖** |

### 0.2 四条不可让的约束

1. **单 exe，零运行时依赖**。残缺版 Windows 上双击就能跑。
2. **插件是一等公民**，不是事后加的扩展点。内置功能全部写成插件（dogfood）。
3. **`cargo build` 之后能跑**。前端构建产物 `dist/` 提交进 git，全新克隆不装 Node 也能编译出能用的 exe。
4. **前端体积预算 100 KB 以内**（实测 vite 构建 63 KB）。

### 0.3 明确不做

- HTTP/3、BT/磁力、Metalink、RPC 兼容 aria2 JSON-RPC（错误码数值兼容，协议不兼容）
- 插件市场、账号体系、云同步
- 跨平台（先只做 Windows，但核心不写 Windows-only 代码）

---

## 1. 架构总览

### 1.1 单进程分层

```
┌─────────────────────────────────────────────────────┐
│  adl.exe                                            │
│                                                     │
│  ┌───────────────────────────────────────────────┐  │
│  │ UI 线程（主线程，tao 事件循环）                │  │
│  │   窗口 / 托盘 / WebView2 / IPC handler        │  │
│  └────────────────┬──────────────────────────────┘  │
│                   │ EventLoopProxy<UserEvent>       │
│  ┌────────────────▼──────────────────────────────┐  │
│  │ adl-core（tokio runtime，多线程）              │  │
│  │   引擎 / 分片调度 / 事件总线 / hook 管线       │  │
│  └────────────────┬──────────────────────────────┘  │
│                   │ 每条插件一条专属 OS 线程       │
│  ┌────────────────▼──────────────────────────────┐  │
│  │ 插件宿主（QuickJS Runtime × N，!Send）         │  │
│  └───────────────────────────────────────────────┘  │
│                                                     │
│  本地 HTTP server（127.0.0.1:0）—— 静态前端 + 浏览器接管 API │
└─────────────────────────────────────────────────────┘
```

**为什么单进程**：上一版拆三进程的唯一正当理由是 **falloc 预分配需要管理员权限**（`SE_MANAGE_VOLUME_NAME`）。
ADL 用 `trunc`（`SetFileEnd` 只改元数据，瞬时完成，不需要管理员），这个理由消失了。
单进程省掉：命名管道 IPC、SDDL 安全描述符、`SeSecurityPrivilege`、Job Object 守护、提权自检、三套错误传播。

**线程模型三条铁律**：
1. `tao::Window` / `wry::WebView` **都不是 `Send`**，只能活在主线程闭包里，不能塞 `Arc<Mutex<>>`。
2. QuickJS `Runtime` / `Context` 是 `!Send`，**每个插件一条专属 std 线程**，卸载 = `drop` + `join`。
3. 插件线程**绝不碰 tao/wry 类型**。上报事件靠 `impl Fn(UserEvent) + Send + 'static` 闭包。

### 1.2 crate 结构

```
adl/
├── Cargo.toml              # workspace
├── crates/
│   ├── adl-core/           # 引擎、分片调度、事件总线、hook 管线（零 UI 依赖）
│   ├── adl-plugin/         # manifest、权限、QuickJS 宿主、slot
│   └── adl-app/            # 唯一二进制：CLI + GUI + 托盘 + 本地 HTTP
├── web/                    # Vue SFC + vite（构建产物 dist/ 提交进 git）
│   ├── package.json        # 4 个 devDependency，无组件库
│   ├── vite.config.ts
│   ├── index.html
│   ├── src/
│   │   ├── main.ts
│   │   ├── App.vue
│   │   ├── ipc.ts          # 协议原样搬上一版
│   │   ├── views/{Downloads,Settings,Plugins}.vue
│   │   └── components/*.vue
│   └── dist/               # 65 KB，include_dir! 内嵌，随 git 走
├── plugins/                # 内置插件（dogfood）
└── docs/
```

**只有一个二进制**。CLI 模式和 GUI 模式靠参数区分，不靠 feature 拆 crate —— 因为要复用同一个本地 HTTP server 和同一套 IPC 路由，拆开反而重复代码。

### 1.3 体积预算（实测）

| 组成 | 实测体积 |
|---|---|
| tao + wry + tray-icon | 1.20 MB |
| + rquickjs | +0.33 MB |
| + serde / serde_json | +0.33 MB |
| + tokio + reqwest(rustls, http2) | +1.55 MB |
| **GUI 全功能合计（不含前端）** | **3.08 MB** |
| 纯 CLI（clap + tokio + reqwest + serde） | **2.06 MB** |

前端（Vue SFC + vite 构建后，`include_dir!` 内嵌）：

| 方案 | 产物 |
|---|---|
| vite 构建，只引 vue（**采用**） | **64,520 B（63.0 KB，gzip 25.5 KB）** + CSS 278 B + HTML 290 B |
| vite + naive-ui（只用 NProgress + NButton） | 155,367 B（151.7 KB）→ **不用** |
| 零构建全量 `vue.global.prod.js` | 170,605 B（166.6 KB）→ **不用** |

全部 `-C target-feature=+crt-static`，导入表只剩 8–20 个系统 DLL，**无 vcruntime140 / msvcrt / api-ms-win-crt-\***。

> ⚠️ GUI 版 3.08 MB，比 3 MB 目标超 80 KB。**vite 的 tree-shaking 正好解决这件事**：零构建方案前端占 166.6 KB，vite 构建后只占 63.0 KB，省下 103.6 KB —— 补上 80 KB 缺口后还剩 23 KB 余量，前端再加两个页面也压得住。
> 所以"体积 ≤ 3 MB"这个目标在 **Vue SFC + vite** 方案下重新成立，不需要换 hyper 手搓，也不需要砍 http2（见 §10.6）。

---

## 2. UI 层

### 2.1 为什么上次觉得麻烦（重新归因）

上一版的痛苦**不在 tao/wry**，也不在 Vue 本身，在**构建链的边界没划清**：

| 痛点 | 实测真相 |
|---|---|
| "`node_modules` 几百 MB" | 其实只有 **77.2 MB**（34 包）；带 naive-ui 才 149.2 MB |
| "`target/` 几个 G" | **这一条是真的，但它是 Rust 的**：单个 release target 673–694 MB，是 node_modules 的 4~9 倍 |
| "`dist/` 要跟 exe 走" | 确实是个耦合，但产物只有 **65 KB**，提交进 git 就消失了 |
| 每个调用手写 `ipc.postMessage` | Tauri 帮你做了这层胶水 —— 上一版已经自己写了 `ipc.ts`，这层其实**不是问题** |

**所以结论要改**：真正的问题不是"有构建链"，而是**没想清楚构建链的边界**（产物多大、谁提交、缺了怎么办）。
**保留 Vue SFC + vite，把边界用三条护栏钉死**（§2.3），代价是 65 KB 的 `dist/` 和一个可逆的构建顺序。

**而且构建链是净收益**：vite 的 tree-shaking 把 Vue 运行时从 166.6 KB 砍到 63.0 KB，**省下的 103.6 KB 正好补上 GUI 版超 3 MB 的 80 KB 缺口**（§1.3）。

### 2.2 方案：Vue SFC + vite（构建链只在开发期存在）

**先说清楚上一版到底贵在哪**（实测，不是估计）：

| 项 | 实测 |
|---|---|
| `node_modules`（vue + vite + TS，无组件库） | **77.2 MB**（34 个顶层包，1,451 文件） |
| `node_modules`（+ naive-ui） | 149.2 MB（58 包，17,128 文件） |
| 单个 Rust `target/`（release） | **673–694 MB** |
| Rust `target/`（release + debug 并存） | **933 MB** |
| `cargo clean` 后全量 release 重建 | **213 秒** |

→ **一个 Rust target 目录是 node_modules 的 4~9 倍。**"构建超过 4 GB 要清一下"这件事，几乎全部由 Rust 触发，不是前端。
→ 而且 `target/` 是**可以随时删的缓存**，`node_modules` 也是（`npm install` 19.5 秒）。**真正的成本不是磁盘，是构建耦合**（见 §2.3）。

**方案**：正常 Vue 3 单文件组件 + vite 构建，产物 `include_dir!` 内嵌进二进制。

```
web/
├── package.json           # 只有 4 个 devDependency
├── vite.config.ts
├── index.html
├── src/
│   ├── main.ts
│   ├── App.vue
│   ├── ipc.ts             # 复用上一版协议（§5.3）
│   ├── views/{Downloads,Settings,Plugins}.vue
│   └── components/*.vue   # 手写，不用组件库
└── dist/                  # 构建产物，提交进 git
```

依赖清单（实测 19.5 秒装完，77 MB）：

```json
{
  "dependencies": { "vue": "^3.5.44" },
  "devDependencies": {
    "@vitejs/plugin-vue": "^6.0.7",
    "typescript": "~6.0.3",
    "vite": "^8.0.16",
    "vue-tsc": "^3.3.5"
  }
}
```

**不用 naive-ui**。实测只引 `NProgress` + `NButton` 两个组件，包从 64.5 KB 涨到 155.4 KB —— **+91 KB 买两个组件**。
下载器真正需要的十来个组件（进度条 / 按钮 / 列表行 / 输入框 / 下拉 / 模态 / 标签页 / tooltip / 开关 / 状态点）手写约 600–800 行 SFC，产出 30–40 KB，无版本升级风险。

**构建耗时（实测）**：

| 命令 | 耗时 |
|---|---|
| `npm install` | 19.5 秒（一次性） |
| `npx vite build` | **0.37 秒** |
| `npm run build`（= `vue-tsc --noEmit && vite build`） | 6.0 秒 |
| Rust debug 增量（改一行业务代码） | **2.7 秒** |
| Rust release 增量（lto=fat） | 24.6 秒 |

→ **`vue-tsc` 占完整构建一半以上时间**，拆成独立脚本：

```json
"scripts": {
  "dev":   "vite",
  "build": "vite build",
  "check": "vue-tsc --noEmit"
}
```

日常开发 `npm run dev`（HMR，改一行浏览器立刻变），只有出包才 `npm run build` + `cargo build --release`。

### 2.3 三条护栏：把"构建耦合"限制在可逆范围

引入构建链后唯一的真问题是：**`cargo build` 不再是"全部"**，`include_dir!` 要求 `dist/` 先存在。

| 护栏 | 做法 | 解决什么 |
|---|---|---|
| ① `dist/` 提交进 git | 65 KB，随代码一起走 | 全新克隆 → `cargo build` 直接能跑，不需要 Node |
| ② `build.rs` 检查 `dist/index.html` | 不存在就嵌入"前端未构建，请运行 `cd web && npm run build`"提示页 | 而不是编译失败 |
| ③ IPC 契约与前端框架无关 | `window.ipc.postMessage` / `fetch` 双路径（§5.3） | 想改回零构建，**Rust 侧一行不用改** |

**这个决定是可逆的**，这是它敢做的唯一理由。

### 2.4 保留了什么、付出了什么

| 得到 | 付出 |
|---|---|
| `.vue` 单文件 + scoped CSS | 需要 Node（v24 已装） |
| TS 类型检查（`npm run check`） | 两套工具链要都能跑（半年后 vite 可能要求 Node 26） |
| HMR（改一行立刻见） | 构建顺序：`npm run build` → `cargo build` |
| tree-shaking：前端 166.6 KB → **63.0 KB** | `dist/` 要提交进 git（65 KB） |
| 旧项目 11,207 行 Vue 代码**可直接翻着抄** | —— |

**可直接复用的旧代码**（`E:\Aria2-Harness-Dev\client\src\`）：
`components\NodeProgressBar.vue`（126 行，分片进度条）、`components\ThreadMatrixCard.vue`（346 行，连接矩阵）、`components\Sidebar.vue`（155 行）、`components\PageHeader.vue`（85 行）、`components\TopBar.vue`（413 行）、`ipc.ts`（协议原样搬）。
`views\` 下四个大文件（DownloadsView 2,423 行 / SettingsView 1,973 行 / PluginsView 1,783 行 / DashboardView 450 行）是**参考**而不是拷贝目标 —— 它们绑着 aria2 的 RPC 模型，ADL 的引擎契约不一样（§3）。

### 2.5 前端一律走本地 HTTP server（不内嵌协议）

前端资源 `include_str!` / `include_dir!` 内嵌进二进制，但**加载方式统一是 `http://127.0.0.1:<port>/`**，不用 `file://`，也不用 wry 自定义协议。

**理由**：WebView2 在 `file://` 下 CORS/同源限制会挡住 ES module 与 `fetch`。上一版踩过，改本地 server 后行为与 Vite dev 完全一致。

```rust
let listener = std::net::TcpListener::bind(("127.0.0.1", 0))?;  // 0 = 内核分配端口
let port = listener.local_addr()?.port();
```

**这个决策的额外收益**：WebView 窗口和"降级到默认浏览器"用的是**同一个 server**，降级只是"谁来打开这个 URL"的区别，不是两套代码。

### 2.6 托盘与窗口生命周期

托盘用 `tray-icon 0.25.1`：

```toml
tray-icon = { version = "0.25.1", default-features = false }
```

> ⚠️ **必须 `default-features = false`**。默认 feature 含 `muda-libxdo` + `libappindicator`（Linux 专用），实测报 `error: no matching package named 'libxdo' found` —— 这两个不是 crates.io 上的包（走 git 依赖），联网也装不上。关掉在 Windows 上零损失。

**窗口生命周期采用 Motrix 式"关闭即销毁、唤醒即重建"**：

- `CloseRequested` → 记录 `is_maximized()` / `outer_position()` → `drop(webview)` → `drop(window)` → 进程继续跑
- 托盘点击 → 窗口还活着就 `set_visible(true)`，否则全新建

**为什么不用 `set_visible(false)` 隐藏**：上一版实测，`set_visible(false)` 会让系统挂起 WebView2 合成器，唤回即**白屏**；而 `Arc<Mutex<Option<Window>>>` 装不下（`!Send`）。
销毁重建实测释放 379 MB（445.9 MB → 67.0 MB，`msedgewebview2` 6 进程 → 0），三轮无累积。

**防白闪**：新建窗口时 `with_visible(false)`，等 `PageLoaded` 再 `set_visible(true)`，另起 1.2s 兜底。

**`frontend_url()` 必须 `OnceLock<String>` 缓存**，否则每次重建窗口都新绑端口、新起 server 线程 → 泄漏。

### 2.7 WebView2 缺失的三级降级

```
1. adl.exe 启动 → 检测 WebView2 运行时
2. 缺失 → 起本地 HTTP，用默认浏览器打开同一 URL
3. 用户禁 GUI（--no-gui）→ 纯 CLI
```

因为前端是纯静态 + 走 HTTP，**第 2 条不需要改任何前端代码**，只把 `window.ipc.postMessage` 换成 `fetch`（`ipc.js` 里一个 `if`）。

---

## 3. 核心引擎

### 3.1 下载流水线

```
URL
 └─► [onResolve]        Transform · 3s   → Vec<Candidate>（站点适配：改写 URL/headers/cookie）
      └─► Range 探测     Range: bytes=0-0 → 206/200/416 判定
           └─► [onPlan]  Transform · 3s   → DownloadPlan { segments, concurrency, ua, referer }
                └─► trunc 预分配 + 分片表
                     └─► 并发分片 ─► [onBeforeRequest] Guard|Transform · 3s ─► HTTP
                          └─► [onResponse] Guard · 3s → Allow / Deny / Retry
                               └─► 写盘 + 更新位图
                                    └─► [onProgress] Observe · 异步丢弃
                                         └─► [onComplete] Observe|Transform
```

### 3.2 Range 探测（实测通过）

| 响应 | 含义 | 处理 |
|---|---|---|
| `206` + `Content-Range: bytes 0-0/N` | 支持 Range，总长 N | 分片下载 |
| `200` | 不支持 Range | 单流顺序写 |
| `416` + `Content-Range: bytes */N` | 空文件或越界 | 取 N，允许**一次**重规划（`restarted` 标志防重试风暴） |

已有完成位图时**不探测**（控制文件里的总长就是真相），直接发续传 Range。

**探测本身带重试**（DESIGN.md 补充，m01800）：探测是"门面"，16 并发下只要探测那一下被 429 就整单失败，所以探测对**临时性**错误（连接失败 / 超时 / 429 / 503 / 504 / 连接重置）指数退避重试，最多 5 次，退避 1.6s→3.2s→5s→5s→5s（带 0~250ms 抖动，封顶 5s）。404 / 401 / 网页误判 / Content-Range 不符等**永久**错误不重试，立即返回。全 429 的死服务器约 21 秒后 fail-fast（错误码 29）。

**206 响应必须校验 `Content-Range` 的 start 与请求一致**，否则 abort。
**`200` 且期望 start ≠ 0 必须 abort** —— 服务器忽略 Range 就意味着准备写坏文件。

### 3.3 分片表：从第一天就用 `(offset, len, owner)`

上一版（aria2）的抢占粒度被 piece 索引空间锁死：piece 索引被 `.aria2` 持久化，抢占只能整片放弃。
ADL **不复制这个结构**，分片表直接记：

```rust
struct Segment {
    offset: u64,
    len: u64,
    state: SegState,   // Missing | Active{owner, token} | Done
}

struct PieceState {
    segments: Vec<Segment>,
    bitfield: Vec<u8>,   // 按 1 MiB 粒度记完成情况，用于续传
}
```

- **记账粒度**：`(offset, len, owner)`，抢占可以只砍一部分
- **持久化粒度**：1 MiB 位图（`DEFAULT_PIECE_LENGTH = 1024 * 1024`）
- 两者解耦，互不影响

> **M1 实测教训（血的）**：解耦是对的，但**恢复时必须把位图逐 piece 还原成 1 MiB 的段，绝不能把连续同类 piece 合并成大段**。
> M1 第一版 `from_bitfield` 为了"省内存"把 92 MiB Done + 3059 MiB Missing 合并成 2 个段，结果 `report_run_progress` 拿"段"当"piece"算（`piece_len = seg.len()`），3 GB 的段永远写不满 → **一个 bit 都不置**。
> 症状极具欺骗性：进度条从 59 MB 涨到 166 MB（`completed()` 累加 `written`），但控制文件里位图纹丝不动，重启又从 92 MiB 开始。
> 正确做法：`from_bitfield` 逐 piece 推入（3.3 GB / 1 MiB = 3151 个段，内存 ~100 KB，完全不是问题），请求级合并交给 `next_run()` 单独负责。两件事必须分开：
> - **记账粒度** = 1 piece = 1 segment（谁写满了谁置位，位图才动）
> - **请求粒度** = `next_run()` 把最靠前的一串连续 Missing 合成一次 Range 请求（避免 3151 个请求）

**默认参数**（沿用上一版实测值）：

| 参数 | 值 | 依据 |
|---|---|---|
| `min_split_size` | 20 MiB | aria2 `--min-split-size` 默认 |
| 并发分片数 | **期望上限默认 16（夹到 [1,64]），引擎自适应收敛到服务器能接受的量** | 真实网络 16 并发基本能跑满；服务器只放行一部分时按实际放行数跑，再慢慢试探 +1 |
| 分片下限约束 | 实际派发数还受 `total / min_split_size` 与剩余分片数约束，`total < 2×min_split_size` 时强制 1 | 避免小文件开一堆无意义连接 |
| 单片粒度 | 1 MiB | 位图精度与内存的平衡 |
| 僵死阈值 | 12s 无进展 | `--dynamic-slicing-stall` 默认 |
| 单分片最大抢占次数 | 2 | `--dynamic-slicing-max-preempts` 默认 |
| 抢占冷却 | 5s | 防毫秒级乒乓 |

### 3.4 调度器：单写者 + 时效 token

**唯一能改分片状态的只有调度协程**。worker 只通过事件通道上报，事件带 `(worker_id, token)`。

```rust
enum PieceState {
    Missing,
    Assigned { owner: u32, token: u64 },
    Downloading { owner: u32, token: u64, written: u64, last_progress: Instant },
    Reclaiming { owner: u32, token: u64, written: u64 },
    Done,
}
```

- **迟到事件一律丢弃**：`token` 不匹配说明这是被抢占前发出的旧事件
- **抢占两阶段**：`Downloading → Reclaiming`（已发 Stop）→ 收到 `Stopped` 才回 `Missing`
  （不能直接回 `Missing`，否则 worker 还在写同一区间，两个 worker 交叉写会写坏文件）
- **`Started` 在发请求之前上报**：僵死时钟从派发即开始计时，服务端迟迟不发响应头也能被抢占
- **`Stop` 立即 drop body**（HTTP/2 为 `RST_STREAM`），已写字节留在文件里但整片重下

**M1 已落地的单写者 API**（`E:\ADL\crates\adl-core\src\segment.rs`，无 worker 的简化版，M2 扩展）：

```rust
// 请求级：把最靠前的连续 Missing 合成一次 Range
pub fn next_run(&self) -> Option<(usize, usize)>            // (起始下标, 数量)
pub fn assign_run(&mut self, start: usize, count: usize, owner: u32, token: u64) -> bool
// 记账级：written 是从区间起点算起的累计字节，写满的 piece 立刻转 Done
pub fn report_run_progress(&mut self, start: usize, count: usize, token: u64, written: u64) -> u64
pub fn finish_run(&mut self, start: usize, count: usize, token: u64) -> bool  // true = 仍有未完成 piece
```

`report_run_progress` 返回"本段新确认完成的字节数"，引擎拿它决定是否要落控制文件 —— 这个返回值就是**续传进度的唯一真相源**，不要另算。

**为什么必须单写者**：分片状态机的 bug 全是竞态。上一版用 696 行 Rust + 多轮实测才收敛（报告里记录了 208 次抢占 / 8 个 piece 的毫秒级乒乓、828 条 `No segment available` 的命令链扇出）。单写者把并发点从 N 个降到 1 个。

### 3.5 断点续传

控制文件 `<target>.adl`（JSON，**原子写**：写 `.tmp` 再 `rename`）：

```json
{
  "version": 1,
  "url": "https://...",
  "etag": "\"abc123\"",
  "last_modified": "Wed, 21 Oct 2015 07:28:00 GMT",
  "total": 1073741824,
  "piece_length": 1048576,
  "bitfield": "base64...",
  "sha256": null
}
```

**恢复流程**：

1. 发 `Range: bytes=<first_missing>-<end>`
2. **服务器返回 200 而不是 206 → 文件已变（或服务器不支持 Range），整个重下**（这条规则避免 90% 的续传损坏）
3. 返回 206 → 校验 `Content-Range` 起点一致，继续；同时**比对响应里的 `ETag` / `Last-Modified` 与控制文件记录是否相同**，不同则整个重下

> **不要用 `If-Range` 请求头**（M1 实测纠正）。直觉上 `If-Range: <etag>` 是最优雅的"变了就重下"机制，但实测它让服务器慢 **7~9 倍**：
>
> | 镜像 | 无 `If-Range` | 有 `If-Range` |
> |---|---|---|
> | tuna | 4.31~5.29 MB/s | 0.50~0.67 MB/s |
> | ustc | 4.40 MB/s | 0.50 MB/s |
>
> （10 MB 封闭 Range，各 3 轮。开放式 Range `bytes=N-` 两种情况都跑满超时，看不出差异，必须用封闭 Range 才测得出。）
> 根因：nginx 见到 `If-Range` 会禁用 `sendfile` 快速路径，退回用户态慢速拷贝。
> **改成"先拿 206 再比 ETag"**：安全性完全等价（文件真变了照样整单重下），但服务器走快路径，性能恢复满速。

**位图精度必须与控制文件里的 `piece_length` 绑定**：换了 `piece_length`，1 bit 就对应不同的字节区间 → 文件静默损坏。所以控制文件里存 `piece_length`，不匹配则返回 `CANNOT_RESUME`（错误码 8），拒绝续传。

### 3.6 预分配：只用 `trunc`

| 方式 | 语义 | 需要管理员 | 采用 |
|---|---|---|---|
| `trunc`（`SetFileEnd`） | 只改文件长度元数据，**不写零** | ❌ | ✅ |
| `falloc`（`SetFileValidData`） | 推 VDL，真占盘 | ✅ `SE_MANAGE_VOLUME_NAME` | ❌ |
| `prealloc`（写零） | 真写零，阻塞 | ❌ | ❌ |

> **关键事实 1**：非管理员调用 `falloc` **不会退化成 `trunc`**，而是退化成"阻塞填零拖死整个引擎且零提示"（aria2 源码注释原话：`File will be allocated by filling zero, which blocks whole aria2 execution.`）。
> 这是上一版引入提权服务的唯一原因。ADL 用 `trunc`，问题消失，进程数从 3 降到 1。

> **关键事实 2（M1 实测纠正）**：`trunc` 只省掉"写零"的时间，**不省磁盘空间**。NTFS 上 `set_len(total)` 会真实预留簇：
> ```
> fsutil sparse queryflag <file>  → This file is NOT set as sparse
> compact /q <file>              → 3,303,444,480 total bytes of data are stored in 3,303,444,480 bytes
> ```
> 即预分配 3.3 GB 就是立刻吃掉 3.3 GB 可用空间，磁盘不够时**启动即 `DiskFull(34)`**（M1 首次真实续传测试就栽在这里，被误判成"续传不工作"）。
> 三个选项，ADL 当前选第 1 个：
> 1. **接受真实占盘**（现状）：下载大文件前用 `GetDiskFreeSpaceExW` 预检 `total × 1.05 + 2 GiB` 余量，不够直接报 34 并提示剩余空间 —— 行为可预期，且用户删掉文件空间立刻回来。
> 2. **改用稀疏文件**（`FILE_ATTRIBUTE_SPARSE_FILE` + `FSCTL_SET_SPARSE`）：`set_len` 变成元数据操作，磁盘按实际写入增长。代价：碎片化严重、`fsutil sparse` 之外的工具（备份/压缩/某些杀软）会展开它、NTFS 上随机写稀疏区可能触发簇分配抖动。**M2 分片并发时再评估**。
> 3. 不预分配：写入时自然增长。代价：`seek` 到远处写会先填零（等于回到 `prealloc` 的毛病），所以**不能用**。
> 预检这件事 M1 还没做（当前是 `set_len` 失败才报 34），M2 补。

### 3.7 错误码（兼容 aria2 数值）

```rust
enum AdlError {
    Unknown = 1, Timeout = 2, ResourceNotFound = 3, MaxFileNotFound = 4,
    SpeedTooSlow = 5, Network = 6, ResumeNotSupported = 8,
    PieceLengthChanged = 10, DiskFull = 34, ...
}
```

| 条件 | 码 | 处理 |
|---|---|---|
| `403` / `429` | 3 | 自适应降并发：每遇一次容量失败 `effective -= 1`（不低于实际跑通数），收敛到服务器能接受的量；稳定后每 `probe_interval` 试探 +1，失败则翻倍间隔（20s→…→300s） |
| 连接被拒/不可达（`Network`） | 6 | 同样计入容量降档；但**不惩罚具体分片**（不计 attempts），退避 300ms 起、封顶 2s |
| `401` / `407` | 33 | 弹认证，不自动重试 |
| `404` | 3 | 直接失败 |
| `502` / `503` / `504` | 29 | 指数退避重试 |
| 连接重置 | 35 | 熔断计数 ≥3 后转 36（`NETWORK_BLOCKED`） |
| 磁盘满（`ERROR_DISK_FULL` 112 / `ENOSPC` 28） | 34 | 立即失败，保留位图 |
| 位图 `piece_length` 变了 | 10 | 拒绝续传 |
| 其它续传失败 | 8 | 拒绝续传 |

**重试策略**：只重试 `Timeout | ConnectionReset | Network | 504`。`4xx` 不重试（除 429）。指数退避 + jitter。

### 3.8 限速、重定向、其它

| 项 | 方案 |
|---|---|
| 限速 | 令牌桶，全局 + 单任务两级，取更严格者 |
| **连接策略** | **每个分片一条独立连接**（独立 `reqwest::Client`），不共用连接池。实测：每段独立 h2 = 5.78 MB/s，共享 Client 的 h2 多流 = 5.44 MB/s，h1.1 连接池 = 4.47 MB/s。单条 TCP 的拥塞窗口才是瓶颈，h2 多流替代不了多连接 |
| ALPN | 必须显式 `vec![b"h2", b"http/1.1"]`，否则永远协商到 HTTP/1.1（§7.1） |
| 重定向 | 最多 20 跳；**不自己解析相对 Location**（用 URL join）；记录跳转链供 hook 观察 |
| Cookie | 初始 Cookie 带到每一跳（跨 host 也不丢）；跳转响应的 `Set-Cookie` 只按域吸收 |
| 网页误下载拦截 | 成功响应 + `Content-Type: text/html` + 目标不是 `.html/.htm` → 报错，不当文件存 |
| 长路径 | `\\?\` 前缀（仅在确定是文件路径处加，不能改通用的 UTF-8→宽字符函数） |
| 文件名清洗 | 替换 `<>:"/\|?*` 与控制字符；剥尾部 `.` 和空格（同时防 `.`/`..` 穿越）；保留名 `CON/PRN/AUX/NUL/COM1-9/LPT1-9` 加前缀 |
| 分段切分 | **最后一段必须 `end = total - 1` 吃掉余数**。`base = total/n` 直接切会丢字节（实测 64 MiB / 6 段丢 4 字节） |

### 3.9 会话与退出

- 会话文件不存在时**先创建**（aria2 的坑：`--input-file` 指向不存在的文件会启动失败）
- 退出顺序：`save_session()` → `shutdown()` → 等 500ms → `kill` 兜底，整体 `timeout(1500ms)`
- **不要依赖定时保存会话**（上一版 `--save-session-interval=60` 实测靠不住：用户十几秒就关，session 恒 0KB）

---

## 4. 插件系统

### 4.1 引擎选型：只有 QuickJS

| 引擎 | 增量体积 | 缓存可用 | 结论 |
|---|---|---|---|
| **rquickjs 0.14** | +1.1 MB | ✅（vendored C 源码，实测离线构建成功） | ✅ **采用** |
| wasmi | +0.2 MB | ❌ 不在缓存 | ❌ 延后到 M7 |
| wasmtime 49 | +5~15 MB | ✅ | ❌ 太重 |
| rhai | +0.4 MB | ❌ 不在缓存 | ❌ 表达力不足 |
| boa_engine | +5 MB+ | ❌ | ❌ |

**决定性理由**：上一版的 4 个官方插件共 66 KB 已验证 JS 代码（hls-downloader 16.6 KB / live-recorder 17.3 KB / media-converter 6 KB / web-media-extractor 26.5 KB），换引擎全部作废。而且 DSH 的插件本身就是 JS —— 用户的直觉（"插件系统类似 deepseek harness"）指向 JS。

> WASM 不是被否掉，是**延后**。`hostapi` 的 `respond(op, payload) -> String` 是纯字符串协议，与 JS 引擎完全无关，将来加 WASM 只需 `linker.func_wrap("env", "host_call", ...)` 复用全部权限校验。M6 再说。

### 4.2 核心原则

> **能拒绝的权限比能改写的权限安全一个数量级。**

所以 hook 按**强度分档**，"安全模式"就是"只放行低档"。

| 档位 | 语义 | 能否改数据 | 默认 |
|---|---|---|---|
| `Observe` | 只读通知 | ❌ | ✅ |
| `Guard` | 返回 `Allow` / `Deny(reason)`，不能改 | ❌ | ✅ |
| `Transform` | 可改写请求/数据 | ✅ | ⚠️ 需授权 |
| `Replace` | 替换核心实现 | ✅✅ | ❌ 仅原生 |

### 4.3 Hook 点与同步性

**所有 hook 都是同步调用**（跨 dylib / JS 引擎的 `async` 是灾难），但分两类：

**A. 同步阻塞**（必须等结果才能继续，天生只能同步）：

| Hook | 档位 | 超时 |
|---|---|---|
| `onResolve(url) -> Vec<Candidate>` | Transform | 3s |
| `onBeforeRequest(&mut Request)` | Guard \| Transform | 3s |
| `onResponse(&Response) -> Action` | Guard | 3s |
| `onComplete(&Path, &Meta)` | Observe \| Transform | 3s |
| `onUiSlot(slot, props) -> Widget` | Transform | 1s |

**B. 异步丢弃**（每 256 KB 都停下来等插件会慢十倍）：

| Hook | 档位 | 队列 |
|---|---|---|
| `onProgress(done, total, speed)` | Observe | 有界 `sync_channel(256)`，满则丢弃并计数 |

**超时是硬性要求**：QuickJS 用 `set_interrupt_handler` 强制中断。实测死循环在 301ms 被中断（`Exception generated by QuickJS`）。

**管线语义**：
- `Guard`：全部跑，**任一 Deny 即 Deny**（短路）
- `Transform`：按 `priority` 升序链式，前一个输出是后一个输入
- `Replace`：按 `priority` 降序，**第一个返回 `Replaced` 的赢**
- `Observe`：全部跑，错误只记日志

### 4.4 插件能不能改 ADL 自己（类 DSH）

这是用户明确要的能力。DSH 的四档机制对应到 ADL：

| DSH | ADL 对应物 | 权限档 |
|---|---|---|
| `ctx.tools.restrict()`（只能删） | `onToolList` 只能过滤 | Observe |
| `ctx.tools.guard()`（只能拒） | `onCommandGuard` | Guard |
| waterfall 监听器（能改写，依赖注册顺序） | `onResolve` / `onBeforeRequest` / `onPlan` | Transform |
| `system-prompt/assemble` 整体替换 | `Replace` 档 | Replace（仅原生） |
| `ctx.slots.inject()` + React 组件 | `onUiSlot` + 声明式组件树 | Transform |
| `Config` schema + `cordis.patch.yml` | manifest 的 `[settings]` schema | — |

**插件可以做的事**：
- 加/改**菜单项**（`onMenu` → 声明式菜单条目）
- 加/改**配置项**（manifest `[settings]` schema，宿主纯 schema 驱动渲染，**零硬编码**）
- 往 UI 的 slot 里塞**声明式组件**（下载行按钮、详情页面板、状态栏胶囊）
- 改写请求（UA / Referer / Cookie / URL）
- 拦截并拒绝（Guard）

**插件不能做的事**（`--safe` 下）：
- 执行任意 JS（拿不到 DOM，只能返回 JSON）
- 读 cookie、读文件、发网络请求（除已授权）

### 4.5 Manifest

```toml
[plugin]
id          = "com.adl.bilibili"
name        = "B站适配"
version     = "0.1.0"
api_version = 1
kind        = "js"              # js | native
entry       = "main.js"
priority    = 100

[permissions]
net        = ["api.bilibili.com:443", "*.bilivideo.com:443"]
fs_read    = ["${download_dir}/**"]
fs_write   = ["${download_dir}/**"]
notify     = true
downloads  = ["read", "write", "intercept"]

[hooks]
onResolve       = { priority = 100 }
onBeforeRequest = { priority = 50 }
onUiSlot        = { priority = 100 }

[settings]
quality = { type = "select", label = "清晰度", options = ["1080P", "720P"], default = "1080P" }
cookie  = { type = "textarea", label = "Cookie", default = "" }

[limits]
timeout_ms      = 3000
task_timeout_ms = 1800000
max_http_bytes  = 8388608
memory_mb       = 64
```

- `api_version` 不匹配 → 直接拒载（唯一的 breaking-change 防线）
- `settings` 六类型：`boolean` / `string` / `text` / `textarea` / `number` / `select`，**纯 schema 驱动渲染，UI 里零按插件 id 分支的代码**

### 4.6 权限：声明 ≠ 授权

**两个独立集合**：

```rust
pub struct PluginHost {
    declared: Arc<HashSet<String>>,        // manifest 里写的
    granted:  Arc<RwLock<HashSet<String>>>, // 用户实际授权的
}
```

- 未在 manifest 声明 → 直接拒绝 + 审计
- 声明了但用户没授权 → 拒绝 + 审计（"声明≠授权"）
- 授权**即时生效**，无需重载插件

**唯一出口**：

```js
globalThis.__adl_host(op, payloadJson) -> string
```

所有权限校验、审计、metrics 全部收敛到这一个函数里。返回 `{"ok":false,"error":"权限不足"}`。

### 4.7 生命周期 = Effect（抄 DSH）

插件注册的一切都挂在自己的 `Effect` 上，**drop 即撤销**：

```rust
pub struct PluginCtx {
    effects: Vec<Box<dyn FnOnce() + Send>>,
}
impl PluginCtx {
    pub fn register_hook(&mut self, point: HookPoint, h: Box<dyn Hook>) { ... }
    pub fn register_slot(&mut self, slot: SlotId, w: Widget) { ... }
    pub fn on_effect(&mut self, f: impl FnOnce() + Send + 'static) { ... }
}
```

没有这一条，插件系统三个月后必然变成"重启才能卸载"的垃圾堆。

### 4.8 热重载：DSH 要重启，ADL 不用

| | DSH | ADL |
|---|---|---|
| 插件形态 | Node.js 模块 | QuickJS 嵌入式 |
| 卸载 | `require()` 后永久留在 Node 模块缓存，**Node 没有卸载 API** | `drop(runtime)`，整个 JS 堆一次性消失 |
| 结论 | 只能重启进程 | 安装/卸载/启停/改代码**全部即时生效** |

**实现**：`notify` 监听 + 独立 std 线程 + `recv_timeout(300ms)` 防抖 + **new-before-swap**（新实例建好、校验通过才原子替换；失败则旧实例分毫未动）。

**改权限的语义**：即时生效，但**新增权限必须强制禁用 + 弹窗确认**，绝不默默沿用旧授权。

**必须过滤的路径**：`registry.json`、`.tmp`、`.` 开头的文件 —— 否则"监听 → 重载 → 写 registry → 再触发"死循环。

### 4.9 熔断

连续 3 次 hook 失败 → 置 `enabled = false` + 审计 + 发事件给 UI，**但绝不 join/kill 插件线程**（线程杀了就没法恢复，只能重启）。

### 4.10 原生插件（`--unsafe-native`）

| 类型 | 隔离机制 | 能否强制 |
|---|---|---|
| **JS**（QuickJS） | 只注入已授权的 host 函数，没授权就没有这个函数 | ✅ |
| **Native**（libloading） | **无法强制**。dll 被加载后能读内存、能崩溃进程 | ❌ |

**必须诚实地写进文档：原生插件拦不住。** 给用户假的安全感比不给更糟。

### 4.11 安全模式

| 模式 | 允许插件类型 | 允许 Hook 档 | 网络 | 文件 |
|---|---|---|---|---|
| `--safe`（默认） | js | Observe, Guard | ❌ | ❌ |
| `--trusted` | js | + Transform + UI slot | 按 manifest | 按 manifest |
| `--unsafe-native` | + native | + Replace | 全开 | 全开 |

默认必须是 `--safe`。用户装插件看到的第一句话应该是"这个插件想访问网络，允许吗"。

### 4.12 Dogfood

**内置功能全部写成插件**：站点适配、后处理、通知、托盘菜单项。插件 API 就是内部模块边界，设计错了自己第一个疼。

---

## 5. UI 与插件的契约

### 5.1 事件流（core → 所有前端）

```rust
pub enum CoreEvent {
    TaskAdded   { id: TaskId, snapshot: TaskSnapshot },
    Progress    { id: TaskId, done: u64, total: Option<u64>, speed: u64 },
    Completed   { id: TaskId, path: PathBuf },
    Failed      { id: TaskId, error: String },
    PluginLoaded{ id: String },
    PluginError { id: String, error: String },
}
```

`tokio::sync::broadcast` 分发。CLI 渲染成行，wry 推给 JS，HTTP 走 SSE。

> ⚠️ **"发射即丢失"必踩的坑**：后端连上就 emit `connected`，前端 `listen()` 晚一步注册就永久卡"连接中"。
> **任何"状态就绪"事件都必须配一个可主动查询的幂等命令**（`getStatus()`）。

### 5.2 Slot：声明式组件树，不是 HTML

```json
{
  "type": "row",
  "children": [
    { "type": "text", "value": "B站 · 1080P" },
    { "type": "button", "label": "重新合并", "action": "bilibili.remux" }
  ]
}
```

**组件白名单**：`row` / `col` / `text` / `button` / `progress` / `badge` / `select` / `input` / `divider` / `link`。

**为什么不能返回 HTML 字符串**：那等于让插件执行任意 JS —— 能读 DOM、能 `fetch`、能偷 cookie，前面所有沙箱工作白做。

代价是表达力受限，但对下载器够用。需要复杂 UI 时**加新组件类型**，而不是放开 HTML。

### 5.3 IPC 协议（复用上一版已验证的）

前端 → 宿主：
```js
window.ipc.postMessage(JSON.stringify({ id, cmd, args }))
```

宿主 → 前端：
```js
window.__ipcResolve(id, result)     // result 为 {"__err":{code,message}} 表示失败
window.__ipcEmit(event, payload)    // 主动推送
```

`web/src/ipc.ts` 导出（原样搬上一版的 `client/src/ipc.ts`）：
```ts
export function invoke<T>(cmd: string, args?: unknown): Promise<T>
export function listen<T>(event: string, handler: (p: T) => void): () => void
export class IpcError extends Error { readonly code: string }
```

> ⚠️ **`evaluate_script` 绝不能在 `ipc_handler` 回调里同步调用**（会死锁）。必须 `proxy.send_event` 转到事件循环再执行。

**纯浏览器降级**：`if (!window.ipc) { /* 换 fetch('/api/rpc') */ }`，`ipc.ts` 里一个分支。

### 5.4 样式隔离

插件组件只能用宿主的 CSS 变量（`--adl-*`），不能写死颜色。主题切换自动生效。

---

## 6. 浏览器接管

| 方式 | 体验 | 工作量 | 采用 |
|---|---|---|---|
| 扩展 + 本地 API | 好 | 1–2 周 | ✅ 主路径 |
| 剪贴板监听 | 粗 | 半天 | ✅ 兜底 |
| `adl://` 协议 | 中（弹确认框） | 2 天 | 可选 |

### 6.1 本地 API 的三条 CSRF 防护（缺一即漏洞）

本地 API 是**真实的攻击面**：你访问的任何网页都能 `fetch('http://127.0.0.1:PORT/download?url=...')`，让下载器往磁盘写任意文件。

1. **只绑 `127.0.0.1`**，不是 `0.0.0.0`
2. **随机 token**：启动生成，扩展首次配对时存下，之后每个请求带 `Authorization`
3. **校验 `Origin` / `Sec-Fetch-Site`**，非扩展来源一律 403

> ⚠️ 上一版踩过的坑：MV3 Service Worker 声明了 `host_permissions` 后由网络栈直发请求，**实测不带 `Origin`**，而浏览器禁止网页脚本省略/伪造 `Origin`。
> 所以正确语义是 `origin.is_empty() || is_extension_origin(origin)` —— 放行空 Origin 是安全的，收紧会让扩展每 30s 心跳被 403。

### 6.2 扩展职责

扩展只做一件事：抓到 URL → `POST /api/download`。**所有逻辑留在核心**，改核心不用重发扩展。

**幽灵下载**：接管成功后必须 `chrome.downloads.erase()`（只 cancel 会在历史里留一条，用户点了会重复下载）。

---

## 7. 依赖与构建

### 7.1 关键版本

> 网络实测（2026-11-20）：crates.io / `registry.npmjs.org` / `registry.npmmirror.com` / 清华 tuna **全部可达**。下面的版本号是本机缓存里已验证能编译的，不是唯一可选。

```toml
tao                        = "0.37.1"
wry                        = "0.57.0"
tray-icon                  = { version = "0.25.1", default-features = false }
rquickjs                   = { version = "0.14.0", default-features = false, features = ["std"] }
tokio                      = "1.53.3"
reqwest                    = { version = "0.12.28", default-features = false,
                               features = ["rustls-tls-no-provider", "http2", "stream"] }
rustls                     = { version = "0.23.46", default-features = false,
                               features = ["ring", "std", "tls12"] }
rustls-platform-verifier   = "0.6.2"
serde                      = { version = "1.0.229", features = ["derive"] }
clap                       = { version = "=4.5.26", features = ["derive"] }
notify                     = "8.2.0"
```

**为什么 `rustls-tls-no-provider` + `rustls-platform-verifier` 而不是 `rustls-tls`**（§9 #13 有完整实测）：

```rust
use rustls::ClientConfig;
use rustls_platform_verifier::BuilderVerifierExt;

let mut cfg = ClientConfig::builder()
    .with_platform_verifier()?          // 用 Windows 证书库，不是内置 Mozilla 根
    .with_no_client_auth();
cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];   // ⚠️ 必须手设

let client = reqwest::Client::builder()
    .use_preconfigured_tls(cfg)
    .build()?;
```

### 7.2 踩过的坑（全部实测）

| 坑 | 症状 | 解法 |
|---|---|---|
| `rustls-tls`（= webpki-roots） | `invalid peer certificate: UnknownIssuer` —— 内置 Mozilla 根**不含** `CN=YR2, O=Let's Encrypt`（tuna 用它） | 换 `rustls-platform-verifier`（读 Windows 证书库） |
| `rustls-native-certs 0.8` | `failed to open current user certificate store: 拒绝访问 (os error 5)`，读到 **0 张**（PowerShell 能读 59 张） | 同上，别用它 |
| `use_preconfigured_tls` 不设 ALPN | 永远协商不到 h2（实测全是 `HTTP/1.1`） | `cfg.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()]` |
| MSVC `D8050`（debug 构建 `ring`） | `cl: 命令行 error D8050 :无法执行 ...c1.dll: 未能将命令行放入调试记录中` | **`[profile.dev.package."*"] debug = false`**（只给依赖关调试信息，自己的代码保留） |
| `tray-icon` 默认 feature | `error: no matching package named 'libxdo' found` | `default-features = false` |
| `reqwest` 的 gzip/brotli/deflate | 需要 `async-compression`，本机缓存没有（联网可装，但下载器不需要自动解压） | 去掉这三个 feature |
| `clap 4.6.1` | 需要 `clap_derive =4.6.1`，缓存只有 4.5.24 | 钉 `=4.5.26` |
| `rquickjs` 用 clang 编译 | `clang: error: no such file or directory: '/DWIN32_LEAN_AND_MEAN'` | **必须用 MSVC cl**（走 `vcvars64.bat`） |
| `ctx.eval::<String, _>()` | JS 返回 number 时 panic `FromJs { from: "int", to: "string" }` | 按实际类型写 `eval::<i64, _>()` |
| 续传分段整除 | `base = total/n` 直接切会**丢字节**（实测 64 MiB / 6 段丢 4 字节） | 最后一段 `end = total - 1` 吃掉余数 |

> **D8050 的无效解法**（都试过）：`CFLAGS_x86_64_pc_windows_msvc=/Zi`（cc-rs 仍先传 `/Z7`）、`[profile.dev] debug = 0`（整个 profile 丢调试信息）、手动钉 MSVC 14.44（ring 的 build script 仍解析到 14.51）。只有 `[profile.dev.package."*"] debug = false` 有效。

### 7.3 构建配置

```toml
[profile.release]
opt-level     = "z"
lto           = "fat"
codegen-units = 1
panic         = "abort"
strip         = true

# 唯一能解 MSVC D8050 的开关：依赖不生成 debuginfo，自己的代码照常
[profile.dev.package."*"]
debug = false
```

```toml
# .cargo/config.toml
[target.x86_64-pc-windows-msvc]
rustflags = ["-C", "target-feature=+crt-static"]
```

**构建顺序**（前端先进包，Rust 才能 `include_dir!`）：

```powershell
call "D:\Microsoft Visual Studio\18\Community\VC\Auxiliary\Build\vcvars64.bat"   # QuickJS 的 cc 需要 cl
cd web; npm run build; cd ..                                                    # → web/dist/（0.37 秒）
cargo build --release
```

> 日常开发不必走全流程：Rust 侧 `cargo build`（debug 增量 2.7 秒），前端侧 `cd web; npm run dev`（HMR，直连 `127.0.0.1:1420` 的 mock 后端）。
> `panic = "abort"` 下任何 `expect` 失败都是 exit code `-1073740791`（`0xC0000409 STATUS_STACK_BUFFER_OVERRUN`），不是普通 exit 1。看日志时别被吓到。

---

## 8. 里程碑

| 阶段 | 内容 | 产出 | 预估 | 状态 |
|---|---|---|---|---|
| **M1** | 单流下载 + 断点续传 + 进度 | `adl <url>` 能用 | 3–4 天 | ✅ **已完成** |
| **M2** | 分片并发 + 限速 + 重试 + 降档 | 大文件可用 | 5–7 天 | ✅ **已完成** |
| **M3** | hook 管线 + QuickJS + `--safe` | 第一个内置插件跑通 | 5–7 天 | ✅ |
| **M4** | CLI 完整（配置文件 + 下载历史 + 子命令） | **可发布 v0.1** | 3–5 天 | ✅ |
| **M5** | wry 窗口 + Vue SFC 前端（vite 构建） | GUI 可用 | 7–10 天 | ✅ |
| **M6** | 浏览器扩展 + 本地 API | 接管下载 | 7–14 天 | ✅ |
| **M7** | WASM 后端（可选） | 插件双引擎 | 3–5 天 | ⬜ |

**M1 实际交付**（`E:\ADL`，workspace `crates/adl-core` + `crates/adl-app`）：
- `adl-core` 八个模块：`error.rs`（aria2 兼容错误码 + `is_retryable`/`is_rate_limited`）、`path.rs`（`sanitize_name`/`is_within`/`to_long_path`/`split_segments` 14 测试）、`http.rs`（platform-verifier + 显式 ALPN）、`segment.rs`（单写者分片表 + 位图 roundtrip 11 测试）、`control.rs`（`.adl` 原子写 + BOM 兼容 8 测试）、`hooks.rs`（六点 `trait Hooks` 全空实现，M3 填）、`probe.rs`（Range 探测 6 测试）、`engine.rs`（九步下载流程 4 测试）
- 48 单元测试全绿，exe **2.10 MB**，真实网络续传 6.3 MB/s
- **M1 只是单连接顺序下载**：`next_run()` 把连续 Missing 合并成一次 Range，一次只跑一个请求。M2 才是并发 worker + 抢占

**M2 实际交付**（`adl-core/src/{segment,engine,hooks}.rs` + `adl-app/src/main.rs`）：
- **调度器单写者**：一个调度协程独占 `SegmentTable`，worker 通过 `mpsc` 上报 `{Started,Progress,Done,Stopped,Failed}`，事件带 `(id,token)`，token 不匹配的迟到事件丢弃。`tokio::select!` 在"事件 / 500ms tick"之间循环。
- **worker**：领一段连续 Missing piece（`next_run_limited(chunk_pieces)`，chunk 默认 4 MiB），发一个封闭 Range 请求；每写 64 KB 发一次 `Progress`（累计 `written`），写满的 piece 立刻转 `Done`（续传记账点）。
- **抢占（两阶段）**：`Active→Reclaiming`（调度器 `watch::send(true)` 发 Stop）→ worker 在下一个 chunk 边界确认 `Stopped` → `finish_reclaim_run` 退回 Missing 改派。已 `Done` 的 piece 保留。判据：12 秒无新字节且 `written/elapsed < 100 KB/s`，单片 `preempts < 2` 防乒乓。实测单测覆盖 `reclaim_run_preserves_done_pieces` / `reclaim_accepts_done_racing_in`（worker 收 Stop 同时写完整段时改发 `Done`）。
- **限速**：全局限速器 `RateLimiter`（虚拟时钟令牌桶，多 worker 共享），`--limit 2M` 实测 8 MB 文件 4.07s、2.0 MB/s，精确。
- **重试**：可重试错误（超时/重置/网络/504）按 `400ms*2^n + 抖动` 退避；单片 `attempts > 3` 放弃（限流 429 放宽到 12）。**并发降档见下方 M2.1**。
- **探测重试**（m01800 补）：探测阶段的临时性错误（连接失败/超时/429/503/504/重置）指数退避重试，最多 5 次（1.6s→3.2s→5s→5s→5s + 0~250ms 抖动），永久错误（404/401/网页误判/Content-Range 不符）立即返回。实测：服务端前 2 个请求回 429 → 6.1s 内重试成功、SHA 一致；全 429 死服务器 21s 后 fail-fast（码 29）。
- **插件预留点**：六个 hook 调用点全部就位（`on_resolve` / `on_plan` / `on_before_request` / `on_response` / `on_progress` / `on_complete`），**新增三个 Observe hook**：`on_retry(seg, attempt, err)` / `on_preempt(seg, preempts)` / `on_degrade(concurrency, reason)`。并发数/分片大小/限速全部通过 `DownloadOptions` 与 `DownloadPlan` 暴露，插件在 `on_plan` 里可直接改 `concurrency`。
- **新增 CLI**：`-x/--connections`（默认 16，夹到 [1,64]，是"期望上限"而非固定并行度）、`--chunk-size`（默认 `4M`，支持 K/M/G）、`--limit`（默认 0=不限）。
- **验证**：**59** 单元测试全绿（含 8 个新 segment 测试 + 自适应并发控制器 4 个测试 + 限速/退避）；256 MB 本地 Range 服务，`-x 8` → **153 MB/s**、`-x 1` → 127 MB/s，SHA256 全对；**手工构造散乱位图**（前 64 MiB 已完成、其余缺失）续传 → 识别 `已存在 67108864 字节`、只补剩余、SHA256 对；exe **2.03 MB**（≤ 3 MB 预算）。

**M2.1 自适应并发策略（按用户 m01705 最终要求）**：
- **并发不再限制为 2 的幂**：`clamp_concurrency(n)` 只夹到 `[1, 64]`，任意整数都行。默认 **16**，硬上限 **64**。`-x` 设的是"**期望的上限**"，不是固定并行度。
- **模型**：真实网络下我们想开 16 条连接，但服务器常常只放行其中一部分（如只接受 9 个），其余被 403/429/连接重置拒绝。我们不应该一直顶着 16 猛冲（既浪费请求，也容易触发反爬），而应该"**按服务器给多少就跑多少，再慢慢试着往上加**"。
- **控制器 `ConcController`**（`effective`=实际允许的并发，`target`=`-x` 的上限）：
  * **降**：每遇到一次容量类失败（连接级 `Network` 或 403/429），`effective -= 1`，但**不低于当前真正跑通（已收到字节）的连接数**。于是 16 里被拒 7 个 → 一次失败降一格 → 收敛到 **9**（服务器真正能接受的量）。
  * **试探升**：稳定运行 `probe_interval`（初始 **20s**）后 `effective += 1`（9→10）看能不能过；若在 `PROBE_GRACE`=**10s** 内又出现容量失败 → 视为试探失败，退回并把间隔 **翻倍**（20→40→80…封顶 **300s**）；试探成功则间隔回调一半。→ 稳定在 9 的同时，隔一阵才试一次 10，**不会一直 16 并发**。
  * 连接级失败**不惩罚分片**（不 `bump_attempts`，直接退回重派），退避用更短的 `backoff_connect`（300ms 起、封顶 2s）。
  * 无进展连续失败（`hard_failures`，Progress/Done 时清零）超 **40** 次才致命，作死链兜底。
- **实测**（`range-server-limit.exe <root> <max_conc>` 模拟服务端限连接，16 MiB 随机数据、SHA256 校验"相等即真完整"、`-x 16 --chunk-size 1M`）：
  * 服务端只放 9 个（`max_conc=9`）→ **exit 0、2.04s、SHA256 与源一致、REJECT429=7**（16 里被拒 7 个，恰好收敛到 9 并完整下完）。
  * 服务端放 32 个（≥ 目标）→ 1.03s、SHA 一致、**0 拒绝**（不降档）。
  * 服务端全拒（`max_conc=0`）→ 0.13s 内 fail-fast 报 `HTTP 429` / aria2 码 29，不挂死。
  * 极端 `max_conc=1` → 40s 完成、SHA 一致（人工服务器对"瞬时 2 连接"也回 429，导致多次重试退避，属人为极限场景，不代表真实服务器）。
- **单测**：`clamp_concurrency_cases` / `controller_drops_to_working_level` / `controller_probes_up_toward_target` / `controller_never_exceeds_target` / `line_lost_resets_probe_interval` / `line_lost_backs_off_only_fresh_stall`。

**M2.3 线路停发要立刻重置试探间隔（按用户 m10015 的实际疑问）**：
- **问题**：向上试探有个真实漏洞 —— 试探**成功**了（新线路撑过了 `PROBE_GRACE`），但下载一段时间后服务器突然不再发数据。这种"停发"既没有连接错误、也没有 4xx，走不到 `on_capacity_failure`，于是 `probe_interval` 会一直卡在被翻倍过的值上（最多 `PROBE_MAX`=**300s**）。表现就是：线路早停了，程序却要再干等五分钟才重新试探。
- **修法**：ticker 里检测"有连接的 `last_change` 超过 `STALL_SECS`(12s)、且距上次容量事件也超过 12s"，调 `ConcController::on_line_lost(fresh_stalled, floor)`：
  * `probe_interval` 立刻回到 `PROBE_BASE`(20s)、`probing_since = None`、`last_capacity = now`；
  * 只有"**新试探的那条线路一个字节都没收到**"（`fresh_stalled`）才 `effective -= 1`，下限是当前真正跑通的连接数（`ok_now.max(1)`）；老线路停发只重置间隔、不降并发。
- **`last_capacity = now` 兼作节流**：12 秒内最多重置一次，否则每个 tick 都往后推，`now - last_capacity` 永远到不了 `probe_interval`，反而永远试探不上去。
- **单测**：`line_lost_resets_probe_interval`（300s → 20s，且随后能 +1）、`line_lost_backs_off_only_fresh_stall`（只在新线路零字节时降档，`floor` 保护）。

**M2.2 真实下载场景适配（按用户 m01907 / m02040 的实际经验）**：
- **默认 UA 冒充浏览器**：`http::DEFAULT_UA` 固定为一个 Windows Chrome UA（`Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36`）。用户明确要求：aria2/ADL 这类基础 UA 会被服务器直接判为爬虫拒绝（百度网盘等反盗链站尤甚）。个别站要特定 UA 时用 `--user-agent` 覆盖。
- **探测重试**：见 §3.2（探测阶段对连接失败/超时/429/503/504/重置退避重试，永久错误不重试）。
- **重名自动改名**：目标文件已存在且没有对应控制文件时，自动落成 `name (1).ext`、`name (2).ext`…（`path::unique_path`）；`--restart` 才显式覆盖同名文件。**不再静默覆盖**。
- **分块按文件大小自动分配**：`--chunk-size` 不填时（`chunk_size=0`），每段 piece 数 = `clamp(总片数 / (并发×4), 1, 8)`。小文件（几 MB）自动用最小 1 MiB 分块以吃满并发，大文件自动放大到 8 MiB 减少请求数。实测 8 MB / 16 MB 文件 `-x16` 均 ~100–112 MB/s。
- **按 URL 找回控制文件**：`find_control_by_url` 扫下载目录里的 `*.adl`，按 `cf.url` 匹配。落盘名来自 `Content-Disposition` 或 302 后的地址时，也能跨进程续传（以前只按 URL 猜名会找不到控制文件、白下一遍）。
- **文件占用重试**：打开目标文件遇到 Windows `ERROR_SHARING_VIOLATION`(32)/`ERROR_LOCK_VIOLATION`(33)（杀软/索引器占用）时重试 4 次、间隔 200ms。
- **小文件"疑似无用"提示**：`total < 4096` 字节的下载**仍然下完保存**，但成功后打印"⚠ 疑似无用文件：仅 N 字节，可能是错误页/占位文件"。对应"不知名小网站几 KB / 0 KB"的场景。
- **服务器校验头比对**：解析 `Digest` / `Content-Digest` 里的 `sha-256=<base64>`（RFC 3230 与 RFC 9530 两种写法）以及 `Content-MD5` / `Digest: md5=`，下载完比对我们算的 SHA256 / MD5，不一致报 `ChecksumMismatch`（码 8）并**删除损坏文件与进度**。**没有这些头就不校验**（用户："带就校验，不带不验"）。MD5 由本仓自带的 100 行纯 Rust 实现（`crates/adl-core/src/md5.rs`，RFC 1321，含标准测试向量），因为离线缓存里没有 md5 crate，且不影响体积。
- **反爬主动暂停**：连续 **60s** 完全没有新字节（以"任一 worker 收到新字节"为准，慢速但正常的下载不会误判）→ `AdlError::Stalled`（aria2 码 5），**保存进度后退出**，不做无意义的狂重试。对应真实案例：Intel 驱动站 130MB 处触发反爬停发、需等约 40 分钟；此时应立即停手、让用户换网络或稍后手动续传。
- **`--http1`**：强制 HTTP/1.1（ALPN 也只上报 http/1.1）。少数站点 h2 不稳、容易断流。
- **`--proxy`**：HTTP 代理（`reqwest::Proxy::all`），如 `--proxy http://127.0.0.1:7890`。
- **失败时打印来源**：报错附带 `来源: <url>` 与"进度已保存、重跑同一命令自动续传"的提示（校验失败除外，会提示已删除损坏文件）。
- **预分配**：确认使用**真预分配**（NTFS 上 `set_len(total)` 真实占盘，等价 Steam 式预分配）——好处是开下前就知道要占多少、空间不够立即报 `34`；代价是未下完也占满整块空间。
- 单测：`unique_path_renames_on_conflict` / `client_builds_http1_and_proxy` / `default_ua_disguises_as_browser` / `digest_sha256_parses` / `content_md5_parses` / `known_vectors` / `streaming_matches_oneshot`。当前 **66** 单元测试全绿。

**M3 实际交付**（新 crate `adl-plugin` + `adl-core`/`adl-app` 小改）：
- **引擎选型 QuickJS**（`rquickjs 0.14`，`default-features=false, features=["std"]`）：离线可编译（`rquickjs-sys` 自带 `quickjs.c`）。⚠️ 不要设 `CC=clang.exe`（cc-rs 传 MSVC 风格 `/D`、`/std:c11`，clang 不认）；不设 CC 时 cc-rs 用 vswhere 自动找到 VS `cl.exe` 即可。
- **manifest = TOML**（`plugins/<id>/plugin.toml` + `main.js`）：`id/name/description/version/api_version(=1)/entry/kind(=quickjs)` + `[[hooks]] point=... priority=...` + `[permissions] net/fs_read/fs_write/exec/notify` + `[limits] timeout_ms=3000 / memory_mb=32`。`api_version` 不匹配、`kind` 不是 quickjs、`entry` 为空 → 拒载。`HookPoint::parse` 宽容（`onResolve`/`on_resolve`/`Resolve` 都认）。
- **线程模型**：每个插件独占一条 OS 线程（QuickJS `Runtime`/`Context` 是 `!Send`）。宿主侧 `sync_channel(1)` + `recv_timeout(timeout_ms)`：同步 hook 等结果、超时按 pass；Observe hook 用 `try_send`，队列满直接丢。死循环由 `set_interrupt_handler`（每次 hook 前写 deadline）硬中断。`Runtime` 上 `set_memory_limit(memory_mb*1MB)`、`set_max_stack_size(512*1024)`。
- **唯一宿主出口**：JS 侧 `globalThis.__adl_host(op, payloadJson) -> String`（Rust `Function::new` 注册）；`__adl_invoke(hook, payloadJson)` 里 try/catch，插件抛异常转 `{"action":"error"}`，绝不把异常悬在 Rust 侧。
- **权限：声明 ≠ 授权**：`HostCtx.respond` 是唯一入口，op 白名单 `log/notify/getTask/settings.get/httpGet/fs.read/fs.write`；未声明一律 `{"ok":false,"error":"权限不足: …"}` + 审计。`fs.*` 还要求路径在 `fs_read`/`fs_write` 前缀白名单内（`Path::starts_with`）。
- **安全模式**：`--safe`（默认）/`--trusted`。`--safe` 下 `Manifest::active_hooks(trusted=false)` 过滤掉所有 `needs_transform` 的 hook（`onResolve`/`onPlan`/`onBeforeRequest`），插件仍加载但那些 hook **不订阅、不生效**；`--trusted` 才开放 Transform。`--safe` 与 `--trusted` 同用直接 exit 2。
- **9 个 hook 全部接通**（`PluginHooks` 实现 `adl_core::hooks::Hooks`）：`on_resolve`（链式改写 url/userAgent/referer/headers，**在 probe 之前**合入 `opts`，故需特定 UA 的站连探测都过得了）/ `on_plan`（改 concurrency→`clamp_concurrency`、userAgent、referer、headers）/ `on_before_request`（Deny 短路 / Rewrite 追加头，请求级 header 覆盖 client 默认 UA）/ `on_response`（Deny 短路 / Retry{after}）/ `on_complete`（同步，保证进程退出前完成）/ `on_progress`/`on_retry`/`on_preempt`/`on_degrade`（异步 Observe）。同步 hook 在 tokio worker 里用 `tokio::task::block_in_place` 包住阻塞等待。
- **CLI**：`--plugin-dir <dir>`（默认 `exe同级/plugins`）、`--safe`（默认）、`--trusted`。启动时打印每个插件的 id 与生效 hooks 及模式。
- **内置 4 个实用插件**（`E:\ADL\plugins\`，见 `plugins/README.md`）：`ua-fixer`（onResolve 换 Edge UA，可配 `ONLY_HOSTS`）、`auto-concurrency`（onResolve 记域名 + onPlan 按站点调并发，如 github 4 / tencent 1）、`notify-on-complete`（onComplete 发通知，Observe）、`block-html`（onResponse 拦下 text/html 错误页，Guard）。
- **实测（端到端，本地 Range 服务 + 服务器回显 UA/自定义头）**：`--trusted` 加载 4 插件、`ua-fixer` 让探测与下载全走 Edge UA（服务器 `REQ_UA=…edg/131…`）；`--safe` 下 transform 插件 `hooks=[]`、UA 回落到默认 Chrome；`onPlan` 改写实测 `plan.concurrency=16→2`；`onBeforeRequest` 改写实测服务器收到 `REQ_XTEST=from-before-request` 与转发 UA；`block-html` 对 `text/html` 响应 Deny 且**不留残留文件**（0 进度失败会自动删预分配的空文件与 `.adl`）。
- **测试**：adl-core **66** + adl-plugin **13** 全绿；exe **≈3.02 MB**（含 QuickJS，仍在 5 MB 预算内；若要压回 3 MB 以下可考虑 M6 的 wasmi）。

**M4 实际交付（CLI 完整，可发布 v0.1）**（`crates/adl-app/src/{config,history,main}.rs`）：
- **配置文件（TOML）**：`config.rs`。路径优先级：exe 同级 `config.toml`（便携）→ `%LOCALAPPDATA%\ADL\config.toml`（Windows）/`$HOME/.local/share/adl/config.toml`。字段 `dir/connections/chunk_size/limit/user_agent/http1/proxy/mode/plugin_dir/history_enabled`，全部有默认值（`#[serde(default)]`）。**读取时剥掉 UTF-8 BOM**（防记事本/PowerShell 写出 BOM 导致解析失败）；解析失败只警告并回落默认，不让坏配置挡住下载。命令行显式参数永远覆盖配置同名项。`adl config --init` 生成带注释的模板。
- **下载历史（JSON）**：`history.rs`，`%LOCALAPPDATA%\ADL\history.json`，原子写（`tmp`+`rename`）。`TaskRecord{id,url,path,total,status,error_code,error,started_unix,elapsed_ms,sha256}`，`status ∈ completed|failed`。每次下载（成功或失败）在退出前追加一条，`history_enabled=false` 时不记录。
- **子命令**（`args_conflicts_with_subcommands`，`adl <URL>` 仍可直接下载）：
  * `adl list [-l N]` — 列出最近 N 条（默认 20），含状态/大小/耗时/相对时间/文件。
  * `adl resume <id>` — 按历史记录继续（用记录的 url + 目录 + 文件名重新走 `run_download`，命中 `.adl` 即断点续传）。**已完成且文件仍存在时直接提示"已完成"，不会重复下载。**
  * `adl remove <id> [--files]` — 删记录；`--files` 连带删已下载文件与 `.adl` 进度文件。
  * `adl clean` — 只清"已完成"的记录，失败/保留。
  * `adl config [--init]` — 显示配置路径与内容 / 生成模板。
- **测试**：adl-app **7**（config 3 + history 4）+ adl-core 66 + adl-plugin 13 全绿。实测（`LOCALAPPDATA` 指向临时目录隔离）：`config --init` 生成模板；配置里 `dir` / `connections=4` / `history_enabled=false` 全部生效（下载落到配置目录、实测 4 连接、不写历史）；`list` / `resume` / `remove --files` / `clean` 均按预期工作。

**M5 实际交付（GUI：窗口 + 托盘 + 三个页面）**（新 crate `crates/adl-gui` + `adl-app` 改 lib+bin + `web/` 前端）：
- **前端**：`web/`（Vue SFC + vite）。`npm run build` → `web/dist`（index.html 0.39 KB + CSS 2.6 KB + JS 73 KB）。为离线省内存，`web/node_modules` 用目录联接（junction）指向旧项目已有的 `E:\Aria2-Harness-Dev\client\node_modules`（vue 3.5.44 / vite 8.3.4 / @vitejs/plugin-vue 6.0.10），**不跑 npm install**。
- **服务方式**：不用 `include_dir!` 内嵌（离线缓存没有该 crate，且改 UI 要重编 Rust），改为 `crates/adl-gui/src/static_server.rs` 起 `127.0.0.1:0` 静态服务器托管 `web/dist`（SPA 回退 index.html、`..` 穿越拒绝、`Cache-Control: no-store`）。`find_web_root()` 查找顺序：`ADL_WEB_ROOT` → exe 同级 `web-dist`/`web` → cwd `web/dist`/`web`。
- **窗口/托盘**：`tao 0.37 + wry 0.57 + tray-icon 0.25`。⚠️ `tray-icon` 默认 feature 只含 Linux 后端（`muda-libxdo`/`libappindicator`），会把未缓存的 `libxdo`/gtk 拖进依赖解析导致离线构建失败 → 必须 `default-features = false`（Windows 不需要这些）。
- **IPC**：复用 `window.ipc.postMessage` → Rust channel → 主线程 `handle_ipc` → `window.__ipcResolve/__ipcEmit`；`evaluate_script` **只在主线程事件循环里调**（避免回调里死锁），下载进度经 `EventLoopProxy<UserEvent>` 投递后由主循环 eval。
- **命令**：`ping/get_tasks/get_settings/set_settings/add/pause/remove/open_dir`。`add` 从 `Config` 构造 `DownloadOptions` 并加载插件 hooks；进度 **150ms 节流**；暂停用 `handle.abort()`（进度在 `.adl`，重新添加即续传）。
- **三个页面**：任务列表（进度条/速度/暂停/移除/删除文件）、添加下载、设置（默认目录/并发/限速/代理/插件模式/强制 HTTP1/插件目录）。
- **实测（用 WebView2 远程调试 CDP 驱动真实窗口）**：DOM 挂载 ✓、`window.ipc` 存在 ✓、`get_tasks` → `[]` ✓、`get_settings` → `connections=16/mode=safe` ✓；经 GUI `add` 下载 8 MB 本地文件 → 状态 `completed`、**SHA256 与源文件完全一致**、服务器回显 UA 为 Chrome 浏览器 UA。`adl-gui.exe` ≈ **3.38 MB**（仍在 5 MB 预算内）。
- **开发/运行**：`cd web; npm run build`，然后运行 `adl-gui.exe`（或设 `ADL_WEB_ROOT` 指到 dist）。改 UI 只需重建 dist，不必重编 Rust。
- **省磁盘**：`web/node_modules` 是 junction（不占额外空间，删掉不影响源码）；构建产物可随时删 `web/dist` 与 `target/`（`cargo clean`）再重建。

**M6 实际交付（浏览器接管，未做剪贴板兜底）**（新 `crates/adl-gui/src/api_server.rs` + `browser-extension/`）：
- **本地 API**（`api_server::start(gui, port)`，默认端口 `ADL_API_PORT` 或 17800，只绑 `127.0.0.1`，每连接一线程）：
  - `GET /ping`（探活）、`GET /status`（版本 + 任务快照，免 token）。
  - `GET /auto-token`（下发随机 token；要求空 Origin 或 `chrome-extension://`/`moz-extension://`，否则 403）。
  - `POST /add`、`POST /probe`、`GET /check`（**必须带 `x-adl-token`**，401 拒；Origin 非扩展来源 403）。
  - `POST /add` 直接复用 GUI 的 `add_task`，额外接受 `referer` / `user_agent` / `headers`（对象）/ `cookies`（转成 `Cookie:` 头），使登录站与反盗链站点也能接管。
- **三重 CSRF 防护（缺一即漏洞）**：①只绑 127.0.0.1；②改状态接口必须带启动时生成的随机 token（32 hex，经 `/auto-token` 下发并缓存在扩展 storage）；③校验 Origin（普通网页无法省略 Origin，拿不到 token、调不动）。
- **扩展**（`browser-extension/`，MV3，Edge/Chrome 通用）：`downloads.onCreated` 捕获下载 → `POST /add` → 200 且 `ok:true` 才 `chrome.downloads.cancel()+erase()`；失败/ADL 未运行则**原样放行**不丢链接。右键链接菜单「使用 ADL 下载」（`contextMenus`）。需要登录的站由 `cookies.getAll({url})` 取 Cookie 一并转发。`popup.html`/`options.html` 可开关接管、设端口、测试连接。
- **实测**（`E:\ADL\probe\m6test.ps1`，本地 range-server + 真实 adl-gui.exe 进程）：`/ping` ✓；`/auto-token` 返回 32 字符 token ✓；伪造 `Origin: https://evil.com` → **403** ✓；错误 token → **401** ✓；`/add` 下载 8 MB → `completed`、文件 **8,388,608 B**、SHA 与源一致 ✓；扩展 3 个 JS 通过 `node --check`、manifest 为合法 MV3 ✓。
- **权衡**：不做剪贴板监听（用户明确否决：旧软件试过，随便复制链接就弹窗、难以控制）。
- **限制**：ADL 必须处于运行状态。**现已支持「关闭到托盘」**（见下节），关闭窗口不再退出进程，所以接管期间把窗口关掉也没关系。

**插件 UI 扩展点 实际交付（M6 之后，按用户"插件为大"的要求补上）**
- **目标**：让插件能改 ADL 自己的界面（加页面/按钮/设置项/命令，甚至在信任模式下替换内置页面），且**插件永远拿不到 DOM、不能注入 `<script>`、读不到 cookie**——插件返回的是**声明式 JSON 组件树**，由宿主用白名单渲染器画成真实界面。
- **manifest 新增 `[ui]` 段**（TOML）：`[[ui.pages]]`（`id/title/icon/replaces`，`replaces` 非空=替换内置页 `tasks|add|settings`）、`[[ui.actions]]`（`id/label/place=toolbar|task/icon`）、`[[ui.settings]]`（`key/label/kind=boolean|string|number|select/options/default`）、`[[ui.commands]]`（`id/title`）、`[ui.theme]`（`accent`）。
- **JS API**：`adl.onUiRender(req)` 返回组件树；`adl.onAction(req)` 处理按钮动作；`adl.onCommand(req)` 处理 Ctrl+K 命令。组件白名单：`text/heading/badge/divider/spacer/row/col/button/input/select/toggle/progress/list`。
- **分级（安全模式的基础，沿用"能拒绝的权限比能改写的权限安全一个数量级"）**：`safe` 只暴露页面（只读）与设置项，**不暴露** 动作/命令/替换内置页，设置只读；`trusted` 才暴露动作、命令、替换内置页、可编辑设置。
- **设置存储（分段集中）**：主 `config.toml` 只存每插件 `enabled` 开关（`[plugins.<id>] enabled=true`）；详细设置存各插件自己的 `%LOCALAPPDATA%\ADL\plugin-settings\<id>.json`。
- **代码**：`crates/adl-plugin/src/{manifest.rs(UiDecl/…),ui.rs(collect 聚合),plugin_hooks.rs(ui_snapshot/render/run_action/run_command),runtime.rs(PRELUDE 增 onUiRender/onAction/onCommand)}`；`crates/adl-app/src/config.rs(PluginCfg/plugins/plugin_enabled)`；`crates/adl-gui/src/main.rs(Gui.hooks、build_hooks、load_plugin_settings/save_plugin_setting、IPC 增 ui_snapshot/ui_render/run_action/run_command/plugin_setting_set、set_settings 后重建 hooks)`；前端 `web/src/RenderNode.vue`（递归白名单渲染器）+ `App.vue`（插件页侧栏、动作按钮、插件设置区、Ctrl+K 命令面板、trusted 替换内置页）。
- **示例插件**：`plugins/ui-demo/`（页面 + 工具栏动作 + 两个设置项 + 一个命令）。
- **实测**（CDP 驱动真实 WebView2）：`ui_snapshot` 返回 trusted/pages/actions/commands/settings；`ui_render` 返回组件树并在真实 DOM 渲染出 `示例插件页面 · demo`；`run_action`/`run_command` 返回插件消息；`plugin_setting_set` 写入 `plugin-settings/ui-demo.json` 并即时回显；`safe` 模式下 `trusted=false`、`actions=[]`、`commands=[]`、`settings[].editable=false`（页面仍在）；侧栏出现"插件页面"分组、设置页出现"插件设置"区（2 项）、Ctrl+K 命令面板可弹出（1 项）。`cargo test`：adl-app 7 + adl-core 66 + adl-plugin 13 全绿。

**桌面集成补强 实际交付（M6 之后）：单实例 / 关闭到托盘 / 休眠唤醒 / WebView2 降级**
- **新增 `crates/adl-gui/src/platform.rs`**（Win32 胶水，全用 `windows-sys 0.61.2`）：
  - **单实例**：命名互斥量 `Global\ADL_SingleInstance_v1`（`CreateMutexW` + `GetLastError()==ERROR_ALREADY_EXISTS`）；第二个实例通过命名事件 `Global\ADL_Wakeup_v1`（`OpenEventW` + `SetEvent`）唤醒第一个实例后退出。第一个实例起后台线程 `WaitForSingleObject` 等事件 → `UserEvent::Show`。创建互斥量失败（受限令牌）时**放行启动**，不挡用户。
  - **休眠/唤醒**：`PowerRegisterSuspendResumeNotification` + `DEVICE_NOTIFY_CALLBACK`（回调方式，**不用**需要窗口消息泵的 `RegisterSuspendResumeNotification`），回调里把 `PBT_APMSUSPEND(0x0004)` / `PBT_APMRESUMESUSPEND(0x0007)` / `PBT_APMRESUMEAUTOMATIC(0x0012)` 转成 `UserEvent::Suspend`/`Resume`。休眠时把所有进行中任务 `handle.abort()` 并置 `paused`（进度已在 `.adl`）。**唤醒后不自动续传**（用户要求：自动恢复时服务器可能已不发数据/链接失效，须手动点）。
  - **WebView2 检测**：先查 `C:\Program Files (x86)\Microsoft\EdgeWebView\Application\*\msedgewebview2.exe`（及 64 位目录），再查注册表三个视图（HKLM WOW6432Node / HKLM / HKCU 的 `EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}`）。
- **关闭到托盘（销毁/重建，避免白屏）**：`WindowEvent::CloseRequested` 不再 Exit，而是 `webview.take(); window.take();`（**先 drop webview 再 drop window**）——tao 的 `Window::drop` 只发 `Destroyed`，事件循环与 tokio 全存活；托盘「显示主界面」或第二个实例唤醒时用 `make_window(target,...)` **重新创建**窗口与 WebView（`EventLoopWindowTarget` 允许在运行中建窗）。因为前端走同一个本地静态服务器 URL，重建后状态由 `get_tasks`/`ui_snapshot` 拉回。
- **WebView2 缺失 → 浏览器模式（完整功能降级）**：检测不到 WebView2 时**不建窗口**，改为 `cmd /C start "" <url>` 用默认浏览器打开同一个本地 URL；前端 `web/src/ipc.js` 自动探测 `window.ipc`，没有就用 `fetch('POST /ipc')` 走**同一个 `dispatch_ipc` 后端**（静态服务器新增 `POST /ipc`，读 Content-Length 收 body，返回 `{id,payload}`），因此任务列表/添加/设置/插件 UI **全部照常工作**，只是界面在浏览器标签页里。`ADL_FORCE_BROWSER=1` 可强制走浏览器模式（测试用），`ADL_NO_OPEN_BROWSER=1` 跳过自动拉起浏览器。
- **实测**：单实例 ✓（第二进程打印「ADL 已在运行，已唤醒现有窗口。」并退出 0）；关闭到托盘 ✓（`WM_CLOSE` 后进程存活、CDP target 消失；再起第二实例 → target 重新出现、窗口重建）；浏览器模式 ✓（`POST /ipc` 的 `get_settings`/`ping`/`ui_snapshot` 全部正确、`GET /` 返回 index.html）；休眠/唤醒注册成功（运行日志打印「已注册系统休眠/唤醒通知」）。`cargo build --release` 无警告；exe ≈3.5 MB。

**M4 之后就可以开源。** UI 和插件生态是后续加分项。

---

## 9. 已验证的技术假设（全部实测通过）

| # | 假设 | 结论 | 证据 |
|---|---|---|---|
| 1 | QuickJS 能离线构建 | ✅ | rquickjs-sys 0.14.0 vendored 了完整 `quickjs.c`（2,146,468 B） |
| 2 | QuickJS 死循环能被中断 | ✅ | 301ms 中断，`Exception generated by QuickJS` |
| 3 | 越权调用被拒 | ✅ | 返回 `{"ok":false,"error":"权限不足"}` |
| 4 | tao + wry + tray-icon 共存 | ✅ | 托盘菜单点击 → `EventLoopProxy` 正常 |
| 5 | IPC 双向通道 | ✅ | 4 项测试全过（含错误码传播、主动 push） |
| 6 | `evaluate_script` 在回调里死锁 | ✅ | 必须 `proxy.send_event` 转事件循环 |
| 7 | Range 语义（206/416/精确字节） | ✅ | 4 段各 2,097,152 字节，合计 8,388,608 与源文件一致 |
| 8 | 全链路单 exe | ✅ | 托盘 + 窗口 + WebView + QuickJS + tokio + reqwest 同时跑通 |
| 9 | 体积 ≤ 3 MB | ✅ | CLI 2.06 MB；GUI 3.08 MB − vite 省的 103.6 KB ≈ **2.98 MB** |
| 10 | 零 CRT 依赖 | ✅ | 导入表 8–20 个系统 DLL，无 vcruntime / msvcrt / api-ms-win-crt |
| 11 | Vue SFC + vite 可构建 | ✅ | 只引 vue → 64,520 B JS + 278 B CSS + 290 B HTML |
| 12 | vite tree-shaking 有效 | ✅ | 零构建 166.6 KB → vite 63.0 KB，省 103.6 KB |
| 13 | **TLS 只有 platform-verifier 可用** | ✅ | webpki-roots → `UnknownIssuer`（缺 `CN=YR2`）；native-certs → `os error 5`，0 张证书；platform-verifier → `200 OK` |
| 14 | 续传字节精确 | ✅ | 两段 8 MiB 拼接 vs 整段 16 MiB：SHA256 均为 `13fb9acbb51070bca8f47e3f4e223821` |
| 15 | 中断后重连 Range 一致 | ✅ | 收到 1,064,589 B 后 drop，重连同 Range 拿到的 8,388,608 B 与前次完全一致 |
| 16 | h2 多流优于 h1.1 连接池 | ✅ | 5.44 MB/s vs 4.47 MB/s（**+22%**）→ `http2` feature 不能砍 |
| 17 | 每分片独立连接优于共享 | ✅ | 5.78 MB/s vs 5.44 MB/s → 多分片下载用 N 条 TCP，不靠 h2 多流 |
| 18 | 并发段数收益饱和 | ✅ | 1/2/4/8/16 段 ≈ 4.0–4.4 MB/s（波动）→ 期望上限默认 **16**（范围 1–64），按服务器实际放行量自适应 |
| 19 | debug 增量构建够快 | ✅ | 2.7 秒（release 增量 24.6 秒，全量 213 秒） |
| 20 | M1 引擎端到端可用 | ✅ | 48 单元测试全绿；本地 256 MB 下载 2.56 s / SHA256 一致；tuna 3.3 GB ISO 续传 6.3 MB/s；exe 2.10 MB |
| 21 | `If-Range` 会让服务器慢 7~9 倍 | ✅ | tuna 4.31→0.50 MB/s，ustc 4.40→0.50 MB/s（10 MB 封闭 Range）→ 改用"206 后比 ETag" |
| 22 | `trunc` 预分配在 NTFS 上真实占盘 | ✅ | `fsutil sparse queryflag` = NOT sparse；`compact /q` 报 3,303,444,480 bytes stored = 文件长度 |

探针代码在 `E:\ADL\probe\`（`qjs` / `trio` / `ipc` / `all` / `full` / `cli` / `tls` / `m1` / `h2` / `conn` / `conc` / `vue-vite-min` / `vue-zero-build`）。

---

## 10. 风险清单：难在哪、会出什么问题

按"出问题的概率 × 后果严重度"排序。

### 10.1 🔴 最高危：分片调度器写错 → 文件静默损坏

**为什么危险**：写坏的文件不会报错。用户下完一个 4 GB 的 ISO，校验和不对，但下载器显示"成功"。

**具体的坑**（上一版全部踩过）：
- 抢占时直接回 `Missing` 而不等 `Stopped` → 新旧两个 worker 交叉写同一区间
- 事件不带 `token` → 被抢占前发出的 `Progress` 事件污染新 worker 的进度
- 位图与控制文件的 `piece_length` 不一致 → 1 bit 对应错的字节区间
- 206 响应的 `Content-Range` 起点没校验 → 服务器返回错区间，照样写进去

**缓解**：
1. 单写者模型（§3.4），并发点从 N 降到 1
2. 抢占两阶段 + token 校验
3. 位图精度绑定 `piece_length`，不匹配拒绝续传
4. **M2 结束前必须做"下载 4 GB 文件 + 中途杀进程 5 次 + 续传 + SHA256 校验"的测试**
5. 可选 `--verify-sha256`：给一个期望值，下完自动比对

**这是唯一能让项目"看起来做完了但根本不能用"的风险。**

### 10.2 🟠 高危：同步 hook 卡住整个下载

**为什么会出问题**：`onBeforeRequest` 在每个分片请求前都跑。8 个并发分片，插件每次卡 500ms，下载速度直接掉一半。如果插件死循环而 `set_interrupt_handler` 没兜住（比如阻塞在 host 调用里），整个下载停摆。

**缓解**：
- 每个 hook 独立超时（3s / 1s），超时当作 Deny
- `onProgress` 走异步丢弃队列，**永不同步等待**
- 连续 3 次失败熔断，把插件禁用
- **M3 必须测**：插件里写 `while(true){}`，确认下载还能继续（只是这个 hook 被跳过）

### 10.3 🟠 高危：插件线程泄漏

**为什么会出问题**：`Runtime` 是 `!Send`，卸载要 `drop` + `join`。如果 `join` 卡住（插件在死循环里，interrupt handler 又没生效），UI 线程就僵住。热重载频繁改文件时最容易触发。

**缓解**：
- `join` 带 2s 超时，超时就 detach（线程泄漏总比 UI 卡死好）
- 热重载用 `new-before-swap`：新实例建好才换，失败则旧的继续跑
- `notify` 防抖 300ms，避免编辑器保存触发多次重载
- **必须测**：连续保存 20 次，看线程数是否稳定

### 10.4 🟡 中危：构建耦合 —— `cargo build` 不再等于"全部"

**为什么是问题**：`include_dir!` 要求 `web/dist/` 先存在。全新克隆的人只跑 `cargo build` 会失败；没有 Node 的人完全改不了前端。

**缓解**（三条护栏，§2.3）：`dist/` 提交进 git（65 KB）+ `build.rs` 缺文件时嵌入提示页而不是报错 + IPC 契约与框架无关（可逆）。

**次要代价**：两套工具链要同时活着（Node v24 已装，但 vite 8 将来可能要求 Node 26）。这是**唯一一条真正会随时间腐烂**的风险，缓解办法是钉死 `package-lock.json` 并把 `dist/` 一起提交。

### 10.5 🟡 中危：托盘 + 窗口销毁重建的坑

**上一版实测踩过的**：
- `set_visible(false)` 隐藏 → 唤回白屏（WebView2 合成器被挂起）
- `Arc<Mutex<Option<Window>>>` 装不下（`!Send`）
- 每次重建窗口都新绑端口 → 端口和线程泄漏
- 新建窗口可见后才加载完 → 白闪

**缓解**：§2.6 的方案（关闭即销毁、`OnceLock` 缓存 URL、`with_visible(false)` + `PageLoaded` + 1.2s 兜底）。

### 10.6 🟢 已解决：体积 3.08 MB 超 80 KB

**结论：vite 的 tree-shaking 已经解决了，不需要换 hyper，也不需要砍 http2。**

- 前端从零构建的 166.6 KB 降到 63.0 KB，省 **103.6 KB**
- GUI 版 3.08 MB − 103.6 KB ≈ **2.98 MB**，回到 3 MB 以内
- `http2` feature **必须保留**：实测 h2 多流 5.44 MB/s vs h1.1 连接池 4.47 MB/s（**+22%**）

剩下的可选项只有一个，且只在真的还差几十 KB 时才值得考虑：换 hyper 手搓替代 reqwest（约 −0.6 MB，但工作量 +3~5 天，要自己写连接池/重定向/TLS 配置）。**不建议做。**

### 10.7 🟢 低危但必须做：Windows 路径与文件系统边界

`guard_path` 5 层校验（上一版验证过）：
1. 空 / NUL / UNC / 设备命名空间
2. `..` 逐分量归一化
3. 分量含 `:` 判 ADS；基名大写比对 `CON/PRN/AUX/NUL/COM1-9/LPT1-9`
4. 与 `download_root` 做**分量前缀比对**
5. 对已存在最深祖先 `canonicalize` 后二次比对（`\\?\` 前缀要先剥）

### 10.8 🟢 低危：休眠 / 网络切换 / 移动硬盘拔出

| 场景 | 处理 |
|---|---|
| 休眠 | `PowerRegisterSuspendResumeNotification`；`PBT_APMSUSPEND` → 快照暂停列表 → 存会话 → `forcePauseAll`（**顺序不可换**） |
| 唤醒 | 恢复网络后 `unpauseAll`，但**只恢复休眠前在跑的任务**（别把用户手动暂停的也恢复） |
| 网络切换 | 只在 `ConnectivityLevel == InternetAccess` 时通知，且**不自动恢复**用户手动暂停的任务 |
| 移动硬盘拔出 | 判 `ERROR_DEVICE_NOT_CONNECTED` 等 → 报错，**不做自动重试**（设备不在，重试只会无界等待） |
| 文件被占用 | `ERROR_SHARING_VIOLATION` → 重试 3 次 × 200ms |

### 10.9 🟢 低危：单实例与唤醒

命名自动重置事件兼任单实例锁 + 唤醒：

```
CreateEventW(NULL, FALSE, FALSE, "Global\\ADL_Wakeup_v1")
if GetLastError() == ERROR_ALREADY_EXISTS(183) {
    AllowSetForegroundWindow(ASFW_ANY);   // 必须先于 SetEvent
    SetEvent(handle);
    exit(0);
}
```

> 创建失败（受限令牌）时**放行启动** —— 宁可多开一个，也不要因为拿不到全局命名对象就完全跑不起来。

### 10.10 不会出问题的地方（可以放心）

- **tao + wry + tray-icon 共存** —— 已实测（§9 #4）
- **QuickJS 离线构建** —— 已实测，vendored C 源码
- **IPC 双向通道** —— 已实测，协议直接复用上一版
- **Range 语义** —— 已实测，206/416/精确字节全对
- **零 CRT 依赖** —— 已实测，导入表干净
- **crt-static 在残缺 Windows 上跑** —— 已实测，这是上一版最大的兼容性收益

---

## 11. 未决问题

1. ~~**默认并发数**~~（已定，见 §M2.1）：期望上限默认 **16**（范围 1–64，可 `-x` 调）；引擎**自适应**——服务器只放行一部分时按实际放行数跑，稳定后慢慢试探 +1（9→10…），失败则退并指数拉长试探间隔，避免触发反爬。
2. **配置格式**：TOML（可读、注释友好）vs JSON（前端直接吃）。倾向 TOML 存储 + API 返回 JSON。
3. **插件目录约定**：`%APPDATA%\ADL\plugins\<id>\`。便携模式则用 exe 同级 `data\plugins\`。
4. **自动更新**：需要签名验证，工作量不小。第一版手动更新。
5. **WASM 后端要不要做**：M7 看有没有真实需求（比如想用 Rust 写高性能插件）。

> 原先第 6 条"`unsafe-eval` 还是 `h()` render"已随零构建方案一起作废 —— Vue SFC 由 vite 在构建期编译成 render 函数，运行时不需要 `eval`，CSP 可以是严格的 `script-src 'self'`。

---

## 12. 环境（实测）

- `rustc 1.99.0 (b940084d3 2026-09-28)`，host `x86_64-pc-windows-msvc`，内置 LLVM 23.1.1
- MSVC：`D:\Microsoft Visual Studio\18\Community\VC\Tools\MSVC\14.44.35207\bin\HostX64\x64\cl.exe`（另有 14.51.36231，**它触发 D8050，见 §7.2**）
- Windows SDK：`D:\Windows Kits\10`
- clang / lld-link：`D:\LLVM\bin\`（**QuickJS 不能用 clang 编译，必须 MSVC cl**）
- Node：`C:\nodejs\node.exe` v24.15.0，npm 11.12.1
- WebView2 运行时：`157.0.4341.60`（已装）
- 网络（2026-11-20 实测）：crates.io / `registry.npmjs.org` / `registry.npmmirror.com` / 清华 tuna **全部可达**
- 磁盘：Rust `target/` 单目录 673–694 MB（release），`cargo clean` 后全量重建 213 秒 → **定期清 target 即可，不必为磁盘改架构**

---

## 13. 原生 UI（egui / eframe）—— 彻底摆脱 WebView2

### 13.1 为什么换
M5 的界面是 tao + wry + WebView2 + Vue。WebView2 缺失或"残缺版 Windows"时只能降级到"用浏览器打开本地页面"，用户明确觉得"在网页里显示很怪"。于是改为**纯 Rust 原生 UI**：`eframe`（`egui` 立即模式 GUI），**同时编入 glow(OpenGL) 与 wgpu 两套渲染后端**，运行时可选。**不再依赖 WebView2、不再有 node/vite 构建链**，产物是单个 exe，跨 Windows / Linux / macOS（含 Apple Silicon）。

### 13.2 选型
- **egui / eframe**（MIT）：纯 Rust、单 exe、跨平台。
- 否掉 Slint：GPLv3 会传染，与项目 MIT 冲突。
- 否掉自绘 Win32：只支持 Windows，工作量大，且不支持 Linux/macOS。

### 13.3 关键 API（eframe 0.37，与旧版差异大，实测踩过）
- `trait App` 的入口是 **`fn ui(&mut self, ui:&mut egui::Ui, frame:&mut eframe::Frame)`**，不是 `update(&mut self, ctx, frame)`；可选 `fn logic(&mut self, ctx:&egui::Context, frame)`。
- 面板 `CentralPanel::default().show(ui, |ui| …)`、`SidePanel::left(id).show(ui, …)` **接收 `&mut Ui`，不是 `&Context`**。
- 样式：`ctx.style_of(egui::Theme::Light)` / `ctx.set_style_of(egui::Theme::Light, style)`（没有 `style()`/`set_style()`）。
- 窗口控制：`ctx.send_viewport_cmd(ViewportCommand::{Visible(bool), Minimized(bool), Focus, CancelClose, Close, …})`。
- `NativeOptions{ viewport: ViewportBuilder::default().with_inner_size([w,h]).with_min_inner_size([..]).with_title(..), .. }`；`run_native(name, opts, Box::new(|cc| Ok(Box::new(App::new(cc,…)))))`。
- 中文字体：默认字体不含 CJK，必须 `fonts.font_data.insert("cjk", FontData::from_owned(读 ttf))` 并插到 `Proportional` 首位；按平台依次找 `simhei.ttf`/`Deng.ttf`/`msyh.ttc`（Win）、Noto CJK（Linux）、PingFang/Hiragino（macOS）。

### 13.4 双渲染后端：默认 glow，可选 wgpu(Vulkan/DX12/Metal)

`eframe` 同时编入两套后端：`eframe = { version="0.37", default-features=false, features=["glow","wgpu","default_fonts"] }`，运行时用环境变量 `ADL_RENDERER` 选择：

| `ADL_RENDERER` | 后端 | 说明 |
|---|---|---|
| （未设）/ `glow` | glow(OpenGL) | **默认**，兼容性最好：虚拟机 / 远程桌面 / 老核显 / 无独显都能开 |
| `wgpu` | wgpu | 自动挑 Vulkan/DX12/Metal 中可用的一个 |
| `vulkan` | wgpu + 强制 `WGPU_BACKEND=vulkan` | 想强制走 Vulkan 时用 |

- **为什么默认 glow**：实测本机（无独显的远程会话）下 wgpu 曾出现"窗口能创建、API 能连，但约 5 秒后进程静默退出、无任何 stderr"（surface 丢失）。glow 在任何环境都能开，所以默认它兜底；想要 Vulkan/DX12 的用户可显式 `ADL_RENDERER=wgpu`。
- 实测：`glow` 稳定、窗口标题 `ADL 下载器`、`/ping` 正常；`wgpu` 在能拿到 DX12 适配器的会话里也能正常工作；`vulkan` 在本机**没有可用 Vulkan 驱动**，会拿不到适配器（此时退回 `ADL_RENDERER=wgpu` 或默认 glow 即可）。
- 体积：双后端比单 glow 大约 +3 MB（未 strip）。用户明确"体积不在意，解压后 <1 GB 即可"，双后端（~13 MB 未 strip）完全满足。
- 下载器 UI 用 GL 还是 Vulkan 性能无区别；Vulkan 的价值主要在特定系统/驱动上，代价是兼容面更窄，所以留成可选而非默认。

### 13.5 构建与体积
- `eframe` 首次需联网拉取（~200 crates，含 glutin/glow/egui_glow/wgpu/naga 等）；**离线下 `--offline` 会因 `accesskit_unix` 依赖的 `async-executor 1.14.0` 被 yank 而解析失败**，必须联网构建。
- 依赖 `muda = { default-features=false }`，否则默认 feature 拉入 Linux 的 `libxdo`，离线/跨平台解析失败。
- 编译开销：wgpu/naga/vello 是重灾区，单次全量 release 构建可达 20~40 分钟；迭代期可用 `CARGO_PROFILE_RELEASE_LTO=false` + `CARGO_PROFILE_RELEASE_CODEGEN_UNITS=16` 加速（注意 profile 变化会触发全量重建）。fat LTO + 单 CGU 在本机会病理级慢，正式出包再开。
- release（`opt-level="z" + lto="fat" + strip`）体积见 §13.7 实测。

### 13.6 与后端的衔接
`adl-gui` 只负责"壳"，仍复用 `adl-core`（引擎）、`adl-plugin`（插件 + 声明式 UI）、`adl-app`（config/history）。浏览器接管 API（`api_server`）与 UI 同进程共享 `Arc<Gui>` 状态，功能不变。桌面集成：单实例（命名互斥量 + 命名事件唤醒）、关闭到托盘（`CancelClose` + `Visible(false)`）、托盘菜单（tray-icon + muda，显示主界面/退出）、休眠暂停。

### 13.7 界面骨架：左右式布局（侧边栏 + 顶栏 + 内容区）

按用户要求改成"左右式，顶上也加"（对齐当前 DeepSeek Harness 的形态，但不要左下角角色图）。落地如下：

```text
┌───────────────────────────────────────────────┐
│ 侧边栏(212px) │  顶栏(54px)：页标题 … 退出 / +添加下载 │
│  ADL 下载器   ├───────────────────────────────┤
│  导航         │                               │
│   任务列表 ●  │        内容区（C_BG 浅底）      │
│   添加下载    │        白色卡片 12px 圆角        │
│   设置        │                               │
│  插件页面/…   │                               │
│               │                               │
│  安全模式     │                               │
│  Ctrl+K       │                               │
└───────────────────────────────────────────────┘
```

- **egui 0.37 没有 `SidePanel` / `TopBottomPanel`**，统一成了 `egui::Panel::{left,right,top,bottom}`，且 `show()` 收 `&mut Ui`（不是 `&Context`）。
- **面板调用顺序决定占位**：先 `Panel::left` 再 `Panel::top`，顶栏才只横跨右侧区域；最外层不要先开 `CentralPanel`。
- 顶栏右侧按钮用**自绘**（`ui.interact` + `painter.rect_filled` + `painter.text`），坐标从 `available_rect_before_wrap()` 的右缘往左推。不用 `with_layout(right_to_left)` 或 `Sides`：前者在 `Panel::top` 的内边距里容易被裁，后者行为不稳定（实测右侧按钮会跑到面板外）。
- **emoji 一律不用**：加载的中文字体（simhei）没有 emoji 字形，`📥 ➕ ⚙ 🧩 ⏻` 全渲染成空方块。导航项改为纯文字 + 选中态左侧 3px 青绿竖条。
- **表格必须固定列宽**：`egui::Grid` 会按内容自适应，文件名一长就把整行宽度抢走，其余列被挤成竖排（实测 `test8m (2).bin` 会一个字一行）。改成 `ui.horizontal()` + 每列一个 `allocate_ui_with_layout(vec2(w,26), left_to_right)` 的定宽 `table_cell`；其中"文件"列吃剩余宽度（`w_name = available - 固定列总和 - 16`），并 `Label::truncate()` + 悬停显示完整 URL。
- **左对齐标签**：`ui.add_sized([w,h], Label::new(..).halign(Align::LEFT))` 观感是居中（各标签看起来像右对齐），必须显式用 `allocate_ui_with_layout(.., Layout::left_to_right(Align::Center), ..)` 包一层（`label_cell`）。
- 主题：`C_TEAL #3D8B7A`（强调）、`C_BG #F4F8F8`（内容底）、`C_CARD #FFFFFF`（卡片）、`C_LINE #E4EDEC`（描边）、`C_MUTED #8A9A98`（次要文字）、`C_INK #2A3333`（正文）。

### 13.8 截图验证与 DPI 陷阱（踩过的坑）

验证方式：`EnumWindows` 找出标题以 `ADL` 开头的窗口 → `PrintWindow(hwnd, dc, PW_RENDERFULLCONTENT=2)` 存 PNG。

**🔴 关键陷阱**：PowerShell 进程默认 **DPI-unaware**。在 144 DPI 的屏幕上，`GetWindowRect` 返回**逻辑像素**（1055×737），而 `PrintWindow` 按**物理像素**渲染进传入的 DC —— 结果只截到窗口左上角一块。

这个假象一度让我误判"顶栏右侧的按钮不见了"、"布局坏了"，反复改了三版 topbar 代码。实际是截图脚本的问题：**UI 一直是对的**。

正确做法：`GetDpiForWindow(hwnd)` 拿到 dpi（本机 144），位图尺寸 = `logical * dpi / 96`（→1582×1106），再按这个尺寸建 `Bitmap`。

其它坑：
- `.ps1` **必须是纯 ASCII**：Windows PowerShell 5.1 按 GBK 读脚本，UTF-8 中文注释会被读坏并报一串 `Missing using directive`。
- 不要用 `Get-Content`/`Set-Content` 批量改 Rust 源文件里的中文 —— 会把 UTF-8 字符串写坏成 `\u{3220}` 之类，rustc 报 `unknown start of token`。用编辑工具改。
- 调试辅助：`ADL_TAB=tasks|add|settings` 可让程序直接落到指定页，省去模拟点击。

### 13.9 自研 UI 组件库（`crates/adl-ui`）

手写 egui 到第三页时已经明显低效：每加一行设置要写 `label_cell` + `add_sized` +
`end_row`，每加一列表格要写五遍 `table_cell`。所以把重复样板抽成独立 crate。

**为什么不直接用 `egui_extras`**：`TableBuilder` 自带虚拟化与列宽拖拽，但为列宽
拖拽要拉 `enum-map` 等依赖，而我们要的只是"固定列宽 + 一列吃剩余 + 截断"这种
最朴素的语义。自己的 `table.rs` 149 行、零新增依赖，正好够用。

**三层结构**（合计 854 行）：

| 文件 | 行数 | 职责 |
|---|---|---|
| `theme.rs` | 285 | 全部主题令牌：`Palette`（颜色）/ `Spacing`（间距）/ `Radius`（圆角）/ `TextSize`（字号），外加 `install_and_apply(ctx)` 一次性装好 `Visuals` 与 `TextStyle`。换肤只改这里 |
| `widgets.rs` | 176 | egui 表达力不足、需要反复手写的控件：`cell` / `label_cell` / `nav_item` / `small_button` / `mode_pill` / `badge` / `card` / `section_title` / `hint` / `empty_state` |
| `form.rs` | 229 | `Field` 枚举 + `form()`：一个字段一行，自动画"标签格 + 控件 + 说明" |
| `table.rs` | 149 | `Column` / `table()` / `virtual_table()` / `truncated()`：定宽列 + 一列吃剩余 + 视口虚拟化 |

**收益（实测）**：

- `crates/adl-gui/src/main.rs` 从 1857 行降到 **1625 行**（−232 行）
- 胶水调用基本清零：`ui.end_row` 8→0、`Grid::new` 3→1（只在注释里）、
  `add_sized` 5→0、`table_cell` 15→0
- 设置页从 201 行降到约 45 行；任务列表从 176 行降到约 150 行（且列宽逻辑内聚）
- 体积代价：可忽略（exe 10.6 MB，比加组件库前的 12.45 MB 还小——省掉的重复
  代码与常量折叠都有贡献）

**新增一页的标准写法**：

```rust
let mut fields = vec![
    adl_ui::Field::text("默认下载目录", &mut self.s_dir).hint("留空 = 当前目录"),
    adl_ui::Field::number("并发数", &mut self.s_conn, 1..=64).note("服务器不给满会自动降"),
    adl_ui::Field::select("插件模式", &mut self.s_mode, &opts),
    adl_ui::Field::checkbox("HTTP/1.1", &mut self.s_http1, "强制使用 HTTP/1.1"),
];
adl_ui::form(ui, "下载", &mut fields);
```

```rust
let cols = [
    adl_ui::Column::new("状态", 72.0),
    adl_ui::Column::remainder("文件"),   // 吃掉剩余宽度
    adl_ui::Column::new("进度", 150.0),
];
adl_ui::table(ui, &cols, tasks.len(), |ui, i, w| {
    adl_ui::widgets::cell(ui, w[0], |ui| { ui.label(&tasks[i].status); });
    // ...
});
```

**为什么这和插件 UI 是同一套**：`lib.rs` 的文档注释写明本体 GUI 与插件渲染器
共用这一套，所以插件的声明式组件树画出来与本体的界面是同一个视觉语言，而不是
"插件的东西看着像另一个软件"。

**踩过的坑**：

- `cell()` 里 `set_min_width` **必须**有：`allocate_ui_with_layout` 只给 max_rect，
  水平布局前进时用的是子 Ui 的"已用宽度"，表头文字窄就会各列挤在一起（这就是
  v4 之前六列全挤在左边的根因）。
- `label_cell` 不能用 `ui.add_sized([w,h], Label::new(..).halign(Align::LEFT))`：
  那样观感是居中（看起来像右对齐），必须显式建 `Layout::left_to_right(Align::Center)`。
- 表单的 `Field::Text { value: &'a mut String }` 在 `iter_mut()` 解构后是
  `&mut &mut String`，传给 `TextEdit::singleline` 要解两层（`&mut **value`）。
- 失败原因**不要**塞进"进度"列：150px 装不下，实测被截成"资源不存在（HTTP 4…"。
  改成进度列给短标记"已中断"，完整原因挂到状态列的悬停提示。

### 13.10 无边框窗口 + 极细线 + 渲染器词汇表

三件事一起做：**拆掉原生标题栏、把分隔线换成底色差、把插件渲染器补全**。它们
看起来无关，其实都是同一个原则：**界面里不要有"装饰性的线"，位置靠底色和留白
表达，线只在真的被截断时才出现**。

#### 13.10.1 自绘标题栏

`ViewportBuilder::with_decorations(false)` 之后，**系统连边框拖拽缩放都不提供了**。
winit 的 `WM_NCCALCSIZE` 会把客户区扩到整个非客户区（`event_loop.rs:1162`），
`to_window_styles()` 虽然仍设 `WS_CAPTION | WS_BORDER`，但 `adjust_rect()` 在
非 decorations 时把 `WS_CAPTION | WS_SIZEBOX` 清掉（`window_state.rs:435`）。
所以两件事必须自己补：

| 交互 | 实现 |
|---|---|
| 拖动窗口 | `Sense::click_and_drag()` 的不可见热区 + `drag_started()` → `ViewportCommand::StartDrag` |
| 双击最大化 | `drag.double_clicked()` → 翻转 `maximized` → `ViewportCommand::Maximized(b)` |
| 八向缩放 | `resize_borders()` 铺 8 个 5px 热区（`Sense::drag()`），hover 时换光标，`drag_started()` → `ViewportCommand::BeginResize(Dir)` |
| 最小化/最大化/关闭 | `adl_ui::window_button()` 三个自绘按钮 |

**图标必须手画，不能用字体符号**：中文字体（simhei）没有 `✕ ▢ ─` 这些字形，
渲染出来是空方块（`—` 能显示纯属意外，换个字体就没了）。`window_button` 里
全部用 `painter.line_segment` / `rect_stroke` 画，半边长 4.5px、`Stroke::new(1.2, fg)`。

**最大化状态要从系统同步回来**：`ui.ctx().input(|i| i.viewport().maximized)` 每帧
读一次写进 `self.maximized`，否则用户拖窗口到屏幕顶部触发系统贴边最大化后，
我们的按钮还显示"最大化"图标。

#### 13.10.2 分隔线 → 底色差

原来有三层都是白底 + `show_separator_line(true)`，于是出现两条明显的 1px 竖/横线
（用户截图里最刺眼的就是这个）。改法：

- `Panel::left` / `Panel::top` 全部 `show_separator_line(false)`
- 侧边栏、顶栏、标题栏**同色**（都是 `palette.card` 白）—— 它们本来就是一整块
  "外壳"，没有任何理由分开
- 内容区 `palette.bg`(#F4F8F8) —— 白外壳浮在浅灰底上，边界自然出现，不需要画线
- 新增 `palette.hair`(#EEF3F2)，比 `line`(#E4EDEC) 更淡；`hairline()` 画
  `Stroke::new(0.6, hair)`，**只在表头下沿**这一处保留
- `table()` 每行之间的 `ui.separator()` **全删**：行高 30px + 留白已经足够区分，
  每行一条线在浅底上像一道道沟
- `card_frame()` 描边从 `line` 降到 `hair`：卡片靠白底 + 圆角 + 极淡描边浮起来

#### 13.10.3 渲染器词汇表（29 个 type）

`render_node` 从 9 个 type 扩到 23 个，再到 29 个，且**宿主只用 `adl_ui` 的令牌与
控件**，观感与本体界面完全一致：

`text` / `label` / `heading` / `badge` / `status` / `divider` / `spacer` / `row` /
`col` / `card` / `grid` / `stat` / `kv` / `table` / `list` / `link` / `code` /
`timestamp` / `button` / `input` / `select` / `toggle` / `progress` /
`alert` / `collapse` / `scroll` / `steps` / `spinner` / `copy`

三条设计约束：

1. **`tone` 只用语义名**（`accent`/`warn`/`danger`/`muted`，别名 `primary`/`ok`/
   `completed`/`paused`/`error`/`failed`），**不允许插件传 RGB** —— 否则换主题时
   插件里那些硬编码颜色会变成一块块突兀的补丁。
2. **未知 type 画成 `[xxx]` 占位，绝不 panic** —— 新插件用新组件跑在旧宿主上时，
   只是少显示一点东西，不会整页崩。
3. **`input` 只读** —— 插件不能抢焦点，否则用户在插件页打字时按键会被吃掉。

**`grid` 必须用 `ui.horizontal_top`**：用 `ui.horizontal` 时各行是独立布局，
某一行卡片高一点就会把后面的行往下推，三张 stat 卡片会呈现"阶梯错开"。实测
第一版就是这个样子，改 `horizontal_top` 后立刻对齐。

#### 13.10.4 第二轮扩充：补"信息密度"类组件

第一批 23 个 type 覆盖了"展示数据"和"绑定设置"，但插件想写**日志面板、
多步流程、错误提示**时缺零件 —— 只能用 `code` 一行行堆，页面立刻失去层次。
补的 6 个都是"把已有信息重新组织"而不是新能力：

| type | 解决的问题 | 关键实现 |
|---|---|---|
| `alert` | 一段话的提示没法用 `badge` 承载 | `Frame::new().fill(底色).corner_radius(8).inner_margin(symmetric(14,9))`，底色按 `tone` 取 `err_bg`/`warn_bg`/`accent_soft`；左侧色条在 `Frame` **之外**用 `painter().rect_filled` 画，否则会和内容抢布局 |
| `collapse` | 长页面的次要信息占满首屏 | `CollapsingHeader::new(...).default_open(open).show_background(false).id_salt(format!("{plugin}.collapse.{title}"))` —— **`id_salt` 必须带插件名**，否则两个插件同名的折叠面板会互相串状态 |
| `scroll` | 插件塞长列表把整页顶出屏幕 | `ScrollArea::vertical().max_height(h).auto_shrink([false,false])`，可选 `id_salt` |
| `steps` | 下载流程这类"第几步"没法表达 | 手画：`circle_filled` 圆点 + `Align2::CENTER_CENTER` 序号 + `hline` 连接线；`i < current` 实心 accent，`i == current` 用 `tone`，`i > current` 用 `hair` 底 + `muted` 字 |
| `spinner` | 插件等异步结果时页面像卡死 | `egui::Spinner::new().size(s).color(pal().accent)` |
| `copy` | 哈希/路径只能靠用户手选 | `ui.ctx().copy_text(v.clone())` —— **剪贴板 API 只在 `Context` 上**，`Ui` 没有；插件本身拿不到剪贴板，必须由宿主代劳 |

顺带修掉一个旧 bug：**`table` 单元格的 `tone` 以前被解析后丢弃**（只取 `value`），
现在单元格可写 `{value, tone}` 真的上色了 —— 插件表格里"已完成/失败"那一列
终于和本体任务列表一样有颜色。

`copy` 的 `label` 为空时按钮文案是「复制」，非空则当按钮文字用；值本身用
`.monospace().small().color(muted)` 展示，方便核对哈希。

### 13.11 外壳重构：顶栏并入主区、添加下载弹窗化、主页与关闭询问

这一版把窗口"外壳"整个重排了，起因是用户看截图提的九条意见。记在这里
是因为其中几条踩坑与取舍并不显然。

#### 13.11.1 顶栏不再是独立 Panel

以前是三层横条叠着：`Panel::top(标题栏)` → `Panel::left(侧栏)` →
`Panel::top(顶栏白底)` → `CentralPanel(内容浅底)`。问题是三个 Panel 各自
有底色，接缝处出现横向白/灰分界带，看起来像三张纸拼起来。

现在：

```
Panel::top(标题栏, 白底)
Panel::left(侧栏, 白底)
CentralPanel(shell_frame: 浅底 + 左上圆角)
  ├── topbar(ui)     ← 透明，chips 那一行
  └── ScrollArea(内容)
```

`topbar_frame()` 改成**完全透明**（`Frame::new().inner_margin(symmetric(0,6))`），
让 `shell_frame` 的浅底透上来 —— 这样"顶栏 + 内容区"是同一块底，只有 chips
与卡片的浮起感来分层，不再有线。

**分区靠底色差，不靠线**（用户 m05639："分隔线太明显了…使用不同颜色的窗口
代表分隔线（一些要截断的地方可以使用特别细的）"）。于是：

- 表格里每行后面的 `ui.separator()` 全部删除，行间只靠 `row_h` 留白；
- 唯一的横线留在**表头下方**，用 `widgets::hairline`（`Stroke::new(0.6, palette.hair)`，
  `hair = #EEF3F2`，几乎和底色同色）—— 它只负责"这里开始是数据"，
  不是装饰；
- 页面标题与内容之间用 `widgets::rule`（`Stroke::new(0.8, Color32::from_black_alpha(22))`，
  比 `hairline` 略深一点的**浅黑线**，用户要求"浅一点的黑线"）；
- 其余分组全部靠 `card` 白底浮在 `bg` 浅底上，卡片本身没有描边。

`rule` 与 `hairline` 都是 `allocate_exact_size(vec2(w, 1.0), Sense::hover())` +
只画 `rect.center().y` 那一根 —— 分配 1px 高再画中间，在 2x DPI 下才不会变成
2px 粗线。

#### 13.11.2 左上角圆角与 eframe 的清屏色（踩坑）

`shell_frame()` 只切 `nw` 一个角（`CornerRadius{nw: card+4, ne:0, sw:0, se:0}`）；
另外三个角贴窗口边缘，切了会露白边。

**圆角外侧露出来的颜色是 eframe 的 `clear_color`，不是任何 Panel 的底色。**
eframe 默认给 `rgba(12,12,12,180)`（半透明深色），在浅色主题下那块圆角缺口
会变成一个**黑三角**。实测第一版就是这个样子。修法：

```rust
fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
    let c = pal().card;   // 与侧栏/标题栏同色
    [c.r() as f32/255.0, c.g() as f32/255.0, c.b() as f32/255.0, 1.0]
}
```

#### 13.11.3 侧栏：仿 DSH 的信息层级

- 最左上角是**折叠键**，与窗口三键同一条横条（`Panel::top` 里先画折叠键，
  再画右侧三个 `window_button`），不再让侧栏顶部单独占一行。
- 侧栏内部顺序：品牌行（只有 `ADL` 三个字，青绿 19px）→ **`+ 新建下载`** →
  主页 / 任务列表 / 插件 / 消息 / 终端 → 插件页面（`hairline` 分隔后紧接，**不再有
  "插件页面"小字分组标题**）→ 底部 `Layout::bottom_up`（Ctrl+K 提示 / 模式胶囊 / **设置**）。
- **品牌不再用方块图标**。第一版是"`A` 方块 + ADL / 下载器"两行，用户要求
  "直接写 ADL 就行了" —— 手画的方块字母和字体渲染的正文放一起永远对不齐，
  而且侧栏本来就是自己画的，不需要 logo 撑门面。
- **收起态 = 整个侧栏不渲染**（用户："侧栏收起后直接完全隐藏，不要窄轨"）。
  早先那套"56px 窄轨 + 首字当图标"的方案连同 `Spacing::sidebar_w_collapsed`
  令牌一起删除（中文字体没有图标字形，用单字当图标本来就只是妥协）。
  收起时**品牌字跟着一起消失**（用户："收起之后这个 adl 可以不用显示"），
  `titlebar()` 不再补画。
- **折叠键图标固定成同一个**：`sidebar_toggle(ui, rect, _will_expand)` 永远画
  **带竖向分隔线的圆角矩形**。第一版按"点了会变成什么"画了两种图标（展开时方框+竖线、
  收起时汉堡），用户的反馈是收起后"图像变不一样了"，看着像换了颗按钮。
  参数保留 `_will_expand` 只为签名稳定，实际不再影响画法。
  收起的动作反馈交给**侧栏本身的宽度动画**（0.22s），不再靠图标变化提示。
- 侧栏不再有"添加下载"导航项 —— 添加改成了弹窗，入口是 `+ 新建下载`。
- **选中态是整块淡入**（不是从左到右擦入，见 §13.12）。

#### 13.11.4 添加下载 → Modal

`egui::Modal::new(Id::unique("adl_add_modal"))`，宽 520，两个 `TextEdit`
（`desired_width(f32::INFINITY)`，URL 很长不适合 `form()` 的定宽控件格），
地址框里按 Enter 直接开始。`ModalResponse::should_close()`（背板点击 / Esc）
等于取消。

#### 13.11.5 顶栏 chips（参考 DSH）

`with_layout(Layout::right_to_left(Align::Center))` 从右往左摆，第一个 `add`
就落在最右边 —— 比手算 `x -= width` 稳得多（手算版第一版就把最右一个 chip
挤出面板外了）。所以视觉顺序"限速 → 线路 → 总计"必须**倒着 add**。

三个 chip（早先还有一个 `任务列表` chip 负责回任务页，实测没人用 ——
侧栏已经有高亮的导航项，删掉了）：

| chip | 图标 | 含义 | 交互 |
|---|---|---|---|
| `标准模式` / `限速 2.0 MB/s` | `ChipIcon::Gauge` | 当前限速档位；**不限速时叫「标准模式」** | `Popup::menu` 选 不限速/1/2/5/10 MB/s，直接写 `cfg.limit` + 落盘 |
| `N 条线路` | `ChipIcon::Lines` | 所有进行中任务的活跃连接数之和（**服务器机架**图标，几个并排的机箱 + 状态点） | 悬停说明"服务器会限流" |
| `完成 N 总计 M` | `ChipIcon::Stack` | 任务计数（**上菱形 + 下两层 V**，见下） | 点开是任务卡片列表：状态圆点 + 文件名 + 大小/速度，整行可点跳任务列表，右上角「清空已完成」（只清界面，不删磁盘文件） |

**图标全部手画，不用字体符号**（`adl-ui` 的 `ChipIcon` + `paint_chip_icon`）：
simhei 里没有 `⚙ ⚡ ≡` 这些字形，用了会渲染成空方块。`Gauge` 是一段开口
圆弧（12 段折线拼，egui 没有 arc 图元）+ 指针 + 中心点。

`Lines` 的用户要求是"线路的图标你看看怎么做" —— 第一版画的是"一个节点分叉成
两个"的拓扑，12px 下和 `<` 分不开；第二版画人形（和 DSH 那排 chip 同构）但语义
不符；最终改成**服务器机架**：10.4×12.0 的描边外框 + 两条 0.9 高的横隔板 +
三个 r=1.0 的状态点。

`Stack` 前后改了三版，最后是拿参考图（23×31 PNG）**逐像素转 ASCII 量出来的**：

1. 第一版：实心菱形（`Shape::convex_polygon`）+ 两条横条 → 像"钻石加两道杠"。
2. 第二版：菱形改空心描边，横条改成等宽横条 → 上层对了，下层还是横条。
3. 最终版：**上层一个完整菱形描边，下面两层只画下半截的 V**（上半截被上层挡住）。
   参考图实测半宽 7 / 半高 4（**7:4，不是 2:1**），层间中线距 4 = 半个菱形高，
   于是上层菱形的下顶点正好落在下层 V 的两端连线上。

```rust
let h = 3.25; let w = h * 2.0; let y0 = c.y - h;
p.add(Shape::closed_line(vec![(c.x, y0 - h), (c.x + w, y0), (c.x, y0 + h), (c.x - w, y0)], stroke));
for i in 1..3 {
    let y = y0 + h * i as f32;
    p.add(Shape::line(vec![(c.x - w, y), (c.x, y + h), (c.x + w, y)], stroke));
}
```

教训：**"堆叠/层叠"这类图标靠猜画不准，量一遍参考图比改三版快**。

`N 条线路` 的数据源是新加的 `Progress::connections`：引擎在 `emit_progress`
里带 `conns: usize`（四个调用点都能直接拿到 `active.len()`），
`adl-plugin` 的 `on_progress` JSON 也同步加了 `connections` 字段。

#### 13.11.6 主页：统计卡 + 热力图

四张 `stat_card`（任务总数 / 已完成 / 失败 / 累计下载）+ `segmented` 切
周/月/年 + `heatmap`。数据源是 `history.json` 的 `started_unix`。

两个布局坑：

1. **统计卡宽度**：写 `(avail - gap*3)/4` 之后又在每张卡后面 `add_space(gap)`，
   等于吃了 4 份间距，最右那张被裁掉半个。正确写法是把间距交给
   `ui.spacing_mut().item_spacing.x = gap` —— `horizontal` 只在元素**之间**
   插间距，n 个元素天然 n-1 个间隙。
2. **热力图坐标系**：年视图是"一列一周、一行一天"（GitHub 那种），
   周/月视图是"一行一周、一格一天"。两者 `cells` 的 `(col,row)` 换算方向相反，
   写成同一个分支就会出现数据落在错格子。
3. **热力图要水平居中**（用户要求）。网格总宽 = `cols*cell + (cols-1)*gap`，
   先 `allocate_exact_size(vec2(full_w, h))` 拿到整行矩形，再让格子从
   `rect.left() + ((full_w - w) * 0.5).max(0.0)` 开始画 —— 直接 `ui.horizontal`
   会左对齐，而用 `Layout::centered_and_justified` 会把每个格子都拉伸变形。

#### 13.11.7 设置页：只给档位，不给自由输入

- **并发**：7 档 `segmented`（1/2/4/8/16/32/64），选的是**上限**；
  引擎按服务器实际放行量自动收敛，稳定后偶尔试探 +1。
  改档位立刻写 `cfg.connections` + 落盘，不需要点"保存设置"。
- **限速**：`DragValue`（按当前单位的数值）+ 单位 `ComboBox`（自动 / B/s /
  KB/s / MB/s / GB/s）。用户明确要求用英文单位，"兆字节/秒"读起来别扭。
  `format_speed_unit(bps, unit)` 负责显示，`auto` 时退化为自适应。
- **HTTP/1.1 开关已从界面移除**（用户："自动就行了"）。`Config.http1` 字段保留，
  保存设置时原样带上现值，不会被重置。
- **关闭行为**：只有两段（最小化到托盘 / 退出程序），对应 `cfg.close_action`
  （`"tray"` / `"close"`）。`""` 只表示"还没问过"，不是可选状态 ——
  **用户明确要求：问过一次之后就定死，不能调回"每次询问"**。
  设置页在未问过时把"最小化到托盘"显示为默认高亮，并附一句说明。

#### 13.11.8 关闭询问与 `closing` 闸门（踩坑）

用户要求"第一次点关闭时问一次，能记住选项"。实现时有个自锁陷阱：

```
close_requested → CancelClose → 弹窗 → 用户选"退出程序" → 再发 Close
                                                ↑ 又回到 close_requested
```

于是永远关不掉。修法是加一个 `closing: bool` 放行闸：确认退出后先置 `closing = true`
再发 `ViewportCommand::Close`，拦截处看到 `closing` 就放行不再拦。

#### 13.11.9 插件设置入口

侧栏的"`<插件名> 设置`"用的是伪页面 `Tab::Plugin("adl", "settings:<pluginId>")`。
`page_plugin` 一进来先认这个前缀，走 `page_plugin_settings(ui, plugin_id)` ——
宿主把该插件的 settings 项单独列成一页，而不是让插件自己渲染。
`page_title()` 同步识别这个前缀，顶栏标题显示 `<pluginId> · 设置`。

#### 13.11.10 任务卡片下拉与显示名兜底

点「剩余 N / 完成 M」弹出任务卡片列表，两个实现细节值得记：

1. **显示名不能直接用 `path`**。失败任务的 `path` 常常是 `None`（0 字节就中断了），
   退到 `t.url` 会把整条 `http://127.0.0.1:61243/test256m.bin` 塞进 340px 宽的面板，
   面板被撑到超出窗口。`display_name()` 的顺序是：落盘文件名 → URL 去掉
   query/fragment 后的**最后一段** → 原始 URL。
2. **`Popup` 里不能用 `self.xxx()` 改状态**。`Popup::menu(&resp).show(|ui| ...)` 的闭包
   借的是 `ui`，而 `self` 已被 `ui.horizontal` 的闭包可变借用占用。做法是把要改的事
   记到闭包外的局部变量（`go_tasks` / `clear_finished`），`show` 返回后再统一处理。

排序用 `(rank, id)`：`downloading`(0) → `paused|failed`(1) → 其他(2)，
所以失败任务浮在最上面，已完成沉底。

#### 13.11.11 淡入淡出与文本选择范围

用户原话：「可以加一些淡入淡出的动画，而且有些就是怎么说可以直接像浏览器文本框选
那样的，可以给他限制一下。」两件事。

**① 关掉全站文本选择。** egui 默认 `selectable_labels = true`，每个 `ui.label()`
都能被拖出一片蓝底，观感像网页而不是原生应用。在 `Theme::apply` 里改两行：

```rust
style.animation_time = 0.14;              // 默认 0.2 偏"拖"
style.interaction.selectable_labels = false;
style.interaction.multi_widget_text_select = false;
```

⚠️ `interaction` 是 **`Style` 的字段**（`style.rs:312`），不是 `Visuals` 的 ——
写 `v.interaction.…` 编译不过。`install_and_apply` 会给 Light/Dark 两份 style 都套，
所以一处生效。少数确实该能复制的文本（`render_node` 的 `code` / `copy`）用
`Label::new(..).selectable(true)` 显式放开，它的语义是
`self.selectable.unwrap_or_else(|| ui.style().interaction.selectable_labels)`，
**局部优先于全局**。

**② 弹窗淡入。** 这里有两个非显然的坑：

1. **`ctx.animate_bool` 不能用来做"打开时淡入"**。`animation_manager.rs` 里首次
   遇到某个 id 走的是 `None => insert(BoolAnim { last_value: end, .. }); end` ——
   **首帧直接返回终值**。而弹窗只在打开期间存在，每次都命中这个分支。所以改成
   自管时间戳：`struct Fade { key: u8, start: Option<Instant> }`，`track(key)` 在
   key 变化时重置，`alpha(dur)` 返回 `1 - (1-t)²`（缓出）。
2. **必须显式给 `Modal` 传 `frame`**。`Modal` 内部用 `Frame::popup(style)` 在
   **内容画完之后**才 `paint` 卡片，而 `ui.set_opacity()` 只作用于"之后画的图元"，
   管不到它。不传的话现象是「文字在淡入、白卡啪一下出现」。`fade_frame()` 把
   `fill`/`stroke`/`shadow` 的 alpha 一起降到 `a`，再配 `ui.set_opacity(a)` 让内容
   同步淡入。

另外**不能对白卡用 `gamma_multiply`**：它把 RGB 也一起乘，淡到一半白卡变**灰**卡，
像蒙了层脏东西。只改 alpha 要用 `Color32::from_rgba_unmultiplied(r, g, b, a)`。

状态条的 6 秒消失也在最后 `STATUS_FADE = 0.45s` 内线性降到 0，不再硬切。

**验证方法（值得复用）**：把 `FADE_SECS` 临时改成 60.0 编译一版，打开弹窗后 3 秒
截图 —— 卡片、文字、按钮整体接近全透明、只剩背板变暗，才说明 `fade_frame` 生效了。
只测 0.14s 的正常时长是看不出来的。

🔴 **拖选测试的两个陷阱**（都踩过）：
- 在 `ScrollArea` 里拖选会触发 **drag-to-scroll**，页面直接滚回顶部，看起来像
  "选择没生效"其实是被滚走了。要测选择得在不可滚动的区域拖，或先记下滚动位置。
- **负结果必须先做正对照**。第一次我在页面标题上拖、看到没有蓝底就以为改对了，
  其实顶栏标题是 `painter.text()` 画的，本来就不参与选择 —— 那次测试什么都没证明。
  把 `selectable_labels` 临时改回 `true`、在同一位置拖出蓝底，才算证明了检测手段有效。
  同理，`accent_soft`（#E2F1ED）的选中底色非常淡，肉眼容易漏看，用像素采样判定更可靠：
  代码行被选中后背景从 `244,248,248` 变成 `226,241,237`。

### 13.12 换页动效：竖条滑动、底色淡入、页面下刷

用户原话：「侧边栏的左边的小绿条也弄一下吧…我想的是这个小绿条下移之后到这个按钮
然后这个按钮亮起来（左到右）然后页面上到下刷新出来（可以快速一点，有微微的观感
就行）还有就是新建下载这个点出来有点奇怪，就是下载地址和下面哪个的输入框先出来
然后这个其他的才显示出来。」

三件事共用**一个时钟**：`tick_page()` 每帧返回 `page_fill`（0→1，缓出
`1-(1-t)²`），页签一变就重置起点。侧栏淡入和页面下刷都读它，才像"一次切换"。

**① 竖条必须跨行插值，所以不能由每一行自己画。** 这是整件事的关键：切页那一帧
旧选中行已经不渲染了，如果竖条还留在 `nav_item` 里，它只会"原地消失、在下一行
重新冒出来" —— 正是用户抱怨的"直接出现"。做法：

- `nav_item_anim(ui, selected, label, fill) -> Response` 只画行和底色，**不画竖条**；
- `sidebar` 里用一个闭包 `track` 统一调用，选中时记下 `(left, top, h)`；
- 整栏画完之后，才用 `animate_value_with_time(id, top, NAV_SLIDE_SECS)` 求出插值 y，
  交给 `nav_bar(ui, left, slide, h)` 画一次。

⚠️ `animate_value_with_time` **首帧直接返回目标值**（和 `animate_bool` 同样的坑），
所以用 `nav_slide: Option<f32>` 兜底：`None` 时直接 `= top`，否则第一次进侧栏
竖条会从顶上滑下来。

**② 底色是整块淡入，不是从左到右擦入。** 第一版做的是裁剪矩形擦入（记录在此，
作为"别再做回去"的参考）：

```rust
// 已删除：用户后来觉得"那块底色扫过来太抢眼"
let wipe = Rect::from_min_max(rect.min,
    pos2(rect.left() + rect.width() * fill, rect.bottom()));
pnt.set_clip_rect(pnt.clip_rect().intersect(wipe));
pnt.rect_filled(rect, CornerRadius::same(th.radius.nav), p.accent_soft);
```

现在直接按 `fill` 算 alpha 整块画：

```rust
let a = (fill.clamp(0.0, 1.0) * 255.0).round() as u8;
if a > 0 {
    let c = p.accent_soft;
    let fill = Color32::from_rgba_unmultiplied(c.r(), c.g(), c.b(), a);
    ui.painter().rect_filled(rect, CornerRadius::same(th.radius.nav), fill);
}
```

⚠️ **只能改 alpha，不能用 `Color32::gamma_multiply`**：它会把 RGB 一起乘下去，
淡到一半时 `accent_soft` 会变成发暗的脏青色，而不是"同一颜色更透明"。

**③ 页面下刷 = 淡入 + 8px 位移。** 只做淡入像"慢慢显影"，加一点位移才有"刷新
出来"的方向感。幅度故意很小（8px / 0.16s）—— 用户要的是"微微的观感"，做大了
就成了花活。实现上先 `ui.add_space((1-pf)*8.0)` 再 `ui.scope(|ui| { ui.set_opacity(pf); ... })`。

⚠️ 时钟必须在 `ui()` 里、**侧栏那个 `if` 外面**算（`self.page_fill = self.tick_page(&ctx)`），
否则侧栏收起时页面不刷新。

**④ 弹窗"分阶段出现"的真凶是背板。** 用户描述的"输入框先出来、其他后出来"，
实际是 `Modal` 的背板在 `Area::show` 里**最先画**，完全不受 `ui.set_opacity` 影响
（它只作用于之后画的图元）→ 观感是"背板啪一下变暗 + 内容慢慢浮出"。修法是给
`Modal` 也传 `backdrop_color(fade_color(Color32::from_black_alpha(100), a))`，让
背板和内容同步。

**验证方法**：把 `NAV_SLIDE_SECS`/`NAV_FILL_SECS`/`FADE_SECS` 临时放大到 1.2~1.6 秒，
用 `burst.cs` 连拍。实测 `burst-03.png`（t≈963ms）能同时看到：竖条停在"主页"与
"任务列表"之间、主页底色淡入到约 60%、页面内容半透明 —— 三项动效都在动。

### 13.13 常驻插件页与消息中心

**常驻「插件」页**（`Tab::Plugins`，`ADL_TAB=plugins`）

用户要的是"安装插件和管理插件的地方"。它和侧栏下面那些插件页是两件事：
`Tab::Plugin(id, page)` 是**插件自己声明的页面**（由插件返回组件树、宿主负责画），
而 `Tab::Plugins` 是宿主的管理页，**一个插件都没装也能进**。

页面布局照着 DSH 的插件页做：顶部「安装、启用和配置插件」+ 右侧「刷新 / + 添加插件」，
下面三个 tab（已安装 / 插件市场 / 发现），列表项**名称在左、开关在右**。

- **数据源**：`adl_plugin::ui::collect()` 新增的 `"all"` 键。原有的 `"plugins"`
  只收"有 UI 的"插件（`pages/actions/settings/commands` 任一非空），而真正干活的
  `ua-fixer`、`block-html` 恰好没有 UI —— 只列 `"plugins"` 的话管理页会空掉一半。
  所以 `collect()` 里把 push 提到 `if !has_ui { continue; }` **之前**。
- **停用的插件会从快照里消失**：`build_hooks` 在加载阶段就 `.filter(|p| cfg.plugin_enabled(&p.id))`，
  停用过的插件根本没被加载，也就不在 `"all"` 里 —— 表现是"点一次停用，这个插件就
  从列表里消失了，再也点不到启用"。修法是 `disabled_plugin_entries(root, known)`：
  扫插件目录、读 `plugin.toml`、跳过已在快照里的，按 `collect()` 同构补一条。
  坏 manifest 直接 `continue`（一个坏插件不该让整页报错）。
- **排序**：启用的排前面，同组按 id 排。否则开关一次，卡片会跳到列表另一头。
- **启停落盘**：`cfg.plugins.insert(id, PluginCfg { enabled })` → `cfg.save()` →
  `*gui.hooks.lock() = build_hooks(&new_cfg)` → 快照立即失效。
  卡片闭包里已经借了 `self.gui` 的锁，**在闭包里写配置会死锁** —— 点击结果先收到
  `let mut toggle: Option<(String, bool)>`，全部卡片画完再落盘。
- **插件市场 / 发现是空壳，而且故意是空壳**：源地址还没定（用户计划接自己的 GitHub
  仓库），所以两页只把"将来会有什么"说清楚。**不做假列表** —— 假数据会让人以为已经
  能装了。「来源配置」按钮和「+ 添加插件」都落到"打开本地插件目录"，因为现在唯一的
  "来源"就是它；留一个点了没反应的按钮比没有按钮更糟。
- **新控件 `switch(ui, on) -> Response`**（`crates/adl-ui/src/widgets.rs`）：38×21
  圆角 11 的轨道 + r=8 白圆点，`animate_bool_responsive` 驱动平移。配一个
  `mix_color(a, b, t)` 做逐通道插值（`Color32` 没有 `lerp`）。

**消息中心**（`crates/adl-gui/src/notice.rs`，`Tab::Notices`，`ADL_TAB=notices`）

以前所有反馈都走底部状态条，6 秒后消失；用户切走了就再也看不到"那个到底下完没有"。

- `Level{Info, Ok, Warn, Error}` + `Notice{id, level, title, body, at, read}`，落盘到
  `%LOCALAPPDATA%\ADL\notices.json`（原子写 `.json.tmp`，读的时候剥 BOM，读坏当空库）。
- **上限 200 条**：这是下载器不是日志系统，留太多只会让「消息」页变成负担。
- **未读用实心圆点、已读用空心圆**，不改整行底色（克制，也省得和 accent_soft 撞）。
- 侧栏入口写 `消息 (N)`，N 是未读数；全部已读后回到纯"消息"。

**toast 弹窗**

- `Area::new(Id::unique("adl_toast")).order(Order::Tooltip).anchor(RIGHT_BOTTOM, (-18,-18))`，
  4 秒后最后 0.35 秒淡出。淡出期间必须 `ctx.request_repaint()`，否则窗口静止时会停在半透明。
- **`Area` 自带 `fade_in` 会和外层 alpha 叠乘**（`area.rs` 在 ui 建好后自己
  `multiply_opacity`），toast 只活 4 秒，再叠一层入场淡入会显得发虚 → `.fade_in(false)`。
- 卡片底色/描边/阴影也要自己跟着 `alpha` 走（`fade_frame` 那套的 `fade_color`），
  否则会出现"文字淡了、白卡还在"。
- 点 toast 跳「消息」页并标记全部已读。

**后台线程 → 界面的通道**

下载任务跑在 tokio 上、插件跑在各自的 OS 线程上，它们都拿不到 `App`：

- `Gui::pending_notices: Mutex<Vec<(Level, String, String)>>`，`fn ui()` 每帧
  `std::mem::take` 取空后逐条 `self.notify(...)`。
- 插件侧走 `adl_plugin::hostapi::set_notify_sink(Some(Arc::new(...)))`：
  `adl.notify()` 从"写 stderr"升级成"落到界面上"。**用全局 `OnceLock` 而不是
  `HostCtx` 字段**，因为 `HostCtx` 在插件加载时就固定了，而宿主（GUI）比插件晚就绪。
  CLI 不注册 sink，就自然退化成原来的 stderr 行为。
- 下载完成的文案带"用时 X 秒"：`start.elapsed()` 是 spawn 里那个 `let start`，不是
  任务结构里的 `elapsed_ms`（后者是给界面进度条用的近似值）。

**文字太浅的修法**（用户 m09142 的第二条要求）

- `Palette::light().muted` 从 `#8A9A98` 压到 **`#5E6D6B`**：白底对比度 3.0:1 → 约 5.9:1，达 WCAG AA。
- `section_title` 改用 `ink`（小节标题本就该比正文重）。
- 🔴 **`.strong()` 必须同时 `.color()`**：`widget_text.rs` 里 `strong` 会走
  `visuals.strong_text_color()`，而它是 `widgets.active.text_color()`；本项目
  `theme.rs` 把 `active.fg_stroke` 设成白色（青绿按钮上要白字），于是**任何不带
  `.color()` 的 strong 文本都会画成白字**，在白卡上整行消失。这个 bug 让插件名
  整行空白了一轮，最后靠像素扫描才发现。

### 13.14 终端页（替代 Ctrl+K 命令面板）

用户原话：「这个命令面板可以变成终端那种（win的powershell）单独页面。」

原来的命令面板是个 `Window` 浮层：按 Ctrl+K 弹出、选一条就跑、关掉就没了。问题不在
交互而在**输出** —— 命令跑完的提示飘在状态条上 6 秒就消失，想翻上一条只能重跑。

- `Tab::Terminal`（`ADL_TAB=terminal`），Ctrl+K 改成"跳到终端页 / 已经在就跳回任务列表"。
  ⚠️ 必须是"当前就在终端才回退"，否则从别的页按 Ctrl+K 会被判成"关闭"。
- **终端自己管滚动、高度钉满视口**：外面那层页面 `ScrollArea` 要跳过
  （`if self.tab == Tab::Terminal { self.page_terminal(ui); return; }`）。套两层滚动区
  时 `PS>` 输入行会被视口边界切掉一半 —— 终端的输入框必须永远停在看得见的地方。
- 高度要先把 `Frame` 的内边距扣掉（`symmetric(14,10)` → 上下各 10），否则内容比框高
  20px，输入行画到框外看不见。`full_h = ui.available_height()`、`inner_h = (full_h - 20).max(160)`。
- **深色不跟主题**：这是浅色界面里的一个"深色控制台孤岛"，配色写死
  （`bg #1B2120` / `border #2E3735` / `out #D6DCDA` / `info #869592` / `echo #7AC8B4` / `err #E38686`）。
  要是跟着主题走，浅色下就成了"白底终端"，那种观感是错的。
- 命令：`help` / `clear` / `tasks|ls` / `add <url> [文件名]` / `pause <id>|all` /
  `resume <id>` / `rm <id>`（不删文件）/ `open <id>` / `settings` /
  `set dir|connections|limit` / `mode safe|trusted` / `plugin list` / `plugin run <p> <cmd>`。
  输出一律走 `self.term.push`，**不弹 toast、不写状态条** —— 终端页自己就是输出面板。
- 历史上限 500 行，`↑/↓` 翻历史，`Esc` 清空当前输入。

🔴 **两个 egui 输入坑（都实测踩过）**：

1. **Enter 必须在建 `TextEdit` 之前读**。单行 `TextEdit` 收到回车会
   `consume_key(Enter)` 并 `surrender_focus`，等它返回之后再
   `ui.input(|i| i.key_pressed(Enter))` 永远是 `false` —— 第一次回车直接丢键。
   所以在 `ui.horizontal` 之前先把 `enter/up/down/esc` 一次读出来。
2. 判断"输入框是活动的"要用 `te.has_focus() || te.lost_focus()`：回车那一帧焦点已经
   交还了，只看 `has_focus()` 会漏掉这一下。

### 13.15 待续
- Linux/macOS 需在 `eframe` features 补 `x11`/`wayland`；`platform.rs` 的 Win32 部分已用 `#[cfg(windows)]` 隔离。
- 旧的 `web/`（Vue）与 `static_server.rs` 已不再编入（保留仅作历史），后续可清理。
- `render_node` 已改用 `adl_ui` 的 `badge`/`card`/`hairline`，但 `table`/`kv`/`grid`
  仍是自己算列宽（插件表格要支持任意列数与单元格节点，与 `adl_ui::table` 的
  `FnMut(ui, i, widths)` 签名不同构）；后续可抽公共的"均分列宽"函数。
- 顶栏"完成 N 总计 M"chip 已能点开任务卡片列表；下一步可给每行加"暂停/删除"
  小按钮（目前要跳任务列表操作）。
- 关闭询问弹窗只在"还没有记忆选择"（`cfg.close_action == ""`）时出现；
  之后只能在「最小化到托盘 / 退出程序」之间切换，**没有"改回每次询问"这个状态**
  （用户明确要求）。
- 自绘标题栏的**边缘缩放热区**（`resize_borders`，8 个 `G = 5.0` 的条）只在
  窗口非最大化时铺；最大化下要跳过，否则鼠标贴屏幕边缘会误触 resize 光标。
- 侧栏"完全隐藏"后没有保留任何窄轨，所以**折叠键是唯一入口** —— 它必须
  一直画在标题栏里，`titlebar()` 不能因为侧栏隐藏就少画那一块。
  侧栏收起/展开现在是 0.22 秒的宽度动画（`Panel::left` 一直存在、只动 `exact_size`），
  面板内部必须 `set_clip_rect` + `set_width(sidebar_w())` 固定排版宽度，否则文字会
  在中途换行、按钮被压扁。
- 文本选择已全局关闭，只对 `code`/`copy` 放开。若以后加了"日志"类页面想让整段
  可选，记得用 `Label::new(..).selectable(true)` 逐个放开，别把全局开关又打开。
- 淡入覆盖三个 overlay（添加下载 / 关闭询问 / 消息 toast）和状态条。页面切换现在走
  的是 §13.12 的"下刷"通道，不再是硬切。
- 消息中心只有"全部已读 / 清空"两个批量动作，没有单条已读、没有按级别过滤。
  条数上限 200 已经够用，真要翻旧账再补。
- 插件页只有"启用/停用/打开目录/重新扫描"，**没有卸载**：删目录是不可逆操作，
  等以后想清楚了再做（要做也得先确认"是删 `plugins/<id>/` 还是移到回收站"）。
- 插件通知目前都是 `Level::Info`。`notify` 权限没带级别字段，若要区分
  "警告/错误"，得先在 manifest 或 `adl.notify()` 的 payload 里加 `level`。
- **插件市场是空壳**：`PluginsTab::Market` / `Discover` 只有说明文字。要做真的，得先定
  索引 JSON 的格式（`id/name/version/download_url/sha256/min_api`）、下载与解压策略
  （看到 `plugin.toml` 才认）、**签名校验**（否则等于开放任意代码执行），以及"装到哪"
  （`plugins/<id>/`，重名要确认覆盖）。用户明确说会接自己的 GitHub 源。
- 终端页的命令是硬编码的 `match`，没有补全、没有 `--help` 子命令、没有把插件命令
  动态列进 `help`（`plugin list` 只列 id，不带命令）。


