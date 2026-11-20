<script setup>
import { ref, computed, onMounted, onBeforeUnmount, provide } from 'vue'
import { invoke, listen } from './ipc.js'
import RenderNode from './RenderNode.vue'

const tab = ref('tasks')
const tasks = ref([])
const settings = ref({
  dir: '',
  connections: 16,
  limit: 0,
  proxy: '',
  mode: 'safe',
  http1: false,
  plugin_dir: '',
})
const addForm = ref({ url: '', dir: '', out: '' })
const toast = ref('')
let toastTimer = null
let pollTimer = null
const unlisteners = []

// ── 插件 UI ──
const ui = ref({ trusted: false, plugins: [], pages: [], actions: [], settings: [], commands: [], replace: {} })
const pluginView = ref(null)
const currentKey = ref('')
const cmdOpen = ref(false)
const cmdQuery = ref('')

const pluginPages = computed(() => (ui.value.pages || []).filter((p) => !p.replaces))
const toolbarActions = computed(() => (ui.value.actions || []).filter((a) => a.place !== 'task'))
const taskActions = computed(() => (ui.value.actions || []).filter((a) => a.place === 'task'))
const filteredCommands = computed(() => {
  const q = cmdQuery.value.trim().toLowerCase()
  const list = ui.value.commands || []
  return q ? list.filter((c) => (c.title || c.id).toLowerCase().includes(q)) : list
})

function showToast(msg) {
  toast.value = msg
  if (toastTimer) clearTimeout(toastTimer)
  toastTimer = setTimeout(() => (toast.value = ''), 2600)
}

function formatBytes(n) {
  if (!n || n <= 0) return '0 B'
  const units = ['B', 'KB', 'MB', 'GB', 'TB']
  let i = 0
  let v = n
  while (v >= 1024 && i < units.length - 1) {
    v /= 1024
    i++
  }
  return v.toFixed(i === 0 ? 0 : 1) + ' ' + units[i]
}
function formatSpeed(bps) {
  if (!bps || bps <= 0) return '—'
  return formatBytes(bps) + '/s'
}
function statusText(s) {
  return { downloading: '下载中', paused: '已暂停', completed: '已完成', failed: '失败' }[s] || s
}
function percent(t) {
  if (t.status === 'completed') return 100
  if (!t.total || t.total <= 0) return 0
  return Math.min(100, Math.round((t.completed / t.total) * 100))
}

async function refreshTasks() {
  try {
    const arr = await invoke('get_tasks')
    tasks.value = Array.isArray(arr) ? arr : []
  } catch (e) {
    /* 忽略轮询错误 */
  }
}

async function loadSettings() {
  try {
    settings.value = await invoke('get_settings')
  } catch (e) {
    showToast('读取设置失败：' + e.message)
  }
}

async function loadUi() {
  try {
    ui.value = await invoke('ui_snapshot')
  } catch (e) {
    /* 无插件时也可能失败，忽略 */
  }
}

async function saveSettings() {
  try {
    settings.value = await invoke('set_settings', {
      dir: settings.value.dir,
      connections: Number(settings.value.connections) || 16,
      limit: Number(settings.value.limit) || 0,
      proxy: settings.value.proxy || '',
      mode: settings.value.mode,
      http1: !!settings.value.http1,
      plugin_dir: settings.value.plugin_dir || '',
    })
    await loadUi()
    showToast('设置已保存')
  } catch (e) {
    showToast('保存失败：' + e.message)
  }
}

async function addDownload() {
  if (!addForm.value.url.trim()) {
    showToast('请填写下载地址')
    return
  }
  try {
    const r = await invoke('add', {
      url: addForm.value.url.trim(),
      dir: addForm.value.dir.trim() || undefined,
      out: addForm.value.out.trim() || undefined,
    })
    if (r && r.ok) {
      showToast('已添加 #' + r.id)
      addForm.value = { url: '', dir: '', out: '' }
      tab.value = 'tasks'
      await refreshTasks()
    }
  } catch (e) {
    showToast('添加失败：' + e.message)
  }
}

async function pauseTask(t) {
  try {
    await invoke('pause', { id: t.id })
    await refreshTasks()
  } catch (e) {
    showToast('操作失败：' + e.message)
  }
}

async function removeTask(t, files) {
  try {
    await invoke('remove', { id: t.id, files })
    await refreshTasks()
  } catch (e) {
    showToast('删除失败：' + e.message)
  }
}

// ── 插件页面 / 动作 / 命令 ──
async function openPlugin(plugin, page, builtin) {
  tab.value = 'plugin'
  currentKey.value = plugin + '/' + page
  try {
    pluginView.value = await invoke('ui_render', { plugin, page, ctx: { builtin: builtin || null } })
  } catch (e) {
    pluginView.value = { type: 'text', value: '渲染失败：' + e.message }
  }
}

async function nav(page) {
  const rep = ui.value.trusted && ui.value.replace && ui.value.replace[page]
  if (rep) {
    await openPlugin(rep.plugin, rep.page, page)
    return
  }
  tab.value = page
}

