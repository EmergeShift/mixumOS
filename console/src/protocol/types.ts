// 协议类型:逐字对齐 reference/internal/wire(payloads.go)的 JSON 字段,
// 不做任何「改良」。详见 docs/protocol.md §4-§5。

export interface Envelope {
  v: number
  id: string
  op: string
  seq?: number
  replyTo?: string
  ts: number
  payload?: unknown
}

export interface ErrorPayload {
  code: string
  message: string
}

export const ERR_CODES = [
  'bad_request',
  'unauthorized',
  'forbidden',
  'not_found',
  'conflict',
  'replay_detected',
  'busy',
  'internal',
  'unsupported',
  'rate_limited',
  'snapshot_invalid',
] as const

// ---------- 4.1 握手 ----------

export interface HelloResponse {
  sessionId: string
  server: string
  arch: string
  kernel: string
  caps: string[]
  versions: number[]
  heartbeat: number
}

// ---------- 4.2 任务 ----------

export interface SnapshotPolicy {
  pre?: boolean
  kind?: 'disk' | 'memory' | 'hybrid'
  onFailure?: 'none' | 'restore'
}

export interface TaskSubmitRequest {
  instruction: string
  agent?: string
  runner?: string
  params?: Record<string, string>
  snapshotPolicy?: SnapshotPolicy
}

export type TaskState = 'queued' | 'running' | 'suspended' | 'completed' | 'failed' | 'canceled'

export interface Task {
  id: string
  instruction: string
  agent: string
  runner: string
  state: TaskState
  preSnapNode?: string
  params?: Record<string, string>
  error?: string
  createdAt: number
  startedAt?: number
  endedAt?: number
}

export interface TaskEvent {
  taskId: string
  type: 'state' | 'log' | 'step' | 'progress' | 'error' | 'window'
  step?: string
  data?: unknown
  ts: number
}

// ---------- 4.3 快照 ----------

export interface SnapNode {
  id: string
  parent?: string
  branch: string
  label?: string
  kind: 'disk' | 'memory' | 'hybrid'
  auto?: boolean
  createdAt: number
  origin?: string
  sizeBytes: number
  chunks: number
  kernel?: string
  arch?: string
}

export interface SnapTreeResponse {
  head: string
  branch: string
  nodes: SnapNode[]
}

export interface SnapRestoreResponse {
  restoredTo: string
  newBranch?: string
  newNode?: string
}

// ---------- 4.4 显示 ----------

export type WindowMode = 'agent' | 'user' | 'free'

export interface WindowInfo {
  id: number
  appId: string
  title: string
  width: number
  height: number
  focused: boolean
  mode: WindowMode
  /** 窗口流实际编码(mixcomp 能力回告后随 window 事件到达) */
  codec?: 'jpeg' | 'h264'
}

export interface DisplayAttachResponse {
  window: number
  codec: string
  width: number
  height: number
  fps: number
}

export interface DisplayInputRequest {
  window: number
  type: 'pointer_motion' | 'pointer_button' | 'pointer_axis' | 'key' | 'text'
  x?: number
  y?: number
  button?: 'left' | 'right' | 'middle'
  action?: 'press' | 'release' | 'click' | 'scroll_up' | 'scroll_down'
  key?: number
  keysym?: string
  mods?: string[]
  text?: string
}

export interface AudioAttachResponse {
  sampleRate: number
  channels: number
  format: 's16le'
  chunkMs: number
}

export interface AudioChunk {
  seq: number
  sampleRate: number
  channels: number
  pcm: Int16Array
}

export interface DisplayTakeoverRequest {
  window: number
  action: 'claim' | 'release'
  force?: boolean
}

// ---------- 5. video 帧 ----------

export interface VideoFrameHeader {
  windowId: number
  width: number
  height: number
  size: number
  keyframe: boolean
  hasCursor: boolean
  /** 快照冻结状态帧：payload 为空，客户端保留最后一帧画面。 */
  frozen: boolean
  cursorX: number
  cursorY: number
  cursorW: number
  cursorH: number
}

export interface VideoFrame extends VideoFrameHeader {
  /** 编码数据:jpeg = JPEG,h264 = H.264 Annex-B(由 attach 协商决定) */
  payload: Uint8Array
  codec: 'jpeg' | 'h264'
  ts: number
}
