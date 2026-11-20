import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// 构建产物要被 Rust 的本地静态服务器直接托管，所以用相对 base。
export default defineConfig({
  plugins: [vue()],
  base: './',
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    // 小项目，不启用 sourcemap，省磁盘
    sourcemap: false,
  },
  server: {
    port: 1421,
    strictPort: false,
  },
})
