import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// 开发中继:浏览器连 ws://<dev 主机>/ws,vite dev server(Node)转发到
// 本机 mixumd。secure:false 只作用于本机 Node 进程,用于自签证书的本地
// 联调;npm run build 产物不经过此路径,直连需在系统层信任证书。
// 目标地址可用环境变量切换:MIXUM_DEV_TARGET=wss://127.0.0.1:17602 npm run dev
export default defineConfig({
  plugins: [react()],
  base: './',
  build: { target: 'es2022' },
  server: {
    proxy: {
      '/ws': {
        target: process.env.MIXUM_DEV_TARGET ?? 'wss://127.0.0.1:17601',
        secure: false,
        ws: true,
      },
    },
  },
})
