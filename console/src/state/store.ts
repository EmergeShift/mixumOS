// 全局状态(zustand)。高频视频帧不进 store,由 WindowCanvas 直接订阅
// client.onFrame;store 只放低频的结构化数据。
import { create } from 'zustand'
import { MixumClient, type ConnStatus } from '../protocol/client'
import type {
  HelloResponse,
  SnapTreeResponse,
  Task,
  TaskEvent,
  TaskState,
  WindowInfo,
} from '../protocol/types'

const LS_ADDR = 'mixum.console.addr'
const LS_TOKEN = 'mixum.console.token'
const LS_DEVICE = 'mixum.console.device'
const MAX_EVENTS = 2000

export const client = new MixumClient()

export type NarrowView = 'tasks' | 'windows' | 'snaps'

interface ConsoleStore {
  status: ConnStatus
  statusDetail: string
  hello: HelloResponse | null

  address: string
  token: string
  device: string

  tasks: Record<string, Task>
  events: TaskEvent[]
  selectedTask: string | null

  windows: WindowInfo[]
  activeWindow: number | null

  // gateway 多节点
  nodes: string[]
  selectedNode: string
  devices: { jti: string; name: string; sub: string; iat: number }[]

  snapTree: SnapTreeResponse | null
  snapsLoading: boolean

  narrowView: NarrowView
  settingsOpen: boolean
  toast: { text: string; kind: 'info' | 'error' } | null

  setSetting: (k: 'address' | 'token' | 'device', v: string) => void
  connect: () => void
  disconnect: () => void
  refreshTasks: () => Promise<void>
  refreshWindows: () => Promise<void>
  refreshSnaps: () => Promise<void>
  selectTask: (id: string | null) => void
  openWindow: (id: number | null) => void
  setNarrowView: (v: NarrowView) => void
  setSettingsOpen: (b: boolean) => void
  showToast: (text: string, kind?: 'info' | 'error') => void
}

function lsGet(k: string): string {
  try {
    return localStorage.getItem(k) ?? ''
  } catch {
    return ''
  }
}

const TASK_STATES: TaskState[] = ['queued', 'running', 'suspended', 'completed', 'failed', 'canceled']

function taskStateOf(data: unknown): TaskState | null {
  // MAP: data 是对象 {state};过渡期兼容旧节点的裸字符串。
  let value: unknown = typeof data === 'string' ? data : undefined
  if (value === undefined && data && typeof data === 'object') {
    value = (data as { state?: unknown }).state
  }
  return typeof value === 'string' && TASK_STATES.includes(value as TaskState)
    ? (value as TaskState)
    : null
}

export const useStore = create<ConsoleStore>((set, get) => ({
  status: 'disconnected',
  statusDetail: '',
  hello: null,

  address: lsGet(LS_ADDR) || '/ws',
  token: lsGet(LS_TOKEN),
  device: lsGet(LS_DEVICE) || 'console',

  tasks: {},
  events: [],
  selectedTask: null,

  windows: [],
  activeWindow: null,

  nodes: [],
  selectedNode: '',
  devices: [],

  snapTree: null,
  snapsLoading: false,

  narrowView: 'tasks',
  settingsOpen: false,
  toast: null,

  setSetting: (k, v) => set({ [k]: v } as Partial<ConsoleStore>),

  connect: () => {
    const { address, token, device } = get()
    try {
      localStorage.setItem(LS_ADDR, address)
      localStorage.setItem(LS_TOKEN, token)
      localStorage.setItem(LS_DEVICE, device)
    } catch {
      /* 隐私模式等场景忽略 */
    }
    client.connect(address, token, device)
  },

  disconnect: () => {
    client.disconnect()
    set({ status: 'disconnected', statusDetail: '', hello: null, windows: [], activeWindow: null })
  },

  refreshTasks: async () => {
    if (!client.connected) return
    try {
      const { tasks } = await client.listTasks()
      const map: Record<string, Task> = {}
      for (const t of tasks) map[t.id] = t
      set({ tasks: map })
    } catch (err) {
      get().showToast(`拉取任务列表失败:${msg(err)}`, 'error')
    }
  },

  refreshWindows: async () => {
    if (!client.connected) return
    try {
      const r = await client.request<{ windows: WindowInfo[] }>('display.list')
      useStore.setState({ windows: r.windows })
    } catch (err) {
      get().showToast(`拉取窗口列表失败:${msg(err)}`, 'error')
    }
  },

  refreshSnaps: async () => {
    if (!client.connected) return
    set({ snapsLoading: true })
    try {
      const tree = await client.snapTree()
      set({ snapTree: tree, snapsLoading: false })
    } catch (err) {
      set({ snapsLoading: false })
      get().showToast(`拉取快照树失败:${msg(err)}`, 'error')
    }
  },

  selectTask: (id) => set({ selectedTask: id }),
  openWindow: (id) => set({ activeWindow: id }),
  setNarrowView: (v) => set({ narrowView: v }),
  setSettingsOpen: (b) => set({ settingsOpen: b }),

  showToast: (text, kind = 'info') => {
    set({ toast: { text, kind } })
    setTimeout(() => {
      if (get().toast?.text === text) set({ toast: null })
    }, 3500)
  },
}))

