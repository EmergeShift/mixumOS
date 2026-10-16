// 窗口画布:display.attach 收 JPEG 帧(canvas 绘制,静默丢帧只画最新),
// 光标来自帧头元数据;接管后鼠标/键盘事件换算物理像素回传 display.input。
import { useEffect, useRef, useState } from 'react'
import { client, msg, useStore } from '../state/store'
import type { DisplayAttachResponse, VideoFrame, WindowInfo } from '../protocol/types'
import { toKeysym, modsOf } from '../util/keysym'
import { fmtTime } from '../util/format'

const ATTACH_FPS = 15

export function WindowCanvas({ win }: { win: WindowInfo }) {
  const showToast = useStore((s) => s.showToast)
  const refreshWindows = useStore((s) => s.refreshWindows)
  const openWindow = useStore((s) => s.openWindow)

  const canvasRef = useRef<HTMLCanvasElement>(null)
  const latestRef = useRef<VideoFrame | null>(null)
  const drawSeqRef = useRef(0)
  const rafRef = useRef(false)
  const physRef = useRef({ w: 0, h: 0 })
  const motionRef = useRef<{ x: number; y: number } | null>(null)
  const motionRafRef = useRef(false)
  const statsRef = useRef({ frames: 0, lastTs: 0 })
  const modeRef = useRef(win.mode)

  const [info, setInfo] = useState<DisplayAttachResponse | null>(null)
  const [stats, setStats] = useState({ frames: 0, lastTs: 0 })
  const [error, setError] = useState('')

  modeRef.current = win.mode

  // 附流 + 帧订阅
  useEffect(() => {
    let dead = false
    setError('')
    client
      .attach(win.id, ATTACH_FPS)
      .then((r) => {
        if (dead) return
        setInfo(r)
        client.registerAttached(win.id, ATTACH_FPS)
      })
      .catch((err) => {
        if (!dead) setError(msg(err))
      })

    const prevHandler = client.onFrame
    client.onFrame = (f) => {
      if (f.windowId !== win.id) return
      latestRef.current = f
      statsRef.current.frames += 1
      statsRef.current.lastTs = f.ts
      scheduleDraw()
    }

    return () => {
      dead = true
      client.onFrame = prevHandler
      void client.detach(win.id)
    }
  }, [win.id])

  // 统计低频刷新
  useEffect(() => {
    const t = setInterval(() => setStats({ ...statsRef.current }), 500)
    return () => clearInterval(t)
  }, [])

  function scheduleDraw() {
    if (rafRef.current) return
    rafRef.current = true
    const step = () => {
      rafRef.current = false
      void draw()
    }
    requestAnimationFrame(step)
    // 内嵌/后台环境不触发 rAF,定时器兜底(rafRef 防重复)
    setTimeout(() => {
      if (rafRef.current) step()
    }, 120)
  }

  async function draw() {
    const f = latestRef.current
    const canvas = canvasRef.current
    if (!f || !canvas) return
    latestRef.current = null // 取走即丢:绘制期间来的新帧下一轮画
    physRef.current = { w: f.width, h: f.height }
    if (canvas.width !== f.width || canvas.height !== f.height) {
      canvas.width = f.width
      canvas.height = f.height
    }
    const seq = ++drawSeqRef.current
    try {
      const bmp = await createImageBitmap(new Blob([f.jpeg as BlobPart], { type: 'image/jpeg' }))
      if (seq !== drawSeqRef.current) {
        bmp.close() // 已有更新帧,静默丢弃这帧
        return
      }
      const ctx = canvas.getContext('2d')
      if (!ctx) return
      ctx.drawImage(bmp, 0, 0)
      bmp.close()
      if (f.hasCursor) drawCursor(ctx, f)
    } catch {
      /* 解码失败丢帧 */
    }
  }

  function toPhys(e: { clientX: number; clientY: number }): { x: number; y: number } | null {
    const canvas = canvasRef.current
    if (!canvas) return null
    const { w, h } = physRef.current
    if (!w || !h) return null
    const r = canvas.getBoundingClientRect()
    return {
      x: Math.max(0, Math.min(w, (e.clientX - r.left) * (w / r.width))),
      y: Math.max(0, Math.min(h, (e.clientY - r.top) * (h / r.height))),
    }
  }

  function flushMotion() {
    motionRafRef.current = false
    const p = motionRef.current
    if (!p) return
    motionRef.current = null
    void client
      .input({ window: win.id, type: 'pointer_motion', x: p.x, y: p.y })
      .catch(() => {})
  }

  function onPointerMove(e: React.PointerEvent) {
    if (modeRef.current !== 'user') return
    const p = toPhys(e)
    if (!p) return
    motionRef.current = p
    if (!motionRafRef.current) {
      motionRafRef.current = true
      // 与绘制调度同理:rAF 优先,定时器兜底,保证任何环境都能发出
      const flush = () => {
        if (!motionRafRef.current) return
        motionRafRef.current = false
        flushMotion()
      }
      requestAnimationFrame(flush)
      setTimeout(flush, 120)
    }
  }

  function onPointerButton(e: React.PointerEvent, action: 'press' | 'release') {
    if (modeRef.current !== 'user') return
    const p = toPhys(e)
    if (!p) return
    const button = e.button === 2 ? 'right' : e.button === 1 ? 'middle' : 'left'
    void client
      .input({ window: win.id, type: 'pointer_button', button, action })
      .catch(() => {})
    e.preventDefault()
  }

  function onWheel(e: React.WheelEvent) {
    if (modeRef.current !== 'user') return
    void client
      .input({
        window: win.id,
        type: 'pointer_axis',
        action: e.deltaY < 0 ? 'scroll_up' : 'scroll_down',
      })
      .catch(() => {})
    e.preventDefault()
  }

  function onKey(e: React.KeyboardEvent, action: 'press' | 'release') {
    if (modeRef.current !== 'user') return
    // 浏览器保留键放行
    if (e.key === 'F5' || e.key === 'F11' || e.key === 'F12') return
    const keysym = toKeysym(e.nativeEvent)
    if (!keysym) return
    if (action === 'press' && e.repeat) return
    void client
      .input({
        window: win.id,
        type: 'key',
        keysym,
        action,
        mods: modsOf(e.nativeEvent),
      })
      .catch(() => {})
    e.preventDefault()
  }

  async function takeover(action: 'claim' | 'release', force = false) {
    try {
      if (force && !window.confirm('agent 正持有输入,强制接管会让 agent 失去输入。继续?')) return
      await client.takeover(win.id, action, force)
      await refreshWindows()
      showToast(action === 'claim' ? '已接管,点击画布聚焦后可打字' : '已释放')
      canvasRef.current?.focus()
    } catch (err) {
      showToast(`接管操作失败:${msg(err)}`, 'error')
    }
  }

  const canInput = win.mode === 'user'

  return (
    <div className="canvas-panel">
      <div className="canvas-head">
        <button className="btn btn-small" onClick={() => openWindow(null)}>
          返回列表
        </button>
        <span className="canvas-title" title={win.title}>
          {win.title}
        </span>
        <span className="canvas-meta">
          {info ? `${info.width}×${info.height} · ${info.codec} · ${info.fps}fps` : ''}
          {stats.frames > 0 ? ` · ${stats.frames} 帧 · 最近 ${fmtTime(stats.lastTs)}` : ''}
        </span>
        <span className={`mode-badge mode-${win.mode}`}>
          {win.mode === 'agent' ? 'agent 持有' : win.mode === 'user' ? '已接管' : '空闲'}
        </span>
        {win.mode === 'user' ? (
          <button className="btn btn-small" onClick={() => void takeover('release')}>
            释放
          </button>
        ) : (
          <>
            <button className="btn btn-small btn-primary" onClick={() => void takeover('claim')}>
              接管
            </button>
            {win.mode === 'agent' && (
              <button className="btn btn-small" onClick={() => void takeover('claim', true)}>
                强制接管
              </button>
            )}
          </>
        )}
      </div>
      {error && <div className="canvas-error">附流失败:{error}</div>}
      <div className="canvas-wrap" onContextMenu={(e) => canInput && e.preventDefault()}>
        <canvas
          ref={canvasRef}
          tabIndex={0}
          className={canInput ? 'input-active' : ''}
          onPointerMove={onPointerMove}
          onPointerDown={(e) => onPointerButton(e, 'press')}
          onPointerUp={(e) => onPointerButton(e, 'release')}
          onWheel={onWheel}
          onKeyDown={(e) => onKey(e, 'press')}
          onKeyUp={(e) => onKey(e, 'release')}
        />
        {!info && !error && <div className="canvas-hint">等待第一帧…</div>}
        {info && !canInput && (
          <div className="canvas-hint">观看模式。点「接管」获得输入控制权。</div>
        )}
      </div>
    </div>
  )
}

// 光标按协议来自帧头(cursorX/Y + flags bit1),不依赖本机指针推算。
function drawCursor(ctx: CanvasRenderingContext2D, f: VideoFrame) {
  const scale = Math.max(0.8, Math.min(2, (f.cursorW || 16) / 16))
  ctx.save()
  ctx.translate(f.cursorX, f.cursorY)
  ctx.scale(scale, scale)
  ctx.beginPath()
  ctx.moveTo(0, 0)
  ctx.lineTo(0, 14)
  ctx.lineTo(3.8, 10.6)
  ctx.lineTo(6.4, 16.4)
  ctx.lineTo(9, 15.2)
  ctx.lineTo(6.4, 9.6)
  ctx.lineTo(11, 9.6)
  ctx.closePath()
  ctx.fillStyle = '#1a1a18'
  ctx.strokeStyle = '#ffffff'
  ctx.lineWidth = 1.4
  ctx.stroke()
  ctx.fill()
  ctx.restore()
}
