# ADL 插件

ADL 的插件就是「一个目录 + 一个 `plugin.toml` + 一个入口 JS」。

```
plugins/
  <你的插件 id>/
    plugin.toml
    main.js
```

- 默认从 `adl.exe` 同级的 `plugins/` 加载；也可以用 `--plugin-dir <目录>` 指定。
- `--plugin-dir` 指向的是**包含多个插件子目录的根**（不是单个插件目录本身）。
- 运行模式：`--safe`（默认）只允许 Observe / Guard 类 hook；`--trusted` 才允许 Transform（改写引擎行为）。

## plugin.toml

```toml
id = "com.example.my-plugin"      # 全局唯一
name = "My Plugin"
version = "1.0.0"
description = "一句话说明"
api_version = 1                   # 必须是 1
entry = "main.js"
kind = "quickjs"                  # 目前只支持 quickjs

[[hooks]]
point = "onBeforeRequest"
priority = 10                     # 越小越先执行

[permissions]
notify = true                     # 需要哪个能力就开哪个
# net = true
# fs_read = ["C:\\Users\\me\\Downloads"]
# fs_write = ["C:\\Users\\me\\Downloads"]
# exec = false
```

`point` 可以写成 `onBeforeRequest` / `on_before_request` / `BeforeRequest`，都认。

## Hook 一览

| hook | 类型 | 说明 | 返回值 |
|---|---|---|---|
| `onResolve` | Transform | 拿到 URL，可换 UA / Referer / 头 / URL（在探测之前生效） | `{action:"rewrite", userAgent?, referer?, headers?, url?}` |
| `onPlan` | Transform | 拿到分片计划，可改并发数等 | `{action:"rewrite", concurrency?, headers?, userAgent?, referer?}` |
| `onBeforeRequest` | Guard/Transform | 每个分片请求前，可拒绝或加/改请求头 | `{action:"deny", reason}` 或 `{action:"rewrite", headers:{...}}` |
| `onResponse` | Guard | 收到响应头，可拒绝（例如拦 HTML 错误页）或要求重试 | `{action:"deny", reason}` 或 `{action:"retry", afterMs}` |
| `onProgress` | Observe | 进度变化（高频，异步丢弃，别做重活） | 无（返回值忽略） |
| `onRetry` | Observe | 某分片重试 | 无 |
| `onPreempt` | Observe | 某分片被抢占 | 无 |
| `onDegrade` | Observe | 并发被降档 | 无 |
| `onComplete` | Observe/Transform | 下载完成（同步，保证退出前跑完） | 可返回 `{action:"deny", reason}` 阻止收尾 |

`--safe` 模式下，`onResolve` / `onPlan` / `onBeforeRequest` 这些 Transform hook **不会被调用**（插件仍加载，但那些 hook 不订阅）。

## JS API（`globalThis.adl`）

```js
adl.onResolve((req) => { ... });        // 注册 hook
adl.onBeforeRequest((req) => { ... });
// ... 其它 hook 同理

adl.log("任意文本");                      // 写 stderr（永远允许）
adl.notify("标题", "正文");               // 需要 permissions.notify
adl.getTask();                           // 当前任务信息（对象）
adl.settings.get("key", 默认值);          // 需要 --trusted
```

- `req` / `plan` / `r` / `c` 是解析好的对象（见上表）。
- 抛异常会被捕获并转成 `{action:"error"}`，不会让下载崩溃，但这个 hook 视为"跳过"。
- 有 3 秒（`timeout_ms`，默认 3000）硬超时；写死循环会被中断。
- 内存上限默认 32 MB。

## 自带的 5 个插件

- **ua-fixer**：给指定站点换上特定浏览器的 UA（默认 Edge）。改 `main.js` 里的 `UA` / `ONLY_HOSTS`。
- **auto-concurrency**：按域名调并发（github/githubusercontent/sourceforge 用 4，tencent 用 1）。改 `RULES`。
- **notify-on-complete**：下载完成后发通知（Observe，安全模式可用）。
- **block-html**：服务器返回 `text/html` 时拒绝写入（拦错误页/登录页）。**如果你确实要下 HTML 网页，请关掉它。**
- **ui-demo**：演示插件 UI 扩展点（页面 + 工具栏动作 + 设置项 + 命令）。

## 安全说明（重要）

- **`--safe`（默认）**：只跑 Observe / Guard，插件无法改变引擎行为、无法联网/读写文件。**装来源不明的插件时用它。**
- **`--trusted`**：允许 Transform（改 UA/头/并发/拒绝响应）。只在你信任的插件上用。
- 原生 dll 插件（未来 M7）无法被沙箱约束，只会在 `--unsafe-native` 下加载，风险自负。
- 权限是"**声明 ≠ 授权**"：manifest 里写了某个权限，还得运行模式允许，且调用时再校验，缺一即拒。