export function msg(err: unknown): string {
  return err instanceof Error ? err.message : String(err)
}

export function selectNode(node: string): void {
  useStore.setState({ selectedNode: node })
  client.node = node
}

export async function refreshDevices(): Promise<void> {
  try {
    const r = await client.deviceList()
    useStore.setState({ devices: r.devices })
  } catch {
    useStore.setState({ devices: [] })
  }
}

export function showToast2(text: string, kind: 'info' | 'error' = 'info'): void {
  useStore.getState().showToast(text, kind)
}

// ---------- client 回调接线 ----------

client.onStatus = (s, detail) => useStore.setState({ status: s, statusDetail: detail ?? '' })

client.onHello = (h) => {
  useStore.setState({ hello: h })
  const st = useStore.getState()
  void st.refreshTasks()
  void st.refreshSnaps()
  void client
    .request<{ windows: WindowInfo[] }>('display.list')
    .then((r) => useStore.setState({ windows: r.windows }))
    .catch(() => {})
  // gateway:拉节点列表并自动选第一个(单节点场景无感知)
  if (client.isGateway()) {
    void client
      .nodeList()
      .then((r) => {
        useStore.setState((s) => ({
          nodes: r.nodes,
          selectedNode: s.selectedNode && r.nodes.includes(s.selectedNode)
            ? s.selectedNode
            : (r.nodes[0] ?? ''),
        }))
        if (useStore.getState().selectedNode) client.node = useStore.getState().selectedNode
      })
      .catch(() => {})
  }
}

client.onTaskEvent = (e) => {
  useStore.setState((s) => {
    const events = s.events.length >= MAX_EVENTS ? s.events.slice(1) : s.events.slice()
    events.push(e)
    const tasks = { ...s.tasks }
    if (e.type === 'state') {
      const st = taskStateOf(e.data)
      if (st && tasks[e.taskId]) tasks[e.taskId] = { ...tasks[e.taskId], state: st }
    }
    return { events, tasks }
  })
}

client.onWindowPush = (w) => {
  useStore.setState((s) => {
    if (w.title === '__closed__') {
      return {
        windows: s.windows.filter((x) => x.id !== w.id),
        activeWindow: s.activeWindow === w.id ? null : s.activeWindow,
      }
    }
    const windows = s.windows.slice()
    const i = windows.findIndex((x) => x.id === w.id)
    if (i >= 0) windows[i] = w
    else windows.push(w)
    return { windows }
  })
}

// ---------- 剪贴板同步（console ↔ 容器选区，回声抑制） ----------
// 双向各记最近一轮文本：容器推来的文本若正是刚推过去的（回声）则不写
// 浏览器；浏览器读到的文本若与两端已知一致则不重发，失败时保留重试机会。
let clipLastSent = '' // 最近一次成功推给容器的文本
let clipLastFromRemote = '' // 最近一次容器推来的文本

client.onClipboard = (_window, text) => {
  clipLastFromRemote = text
  if (!text || text === clipLastSent) return
  navigator.clipboard?.writeText(text).catch(() => {
    /* 无剪贴板权限或文档非焦点：静默（用户可手动复制） */
  })
}

async function pushClipboard(): Promise<void> {
  if (!client.connected) return
  try {
    const text = await navigator.clipboard?.readText()
    if (!text || text === clipLastSent || text === clipLastFromRemote) return
    const s = useStore.getState()
    const win = s.activeWindow ?? s.windows[0]?.id
    if (win == null) return
    await client.clipboardSet(win, text)
    clipLastSent = text
  } catch {
    /* 无剪贴板权限：静默 */
  }
}

window.addEventListener('focus', () => {
  void pushClipboard()
})
document.addEventListener('paste', () => {
  void pushClipboard()
})
