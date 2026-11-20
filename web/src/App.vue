<script setup>
import { ref, computed, onMounted, onBeforeUnmount } from 'vue'
import { invoke, listen } from './ipc.js'

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
  return (
    { downloading: '下载中', paused: '已暂停', completed: '已完成', failed: '失败' }[s] || s
  )
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

onMounted(async () => {
  await loadSettings()
  await refreshTasks()
  pollTimer = setInterval(refreshTasks, 800)

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
  for (const off of unlisteners) off()
})
</script>

<template>
  <div class="app">
    <aside class="sidebar">
      <div class="brand">ADL</div>
      <button class="nav-item" :class="{ active: tab === 'tasks' }" @click="tab = 'tasks'">
        任务列表
      </button>
      <button class="nav-item" :class="{ active: tab === 'add' }" @click="tab = 'add'">
        添加下载
      </button>
      <button class="nav-item" :class="{ active: tab === 'settings' }" @click="tab = 'settings'">
        设置
      </button>
    </aside>

    <main class="main">
      <!-- 任务列表 -->
      <section v-if="tab === 'tasks'" class="card">
        <h2>任务列表</h2>
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
            <div
              class="bar-fill"
              :class="{ failed: t.status === 'failed' }"
              :style="{ width: percent(t) + '%' }"
            ></div>
          </div>
          <div class="row" style="margin-top: 8px">
            <button
              v-if="t.status === 'downloading'"
              class="ghost"
              @click="pauseTask(t)"
            >
              暂停
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
      </section>
    </main>

    <div v-if="toast" class="toast">{{ toast }}</div>
  </div>
</template>