async function runAction(a, payload) {
  try {
    const r = await invoke('run_action', { plugin: a.plugin, action: a.id, payload: payload || {} })
    const text = r && (r.message || r.text) ? r.message || r.text : '动作已执行'
    showToast(text)
    if (tab.value === 'plugin') await openPlugin(...currentKey.value.split('/'))
  } catch (e) {
    showToast('动作失败：' + e.message)
  }
}

async function runCommand(c) {
  cmdOpen.value = false
  try {
    const r = await invoke('run_command', { plugin: c.plugin, command: c.id })
    showToast((r && (r.message || r.text)) || '命令已执行')
    await loadUi()
  } catch (e) {
    showToast('命令失败：' + e.message)
  }
}

async function setPluginSetting(s, value) {
  try {
    ui.value = await invoke('plugin_setting_set', { plugin: s.plugin, key: s.key, value: String(value) })
    showToast('已保存 ' + s.label)
  } catch (e) {
    showToast('保存失败：' + e.message)
  }
}

provide('adlDispatch', async (ev) => {
  if (ev.kind === 'action') {
    await runAction({ plugin: ev.plugin, id: ev.action }, ev.payload)
  } else if (ev.kind === 'setting') {
    await setPluginSetting({ plugin: ev.plugin, key: ev.key, label: ev.key }, ev.value)
  }
})

function onKeydown(e) {
  if ((e.ctrlKey || e.metaKey) && e.key.toLowerCase() === 'k') {
    e.preventDefault()
    if (ui.value.trusted && (ui.value.commands || []).length) cmdOpen.value = !cmdOpen.value
  }
  if (e.key === 'Escape') cmdOpen.value = false
}

onMounted(async () => {
  await loadSettings()
  await loadUi()
  await refreshTasks()
  pollTimer = setInterval(refreshTasks, 800)
  window.addEventListener('keydown', onKeydown)

  unlisteners.push(
    listen('progress', (p) => {
      const t = tasks.value.find((x) => x.id === p.id)
      if (t) {
        t.completed = p.completed
        t.total = p.total
        t.speed = p.speed
      }
    })
  )
  unlisteners.push(
    listen('task_done', (p) => {
      const t = tasks.value.find((x) => x.id === p.id)
      if (t) {
        t.status = p.status
        if (p.path) t.path = p.path
        if (p.error) t.error = p.error
      }
      if (p.status === 'completed') showToast('下载完成 #' + p.id)
      else showToast('下载失败 #' + p.id + '：' + (p.error || ''))
      refreshTasks()
    })
  )
})

onBeforeUnmount(() => {
  if (pollTimer) clearInterval(pollTimer)
  window.removeEventListener('keydown', onKeydown)
  for (const off of unlisteners) off()
})
</script>

