// 与 Rust 宿主通信的 IPC 封装。
// 前端 → 宿主：window.ipc.postMessage(JSON.stringify({ id, cmd, args }))
// 宿主 → 前端：window.__ipcResolve(id, result) / window.__ipcEmit(event, payload)
// 这套协议沿用上一版已验证的实现。

let seq = 0
const pending = new Map()
const handlers = new Map()

export function invoke(cmd, args = {}) {
  return new Promise((resolve, reject) => {
    const id = ++seq
    if (window.ipc && typeof window.ipc.postMessage === 'function') {
      // 原生 WebView2 窗口：wry 自定义 IPC
      pending.set(id, { resolve, reject })
      window.ipc.postMessage(JSON.stringify({ id, cmd, args }))
    } else {
      // 浏览器降级模式（没有 WebView2 时）：走本地静态服务器的 POST /ipc
      fetch('/ipc', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({ id, cmd, args }),
      })
        .then((r) => r.json())
        .then((data) => {
          const payload = data && data.payload !== undefined ? data.payload : null
          if (payload && payload.__err) {
            reject(new Error(payload.__err.message || '未知错误'))
          } else {
            resolve(payload)
          }
        })
        .catch((e) => reject(e))
    }
  })
}

export function listen(event, fn) {
  if (!handlers.has(event)) handlers.set(event, new Set())
  handlers.get(event).add(fn)
  return () => handlers.get(event)?.delete(fn)
}

window.__ipcResolve = (id, result) => {
  const p = pending.get(id)
  if (!p) return
  pending.delete(id)
  if (result && result.__err) {
    p.reject(new Error(result.__err.message || '未知错误'))
  } else {
    p.resolve(result)
  }
}

window.__ipcEmit = (event, payload) => {
  const set = handlers.get(event)
  if (set) for (const fn of set) fn(payload)
}
