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

function taskStateOf(data: unknown): TaskState | null {
  return typeof data === 'string' && ['queued', 'running', 'suspended', 'completed', 'failed', 'canceled'].includes(data)
    ? (data as TaskState)
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