<template>
  <div class="app">
    <aside class="sidebar">
      <div class="brand">ADL</div>
      <button class="nav-item" :class="{ active: tab === 'tasks' }" @click="nav('tasks')">
        任务列表
      </button>
      <button class="nav-item" :class="{ active: tab === 'add' }" @click="nav('add')">
        添加下载
      </button>
      <button class="nav-item" :class="{ active: tab === 'settings' }" @click="nav('settings')">
        设置
      </button>
      <template v-if="pluginPages.length">
        <div class="nav-section">插件页面</div>
        <button
          v-for="p in pluginPages"
          :key="p.plugin + '/' + p.id"
          class="nav-item"
          :class="{ active: tab === 'plugin' && currentKey === p.plugin + '/' + p.id }"
          @click="openPlugin(p.plugin, p.id)"
        >
          {{ p.title || p.id }}
        </button>
      </template>
      <div class="sidebar-foot">
        <span v-if="ui.trusted" class="mode-pill trusted">信任模式</span>
        <span v-else class="mode-pill">安全模式</span>
        <span v-if="ui.trusted && ui.commands.length" class="hint">Ctrl+K 命令</span>
      </div>
    </aside>

    <main class="main">
      <!-- 任务列表 -->
      <section v-if="tab === 'tasks'" class="card">
        <div class="row" style="justify-content: space-between">
          <h2>任务列表</h2>
          <div class="row" v-if="toolbarActions.length">
            <button v-for="a in toolbarActions" :key="a.plugin + a.id" class="ghost" @click="runAction(a)">
              {{ a.label || a.id }}
            </button>
          </div>
        </div>
        <div v-if="tasks.length === 0" class="empty">还没有任务，去「添加下载」新建一个吧。</div>
        <div v-for="t in tasks" :key="t.id" class="task">
          <div class="task-url">#{{ t.id }} · {{ t.url }}</div>
          <div class="task-meta">
            <span>
              <span class="status-tag" :class="t.status">{{ statusText(t.status) }}</span>
              <span style="margin-left: 8px">
                {{ formatBytes(t.completed) }} / {{ formatBytes(t.total) }}
                · {{ formatSpeed(t.speed) }}
              </span>
            </span>
            <span>{{ percent(t) }}%</span>
          </div>
          <div class="bar">
            <div class="bar-fill" :class="{ failed: t.status === 'failed' }" :style="{ width: percent(t) + '%' }"></div>
          </div>
          <div class="row" style="margin-top: 8px">
            <button v-if="t.status === 'downloading'" class="ghost" @click="pauseTask(t)">暂停</button>
            <button v-for="a in taskActions" :key="a.plugin + a.id" class="ghost" @click="runAction(a, { task: t })">
              {{ a.label || a.id }}
            </button>
            <button class="danger" @click="removeTask(t, false)">移除</button>
            <button class="danger" @click="removeTask(t, true)">删除文件</button>
            <span v-if="t.error" style="color: var(--danger); font-size: 12px">{{ t.error }}</span>
          </div>
        </div>
      </section>

      <!-- 添加下载 -->
      <section v-if="tab === 'add'" class="card">
        <h2>添加下载</h2>
        <label>下载地址（URL）</label>
        <input v-model="addForm.url" placeholder="https://example.com/file.zip" @keyup.enter="addDownload" />
        <label>保存目录（留空使用设置里的默认目录）</label>
        <input v-model="addForm.dir" placeholder="E:\Downloads" />
        <label>文件名（留空由服务器/URL 推断）</label>
        <input v-model="addForm.out" placeholder="可选" />
        <div class="row" style="margin-top: 18px">
          <button @click="addDownload">开始下载</button>
        </div>
      </section>

      <!-- 设置 -->
      <section v-if="tab === 'settings'" class="card">
        <h2>设置</h2>
        <label>默认保存目录</label>
        <input v-model="settings.dir" placeholder="E:\Downloads" />
        <label>并发连接数（1–64，按服务器实际接受的量自适应）</label>
        <input v-model.number="settings.connections" type="number" min="1" max="64" />
        <label>限速（字节/秒，0 表示不限速）</label>
        <input v-model.number="settings.limit" type="number" min="0" />
        <label>代理（如 http://127.0.0.1:7890，留空不用）</label>
        <input v-model="settings.proxy" placeholder="http://127.0.0.1:7890" />
        <label>插件模式</label>
        <select v-model="settings.mode">
          <option value="safe">安全模式（仅 Observe / Guard）</option>
          <option value="trusted">信任模式（允许 Transform）</option>
        </select>
        <label>强制 HTTP/1.1</label>
        <select v-model="settings.http1">
          <option :value="false">否（自动，优先 HTTP/2）</option>
          <option :value="true">是</option>
        </select>
        <label>插件目录（留空使用 exe 同级 plugins/）</label>
        <input v-model="settings.plugin_dir" placeholder="可选" />
        <div class="row" style="margin-top: 18px">
          <button @click="saveSettings">保存设置</button>
        </div>

        <template v-if="ui.settings.length">
          <hr class="rn-divider" style="margin: 22px 0" />
          <h3 style="font-size: 15px; margin-bottom: 10px">插件设置</h3>
          <div v-for="s in ui.settings" :key="s.plugin + '/' + s.key" class="plugin-setting">
            <label>{{ s.label }} <small style="opacity: 0.55">（{{ s.plugin }}）</small></label>
            <input
              v-if="s.kind === 'boolean'"
              type="checkbox"
              :checked="s.value === 'true'"
              :disabled="!s.editable"
              @change="setPluginSetting(s, $event.target.checked ? 'true' : 'false')"
            />
            <select
              v-else-if="s.kind === 'select'"
              :disabled="!s.editable"
              :value="s.value"
              @change="setPluginSetting(s, $event.target.value)"
            >
              <option v-for="(o, i) in s.options || []" :key="i" :value="o.value ?? o">{{ o.label ?? o }}</option>
            </select>
            <input
              v-else
              :type="s.kind === 'number' ? 'number' : 'text'"
              :disabled="!s.editable"
              :value="s.value"
              @change="setPluginSetting(s, $event.target.value)"
            />
          </div>
          <p v-if="!ui.trusted" class="hint">（安全模式下插件设置只读，切到信任模式可编辑）</p>
        </template>
      </section>

      <!-- 插件页面 -->
      <section v-if="tab === 'plugin'" class="card">
        <h2>{{ currentKey }}</h2>
        <RenderNode v-if="pluginView" :node="pluginView" :plugin="currentKey.split('/')[0]" />
      </section>
    </main>

    <!-- 命令面板 -->
    <div v-if="cmdOpen" class="palette-mask" @click.self="cmdOpen = false">
      <div class="palette">
        <input v-model="cmdQuery" placeholder="输入命令…（Esc 关闭）" autofocus />
        <div v-for="c in filteredCommands" :key="c.plugin + c.id" class="palette-item" @click="runCommand(c)">
          {{ c.title || c.id }} <small>{{ c.plugin }}</small>
        </div>
        <div v-if="!filteredCommands.length" class="empty" style="padding: 12px">没有匹配的命令</div>
      </div>
    </div>

    <div v-if="toast" class="toast">{{ toast }}</div>
  </div>
</template>
