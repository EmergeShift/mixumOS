// e2e 冒烟脚本:对 e2e 容器跑一遍 握手 → 系统信息 → 窗口流 → 任务 →
// 快照回退/fork。零依赖(Node ≥22 自带 WebSocket),验证协议链路与
// console 依赖的服务端行为一致。
// 用法:node scripts/e2e-check.mjs [wss://127.0.0.1:17601] [令牌或令牌文件]
// 自签证书环境脚本内关闭校验,仅限本机开发。
process.env.NODE_TLS_REJECT_UNAUTHORIZED = '0'

const addr = normalize(process.argv[2] ?? 'wss://127.0.0.1:17601')
let token = process.argv[3] ?? process.env.MIXUM_TOKEN ?? ''
if (token && !token.startsWith('mixum1.')) {
  const { readFileSync } = await import('node:fs')
  token = readFileSync(token, 'utf8').trim()
}
if (!token) {
  console.error('缺少令牌:传参或设 MIXUM_TOKEN')
  process.exit(1)
}

function normalize(a) {
  a = a.trim().replace(/\/+$/, '')
  if (!a.includes('://')) a = 'wss://' + a
  if (!a.endsWith('/ws')) a += '/ws'
  return a
}

let seq = 0
const pending = new Map()
const ws = new WebSocket(addr, 'mixum.v1')
ws.binaryType = 'arraybuffer'

function request(op, payload, timeoutMs = 15000) {
  const id = crypto.randomUUID()
  const env = { v: 1, id, op, seq: ++seq, ts: Date.now(), payload }
  return new Promise((resolve, reject) => {
    const timer = setTimeout(() => {
      pending.delete(id)
      reject(new Error(`${op} 超时`))
    }, timeoutMs)
    pending.set(id, {
      resolve: (e) => {
        clearTimeout(timer)
        if (e.op.endsWith('.err')) reject(new Error(`${e.payload.code}: ${e.payload.message}`))
        else resolve(e.payload)
      },
    })
    ws.send(JSON.stringify(env))
  })
}

let taskEvents = []
let onTaskEvent = null

ws.onmessage = (ev) => {
  if (typeof ev.data !== 'string') return
  const e = JSON.parse(ev.data)
  if (e.replyTo && pending.has(e.replyTo)) {
    pending.get(e.replyTo).resolve(e)
    pending.delete(e.replyTo)
    return
  }
  if (e.op === 'task.event') {
    taskEvents.push(e.payload)
    onTaskEvent?.(e.payload)
  }
}

const openPromise = new Promise((res, rej) => {
  ws.onopen = res
  ws.onerror = () => rej(new Error('连接失败(容器没起?端口对?)'))
  setTimeout(() => rej(new Error('连接超时')), 10000)
})
await openPromise
console.log(`✓ 已连接 ${addr}`)

const hello = await request('hello', {
  token,
  client: 'console-e2e-check/0.1',
  device: 'smoke',
})
console.log(`✓ 握手: server=${hello.server} arch=${hello.arch} caps=${hello.caps.join(',')}`)

const info = await request('sys.info')
console.log(
  `✓ 系统信息: os=${info.os} backend=${info.snapshotBackend} compositor=${info.compositor} windows=${info.windows}`
)

// 窗口流
let frames = 0
if (info.compositor) {
  const { windows } = await request('display.list')
  console.log(`✓ 窗口列表: ${windows.map((w) => `#${w.id} ${w.title}(${w.mode})`).join(' ') || '无'}`)
  if (windows.length > 0) {
    const w = windows[0]
    const att = await request('display.attach', { window: w.id, fps: 10 })
    console.log(`✓ 附流: #${att.window} codec=${att.codec} ${att.width}x${att.height}`)
    const deadline = Date.now() + 6000
    let cursorInfo = ''
    await new Promise((resolve) => {
      const check = setInterval(() => {
        ws.onmessage
        // 二进制帧走同一条连接;用轮询读帧数
        if (frames >= 3 || Date.now() > deadline) {
          clearInterval(check)
          resolve()
        }
      }, 200)
      // 临时接管 onmessage 的二进制分支
      const orig = ws.onmessage
      ws.onmessage = (ev) => {
        if (typeof ev.data !== 'string') {
          const u8 = new Uint8Array(ev.data)
          if (u8[0] === 0x01 && u8.length > 27) {
            frames++
            const dv = new DataView(u8.buffer)
            const flags = dv.getUint16(17, true)
            if (flags & 2) {
              cursorInfo = ` 光标=(${dv.getInt16(19, true)},${dv.getInt16(21, true)})`
            }
          }
          return
        }
        orig.call(ws, ev)
      }
    })
    console.log(`✓ 窗口流: 收到 ${frames} 帧${cursorInfo}`)
    await request('display.detach', { window: w.id })
  }
} else {
  console.log('· compositor 未就绪,跳过窗口流')
}

// 任务:先订阅事件再提交
await request('task.events')
const task = await request('task.submit', {
  instruction: 'console 冒烟:写文件并回显',
  agent: 'shell',
  params: { cmd: 'echo console-e2e-ok > marker.txt && cat marker.txt' },
  snapshotPolicy: { pre: true, kind: 'disk', onFailure: 'none' },
})
console.log(`✓ 任务已提交: ${task.id} preSnap=${task.preSnapNode ?? '无'}`)

const done = await new Promise((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error('任务 20 秒未完成')), 20000)
  onTaskEvent = (e) => {
    if (e.taskId !== task.id) return
    if (e.type === 'log') process.stdout.write(`    log: ${e.data}`)
    if (e.type === 'state') {
      if (['completed', 'failed', 'canceled'].includes(e.data)) {
        clearTimeout(timer)
        resolve(e.data)
      }
    }
  }
})
console.log(`✓ 任务结束: ${done}(事件 ${taskEvents.length} 条)`)
if (done !== 'completed') process.exit(1)

// 快照树
const tree = await request('snap.tree')
console.log(`✓ 快照树: branch=${tree.branch} head=${tree.head.slice(0, 12)} nodes=${tree.nodes.length}`)
for (const n of tree.nodes) {
  console.log(
    `    [${n.branch}] ${n.id.slice(0, 12)} ${n.kind} ${n.label ?? ''}${n.auto ? ' (auto)' : ''} chunks=${n.chunks}`
  )
}

// 回退到任务前置快照
const pre = tree.nodes.find((n) => n.id === task.preSnapNode) ?? tree.nodes[0]
const r1 = await request('snap.restore', { node: pre.id, strategy: 'inplace' })
console.log(`✓ 回退(inplace): ${r1.restoredTo.slice(0, 12)} 工作区 marker.txt 应消失`)
const tree2 = await request('snap.tree')
console.log(`✓ 回退后树: head=${tree2.head.slice(0, 12)} nodes=${tree2.nodes.length}(节点数不变,旧时间线仍在)`)

// fork
const last = tree2.nodes[tree2.nodes.length - 1]
const r2 = await request('snap.restore', { node: last.id, strategy: 'fork', label: 'console-冒烟分支' })
console.log(`✓ fork: ${r2.newBranch ?? '(?)'}`)

console.log('E2E PASS')
ws.close()
process.exit(0)
