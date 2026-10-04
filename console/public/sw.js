// mixumOS 控制台 Service Worker:只缓存应用壳,不缓存数据。
// 协议数据全部走 WebSocket,不经过 HTTP 缓存;接口地址与令牌也不落 SW。
const CACHE = 'mixum-console-shell-v1'
const SHELL = ['./', './index.html', './manifest.json']

self.addEventListener('install', (e) => {
  e.waitUntil(
    caches.open(CACHE).then((c) => c.addAll(SHELL)).then(() => self.skipWaiting())
  )
})

self.addEventListener('activate', (e) => {
  e.waitUntil(
    caches
      .keys()
      .then((keys) => Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k))))
      .then(() => self.clients.claim())
  )
})

self.addEventListener('fetch', (e) => {
  const req = e.request
  if (req.method !== 'GET') return
  const url = new URL(req.url)
  if (url.origin !== location.origin) return

  // 导航:网络优先,失败回退到缓存壳(离线能看到界面,数据等重连)。
  if (req.mode === 'navigate') {
    e.respondWith(
      fetch(req)
        .then((resp) => {
          const copy = resp.clone()
          caches.open(CACHE).then((c) => c.put(req, copy))
          return resp
        })
        .catch(() => caches.match('./index.html'))
    )
    return
  }

  // 同源静态资源(构建产物带内容哈希):缓存优先。
  e.respondWith(
    caches.match(req).then(
      (hit) =>
        hit ||
        fetch(req).then((resp) => {
          if (resp.ok) {
            const copy = resp.clone()
            caches.open(CACHE).then((c) => c.put(req, copy))
          }
          return resp
        })
    )
  )
})
