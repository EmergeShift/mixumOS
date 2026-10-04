// 协议客户端:WebSocket + envelope 请求-应答关联 + 二进制帧分发 + 自动重连。
// 对齐 docs/protocol.md:子协议 mixum.v1,reply op = 请求 op + .ok/.err,
// 回复用 replyTo 指向请求 id。参考实现 reference/cmd/e2eclient/main.go。
import { parseVideoFrame } from './video'
import type {
  DisplayAttachResponse,
  DisplayInputRequest,
  Envelope,
  ErrorPayload,
  HelloResponse,
  SnapRestoreResponse,
  SnapTreeResponse,
  Task,
  TaskEvent,
  TaskSubmitRequest,
  VideoFrame,
  WindowInfo,
} from './types'

export type ConnStatus = 'disconnected' | 'connecting' | 'connected'

export class ProtocolError extends Error {
  code: string
  constructor(code: string, message: string) {
    super(message)
    this.code = code
    this.name = 'ProtocolError'
  }
}

interface Pending {
  resolve: (payload: unknown) => void
  reject: (err: Error) => void
  timer: ReturnType<typeof setTimeout>
}

const DEFAULT_TIMEOUT = 15_000
const MAX_RETRY_DELAY = 10_000
const SUBPROTOCOL = 'mixum.v1'

export function normalizeAddr(addr: string): string {
  return addr.trim().replace(/\/+$/, '')
}

export class MixumClient {
  private ws: WebSocket | null = null
  private pending = new Map<string, Pending>()
  private seq = 0
  private retry = 0
  private wantClosed = false
  private hello: HelloResponse | null = null
  private heartbeatTimer: ReturnType<typeof setInterval> | null = null
  private reconnectTimer: ReturnType<typeof setTimeout> | null = null
  private missPings = 0

  // 连接参数(重连时复用)
  address = ''
  token = ''
  device = 'console'

  // 重连后需要恢复的订阅
  private attached = new Map<number, number>() // window -> fps

  // 事件回调(由 store 接线)
  onStatus: (s: ConnStatus, detail?: string) => void = () => {}
  onHello: (h: HelloResponse) => void = () => {}
  onTaskEvent: (e: TaskEvent) => void = () => {}
  onWindowPush: (w: WindowInfo) => void = () => {}
  onFrame: (f: VideoFrame) => void = () => {}

  get connected(): boolean {
    return this.ws?.readyState === WebSocket.OPEN && this.hello !== null
  }

  get serverInfo(): HelloResponse | null {
    return this.hello
  }

