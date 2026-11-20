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
    pending.set(id, { resolve, reject })
    if (window.ipc && typeof window.ipc.postMessage === 'function') {
      window.ipc.postMessage(JSON.stringify({ id, cmd, args }))
    } else {
      pending.delete(id)
      reject(new Error('当前不在 ADL 窗口内（无 window.ipc）'))
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