## 调试

- 插件启动失败会打印 `[plugin] …` 到 stderr。
- `adl.log()` 也走 stderr。
- 单测：`cargo test -p adl-plugin`。

## 插件 UI 扩展点（改界面）

插件可以给 ADL 加**页面 / 工具栏按钮 / 设置项 / Ctrl+K 命令**，trusted 模式下还能**替换内置页面**。插件返回的是**声明式 JSON 组件树**，宿主用白名单渲染器画成真实界面——插件拿不到 DOM，不能注入 `<script>`，读不到 cookie。

manifest 里加 `[ui]` 段：

```toml
[[ui.pages]]
id = "demo"                    # 侧边栏「插件页面」里的一页
title = "示例页面"
# replaces = "tasks"           # 仅 trusted：用本页替换内置页 tasks|add|settings

[[ui.actions]]
id = "hello"
label = "示例动作"
place = "toolbar"              # toolbar（任务页顶部）或 task（每个任务行）

[[ui.settings]]
key = "greeting"
label = "问候语"
kind = "string"                # boolean | string | number | select
default = "你好"

[[ui.commands]]
id = "say-hi"
title = "示例：打个招呼"        # Ctrl+K 命令面板里的一条
```

```js
adl.onUiRender((req) => ({
  type: "col",
  children: [
    { type: "heading", value: "示例页面 · " + req.page },
    { type: "progress", value: 42, total: 100 },
    { type: "button", label: "点我", action: "hello", kind: "ghost" },
  ],
}));
adl.onAction((req) => ({ message: "收到动作：" + req.action }));
adl.onCommand((req) => ({ message: "命令响应" }));
```

- 组件白名单：`text / label / heading / badge / status / divider / spacer / row / col / card / grid / stat / kv / table / list / link / code / timestamp / button / input / select / toggle / progress`；容器类节点用 `children: []`，列表/键值/表格类用自己的字段（见下）。
- 按钮的 `action` 会回调到 `adl.onAction`（仅 trusted 生效）；`input/select/toggle` 上的 `setting` 会双向绑定到插件设置。
- **分级**：`safe` 只给页面（只读）+ 设置项（只读）；`trusted` 才给动作、命令、可编辑设置、替换内置页。
- 设置存储：主 `config.toml` 只存每插件 `enabled` 开关；详细值在 `%LOCALAPPDATA%\ADL\plugin-settings\<id>.json`。

### 组件速查

| type | 关键字段 | 说明 |
|---|---|---|
| `text` | `value`, `dim?` | 正文；`dim:true` 变弱色 |
| `label` | `value` | 小号灰标题，用于分段 |
| `heading` | `value` | 17px 加粗 |
| `badge` | `value`, `tone?` | 胶囊徽标 |
| `status` | `value`, `tone?` | 圆点 + 文字（运行中/已完成…） |
| `divider` | — | 极细线 |
| `spacer` | `height?`（默认 8） | 竖向留白 |
| `row` | `children`, `gap?` | 横向排列 |
| `col` | `children`, `gap?` | 纵向排列 |
| `card` | `children` | 白卡片容器 |
| `grid` | `columns?`（默认 3）, `gap?`, `children` | 等宽网格，适合统计卡片墙 |
| `stat` | `value`, `label?` | 大数字卡片 |
| `kv` | `items:[{key,value,tone?}]`, `labelWidth?` | 键值对表 |
| `table` | `columns:["标题",…]`, `rows:[[…]]` | 插件表格（列宽均分） |
| `list` | `items:[string或节点]` | 圆点列表 |
| `link` | `label`, `url` | 超链接 |
| `code` | `value` | 等宽小字（哈希/日志） |
| `timestamp` | `value` | 弱色小字 |
| `button` | `label`, `action`, `kind?` | `kind` = `primary`/`ghost`/省略 |
| `progress` | `value` + `total?` 或 `value`(0..1), `label?` | 有 `total` 时显示 `已下/总量` |
| `toggle` | `label`, `value`, `setting` | 开关，双向绑定设置 |
| `select` | `label`, `value`, `options`, `setting` | 下拉，双向绑定设置 |
| `input` | `label`, `value` | **只读**展示（插件不能抢焦点） |

`tone` 只用语义名：`accent` / `warn` / `danger` / `muted`（另有别名 `primary`/`ok`/`completed`/`paused`/`error`/`failed`）。**不要写 RGB** —— 硬编码色在换主题时会变成一块突兀的补丁。

未知 `type` 会被画成 `[xxx]` 占位而不是报错，所以新插件用新组件在旧版 ADL 上只是少显示一点东西，不会整页崩掉。
