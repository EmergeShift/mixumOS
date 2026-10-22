// WSL 后端全链验证:握手/快照/回退物化。
// 用法:
//   node wsl-probe.mjs <token>                 — 握手 + sys.info + snap1(首快照)
//   node wsl-probe.mjs <token> restore <snap1> — snap2(改动后)→ restore 回 snap1
process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0'
const token = process.argv[2]
const phase = process.argv[3] ?? 'snap1'
const ws = new WebSocket('wss://127.0.0.1:7601/ws')
const pend = {}
ws.addEventListener('message', (ev) => {
  const e = JSON.parse(String(ev.data))
  const w = e.replyTo && pend[e.replyTo]
  if (w) { delete pend[e.replyTo]; w(e.payload) }
})
const call = (op, payload) => new Promise((res, rej) => {
  const env = { v: 1, id: crypto.randomUUID(), op, ts: Date.now(), ...(payload ? { payload } : {}) }
  const t = setTimeout(() => rej(new Error('timeout ' + op)), 15000)
  pend[env.id] = (p) => { clearTimeout(t); res(p) }
  ws.send(JSON.stringify(env))
})
ws.addEventListener('open', async () => {
  try {
    const h = await call('hello', { token, client: 'wsl-e2e/0.1' })
    const i = await call('sys.info', {})
    console.log('hello:', h.server, '| os:', i.os, '| backend:', i.snapshotBackend, '| kernel:', i.kernel)
    if (phase === 'snap1') {
      const s = await call('snap.create', { kind: 'disk', label: 'wsl 首快照' })
      console.log('snap1:', s.id)
    } else if (phase === 'restore') {
      const snap1 = process.argv[4]
      const s2 = await call('snap.create', { kind: 'disk', label: '改动后' })
      console.log('snap2:', s2.id)
      const r = await call('snap.restore', { node: snap1, strategy: 'inplace' })
      console.log('restore:', r.restoredTo === snap1 ? 'OK' : 'MISMATCH ' + JSON.stringify(r))
    }
    process.exit(0)
  } catch (e) { console.log('ERR:', e.message); process.exit(1) }
})
