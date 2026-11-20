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
- **单测**：`clamp_concurrency_cases` / `controller_drops_to_working_level` / `controller_probes_up_toward_target` / `controller_never_exceeds_target`。

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
