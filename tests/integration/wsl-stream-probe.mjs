// WSL 窗口流探针:list 窗口 → attach jpeg → 统计 5s 帧数/字节。
process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0'
const token = process.argv[2]
const ws = new WebSocket('wss://127.0.0.1:7601/ws')
const pend = {}
ws.addEventListener('message', (ev) => {
  if (typeof ev.data === 'string') {
    const e = JSON.parse(ev.data)
    const w = e.replyTo && pend[e.replyTo]
    if (w) { delete pend[e.replyTo]; w(e.payload) }
    return
  }
  const b = new Uint8Array(ev.data)
  if (b[0] === 1 && b.length >= 27) {
    stats.frames += 1
    stats.bytes += b.length
  }
})
const stats = { frames: 0, bytes: 0 }
const call = (op, payload) => new Promise((res, rej) => {
  const env = { v: 1, id: crypto.randomUUID(), op, ts: Date.now(), ...(payload ? { payload } : {}) }
  const t = setTimeout(() => rej(new Error('timeout ' + op)), 15000)
  pend[env.id] = (p) => { clearTimeout(t); res(p) }
  ws.send(JSON.stringify(env))
})
ws.addEventListener('open', async () => {
  try {
    await call('hello', { token, client: 'wsl-stream-probe/0.1' })
    const list = await call('display.list', {})
    const w = list.windows[0]
    console.log('window:', w.id, w.title, w.width + 'x' + w.height, 'mode=' + w.mode)
    const a = await call('display.attach', { window: w.id, fps: 30, codec: 'jpeg' })
    console.log('attach:', a.codec, a.width + 'x' + a.height, a.fps + 'fps')
    await new Promise((r) => setTimeout(r, 5000))
    console.log('stream: 5s ' + stats.frames + ' 帧 ' + (stats.bytes / 1024).toFixed(0) + 'KiB(' + (stats.frames / 5).toFixed(1) + 'fps)')
    process.exit(0)
  } catch (e) { console.log('ERR:', e.message); process.exit(1) }
})