  connect(address: string, token: string, device: string): void {
    this.wantClosed = true // 中断可能在跑的重连循环
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer)
    this.reconnectTimer = null
    this.cleanup()
    this.address = normalizeAddr(address)
    this.token = token.trim()
    this.device = device || 'console'
    this.retry = 0
    this.wantClosed = false
    this.open()
  }

  disconnect(): void {
    this.wantClosed = true
    if (this.reconnectTimer) clearTimeout(this.reconnectTimer)
    this.reconnectTimer = null
    this.cleanup()
    this.onStatus('disconnected')
  }

  private cleanup(): void {
    if (this.ws) {
      const ws = this.ws
      this.ws = null
      ws.onopen = ws.onmessage = ws.onclose = ws.onerror = null
      try {
        ws.close()
      } catch {
        /* 忽略 */
      }
    }
    if (this.heartbeatTimer) {
      clearInterval(this.heartbeatTimer)
      this.heartbeatTimer = null
    }
    for (const [, p] of this.pending) {
      clearTimeout(p.timer)
      p.reject(new ProtocolError('internal', '连接已关闭'))
    }
    this.pending.clear()
    this.hello = null
  }

  private open(): void {
    if (this.wantClosed) return
    this.onStatus('connecting')
    let url = this.address
    if (!url.includes('://')) {
      // 相对地址(如 /ws 或空):走当前站点,开发中继走这里
      url = `${location.protocol === 'https:' ? 'wss' : 'ws'}://${location.host}${url || '/ws'}`
    }
    let ws: WebSocket
    try {
      ws = new WebSocket(url, SUBPROTOCOL)
    } catch (err) {
      this.onStatus('disconnected', String(err))
      return
    }
    ws.binaryType = 'arraybuffer'
    this.ws = ws

    ws.onopen = () => {
      const env = this.envelope('hello', {
        token: this.token,
        client: 'mixum-console/0.1',
        device: this.device,
      })
      const timer = setTimeout(() => {
        this.pending.delete(env.id)
        this.ws?.close() // 触发重连
      }, DEFAULT_TIMEOUT)
      this.pending.set(env.id, {
        resolve: (payload) => this.onHelloDone(payload as HelloResponse),
        reject: (err) => {
          // 握手失败(令牌错等):不无限重试,报给用户
          this.wantClosed = true
          this.cleanup()
          this.onStatus('disconnected', err.message)
        },
        timer,
      })
      this.sendEnvelope(env)
    }

    ws.onmessage = (ev) => {
      if (typeof ev.data === 'string') {
        this.onText(ev.data)
      } else {
        const frame = parseVideoFrame(new Uint8Array(ev.data))
        if (frame) this.onFrame(frame)
      }
    }

    ws.onclose = () => {
      if (this.ws !== ws) return // 已被 cleanup 换掉
      this.cleanup()
      if (!this.wantClosed) this.scheduleReconnect()
      else this.onStatus('disconnected')
    }
    ws.onerror = () => {
      /* onclose 会跟着来 */
    }
  }

  private onHelloDone(h: HelloResponse): void {
    this.hello = h
    this.retry = 0
    this.missPings = 0
    this.onHello(h)
    this.onStatus('connected')
    const period = Math.max(5, Math.min(h.heartbeat || 30, 120))
    this.heartbeatTimer = setInterval(() => {
      this.request('sys.ping')
        .then(() => {
          this.missPings = 0
        })
        .catch(() => {
          this.missPings += 1
          if (this.missPings >= 2) this.ws?.close() // 触发重连
        })
    }, period * 1000)
    void this.resubscribe()
  }

  private scheduleReconnect(): void {
    this.onStatus('connecting', `第 ${this.retry + 1} 次重连`)
    const delay = Math.min(1000 * 2 ** this.retry, MAX_RETRY_DELAY)
    this.retry += 1
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null
      this.open()
    }, delay)
  }

  private async resubscribe(): Promise<void> {
    try {
      await Promise.all([this.request('task.events'), this.request('display.windows')])
      for (const [win, fps] of this.attached) {
        await this.request<DisplayAttachResponse>('display.attach', { window: win, fps })
      }
    } catch {
      this.ws?.close() // 下一轮重连再试
    }
  }

  private onText(text: string): void {
    let env: Envelope
    try {
      env = JSON.parse(text) as Envelope
    } catch {
      return
    }
    if (env.replyTo) {
      const p = this.pending.get(env.replyTo)
      if (p) {
        this.pending.delete(env.replyTo)
        clearTimeout(p.timer)
        if (env.op.endsWith('.err')) {
          const e = (env.payload ?? {}) as ErrorPayload
          p.reject(new ProtocolError(e.code ?? 'internal', e.message ?? '未知错误'))
        } else {
          p.resolve(env.payload)
        }
      }
      return
    }
    switch (env.op) {
      case 'task.event':
        this.onTaskEvent(env.payload as TaskEvent)
        break
      case 'display.windows':
        this.onWindowPush(env.payload as WindowInfo)
        break
      default:
        break
    }
  }

  private envelope(op: string, payload?: unknown): Envelope {
    this.seq += 1
    return {
      v: 1,
      id: crypto.randomUUID(),
      op,
      seq: this.seq,
      ts: Date.now(),
      payload,
    }
  }

  private sendEnvelope(env: Envelope): boolean {
    if (this.ws?.readyState !== WebSocket.OPEN) return false
    this.ws.send(JSON.stringify(env))
    return true
  }

  request<T>(op: string, payload?: unknown, timeoutMs = DEFAULT_TIMEOUT): Promise<T> {
    return new Promise<T>((resolve, reject) => {
      if (!this.connected) {
        reject(new ProtocolError('internal', '未连接'))
        return
      }
      const env = this.envelope(op, payload)
      const timer = setTimeout(() => {
        this.pending.delete(env.id)
        reject(new ProtocolError('internal', `${op} 超时(${timeoutMs}ms)`))
      }, timeoutMs)
      this.pending.set(env.id, { resolve: resolve as (p: unknown) => void, reject, timer })
      if (!this.sendEnvelope(env)) {
        clearTimeout(timer)
        this.pending.delete(env.id)
        reject(new ProtocolError('internal', '连接已关闭'))
      }
    })
  }

  // ---------- 各 op 封装 ----------

  submitTask(req: TaskSubmitRequest): Promise<Task> {
    return this.request('task.submit', req, 30_000)
  }

  listTasks(): Promise<{ tasks: Task[] }> {
    return this.request('task.list')
  }

  cancelTask(id: string): Promise<{ id: string }> {
    return this.request('task.cancel', { id })
  }

  attach(windowId: number, fps = 15): Promise<DisplayAttachResponse> {
    return this.request<DisplayAttachResponse>('display.attach', { window: windowId, fps })
  }

  registerAttached(windowId: number, fps: number): void {
    this.attached.set(windowId, fps)
  }

  async detach(windowId: number): Promise<void> {
    this.attached.delete(windowId)
    try {
      await this.request('display.detach', { window: windowId })
    } catch {
      /* 窗口可能已消失 */
    }
  }

  input(req: DisplayInputRequest): Promise<{ ok: boolean }> {
    return this.request('display.input', req)
  }

  takeover(windowId: number, action: 'claim' | 'release', force = false): Promise<{ mode: string }> {
    return this.request<{ mode: string }>('display.takeover', { window: windowId, action, force })
  }

  snapTree(): Promise<SnapTreeResponse> {
    return this.request('snap.tree')
  }

  snapRestore(node: string, strategy: 'inplace' | 'fork', label?: string): Promise<SnapRestoreResponse> {
    return this.request('snap.restore', { node, strategy, label }, 60_000)
  }

  snapFork(node: string, branch: string): Promise<{ branch: string }> {
    return this.request('snap.fork', { node, branch })
  }
}
