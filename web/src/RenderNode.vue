<script setup>
import { inject } from 'vue'

const props = defineProps({
  node: { type: Object, required: true },
  plugin: { type: String, default: '' },
})
const dispatch = inject('adlDispatch', null)

function kids(n) {
  return (n && n.children) || []
}

function onClick() {
  const n = props.node
  if (dispatch && n.action) {
    dispatch({ kind: 'action', plugin: props.plugin, action: n.action, payload: n.payload || {} })
  }
}

function onSettingInput(e) {
  const n = props.node
  if (dispatch && n.setting) {
    let v
    if (n.type === 'toggle') v = e.target.checked ? 'true' : 'false'
    else v = e.target.value
    dispatch({ kind: 'setting', plugin: props.plugin, key: n.setting, value: v })
  }
}

function onSelect(e) {
  const n = props.node
  if (dispatch && n.setting) {
    dispatch({ kind: 'setting', plugin: props.plugin, key: n.setting, value: e.target.value })
  }
}

function pct(n) {
  if (!n || !n.total) return 0
  return Math.min(100, Math.round((n.value || n.completed || 0) * 100 / Math.max(1, n.total)))
}
</script>

<template>
  <!-- 文本 -->
  <span v-if="node.type === 'text'" class="rn-text">{{ node.value }}</span>
  <h2 v-else-if="node.type === 'heading'" class="rn-heading">{{ node.value }}</h2>
  <div v-else-if="node.type === 'badge'" class="status-tag" :class="node.tone || 'completed'">
    {{ node.value }}
  </div>
  <hr v-else-if="node.type === 'divider'" class="rn-divider" />
  <div v-else-if="node.type === 'spacer'" class="rn-spacer" />

  <!-- 布局 -->
  <div v-else-if="node.type === 'row'" class="rn-row">
    <RenderNode v-for="(c, i) in kids(node)" :key="i" :node="c" :plugin="plugin" />
  </div>
  <div v-else-if="node.type === 'col' || node.type === 'column'" class="rn-col">
    <RenderNode v-for="(c, i) in kids(node)" :key="i" :node="c" :plugin="plugin" />
  </div>

  <!-- 交互 -->
  <button v-else-if="node.type === 'button'" class="rn-button" :class="node.kind || ''" @click="onClick">
    {{ node.label || node.value }}
  </button>
  <input
    v-else-if="node.type === 'input'"
    class="rn-input"
    :placeholder="node.placeholder || ''"
    :value="node.value || ''"
    @change="onSettingInput"
  />
  <select v-else-if="node.type === 'select'" class="rn-input" :value="node.value || ''" @change="onSelect">
    <option v-for="(o, i) in node.options || []" :key="i" :value="o.value ?? o">{{ o.label ?? o }}</option>
  </select>
  <label v-else-if="node.type === 'toggle'" class="rn-toggle">
    <input type="checkbox" :checked="node.value === true || node.value === 'true'" @change="onSettingInput" />
    <span>{{ node.label || '' }}</span>
  </label>

  <!-- 数据展示 -->
  <div v-else-if="node.type === 'progress'" class="bar">
    <div class="bar-fill" :style="{ width: pct(node) + '%' }"></div>
  </div>
  <ul v-else-if="node.type === 'list'" class="rn-list">
    <li v-for="(c, i) in kids(node)" :key="i">
      <RenderNode :node="c" :plugin="plugin" />
    </li>
  </ul>

  <!-- 未知类型：原样显示，便于插件调试 -->
  <span v-else class="rn-text" style="opacity: 0.5">[{{ node.type || '?' }}]</span>
</template>
