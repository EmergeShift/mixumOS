// 窗口画布:display.attach 收 JPEG 帧(canvas 绘制,静默丢帧只画最新),
// 光标来自帧头元数据;接管后鼠标/键盘事件换算物理像素回传 display.input。
import { useEffect, useRef, useState } from 'react'
import { client, msg, useStore } from '../state/store'
import type { DisplayAttachResponse, VideoFrame as ProtocolVideoFrame, WindowInfo } from '../protocol/types'
import { toKeysym, modsOf } from '../util/keysym'
import { fmtTime } from '../util/format'

const ATTACH_FPS = 15

// WebCodecs 能力探测(模块级缓存):openh264 输出 Annex-B,无 description,
// 探测从基线 profile 开始(openh264 实际输出基线档),返回命中的 codec 串。
let h264SupportPromise: Promise<string | null> | null = null
function probeH264(): Promise<string | null> {
  if (h264SupportPromise) return h264SupportPromise
  h264SupportPromise = (async () => {
    if (typeof VideoDecoder === 'undefined') return null
    const candidates = ['avc1.42e01e', 'avc1.4d0028', 'avc1.640028']
    for (const codec of candidates) {
      try {
        const r = await VideoDecoder.isConfigSupported({ codec })
        if (r.supported) return codec
      } catch {
        /* 探测失败继续 */
      }
    }
    return null
  })()
  return h264SupportPromise
}

// H.264 解码封装:Annex-B 喂入,输出 VideoFrame 回调。
class H264Decoder {
  private decoder: VideoDecoder | null = null
  private seq = 0
  constructor(
    private codec: string,
    private onFrame: (vf: VideoFrame) => void,
    private onError: (e: string) => void,
  ) {}

  handle(frame: { payload: Uint8Array; keyframe: boolean }): void {
    let dec = this.decoder
    if (!dec) {
      const self = this
      dec = new VideoDecoder({
        output: (vf) => self.onFrame(vf),
        error: (e) => self.onError(e.message),
      })
      dec.configure({ codec: this.codec, optimizeForLatency: true })
      this.decoder = dec
      this.seq = 0
    }
    if (frame.keyframe) this.seq = 0
    else if (this.seq === 0) return // 等首个 IDR
    this.seq += 1
    try {
      dec.decode(
        new EncodedVideoChunk({
          type: frame.keyframe ? 'key' : 'delta',
          timestamp: this.seq * (1_000_000 / 30),
          // WebSocket 二进制来源实际是 ArrayBuffer 背书,零拷贝直用
          data: frame.payload as unknown as BufferSource,
        }),
      )
    } catch (e) {
      this.onError(String((e as Error)?.message ?? e))
    }
  }

  close(): void {
    try {
      this.decoder?.close()
    } catch {
      /* 已关闭 */
    }
    this.decoder = null
  }
}

export function WindowCanvas({ win }: { win: WindowInfo }) {
  const showToast = useStore((s) => s.showToast)
  const refreshWindows = useStore((s) => s.refreshWindows)
  const openWindow = useStore((s) => s.openWindow)

  const canvasRef = useRef<HTMLCanvasElement>(null)
  const imeRef = useRef<HTMLTextAreaElement>(null)
  const latestRef = useRef<ProtocolVideoFrame | null>(null)
  const decodedRef = useRef<VideoFrame | null>(null)
  const decoderRef = useRef<H264Decoder | null>(null)
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
  const [forceJpeg, setForceJpeg] = useState(false)
  const [frozen, setFrozen] = useState(false)
  const frozenRef = useRef(false)
  const [dragging, setDragging] = useState(false)
  const [audioOn, setAudioOn] = useState(false)
  const [volume, setVolume] = useState(0.8)
  const [audioStats, setAudioStats] = useState({ frames: 0, drops: 0, lead: 0 })
  const audioRef = useRef<{
    ctx: AudioContext
    gain: GainNode
    next: number
    frames: number
    lastSeq: number
    drops: number
  } | null>(null)
  const volumeRef = useRef(volume)
  volumeRef.current = volume

  modeRef.current = win.mode

  // 附流 + 帧订阅(codec:服务端窗口回告优先,否则 WebCodecs 探测;
  // 解码失败整体降级重连为 jpeg)
  useEffect(() => {
    let dead = false
    setError('')
    const want: 'jpeg' | 'h264' = forceJpeg
      ? 'jpeg'
      : (win.codec ?? null) === 'jpeg'
        ? 'jpeg'
        : 'h264'
    const attachFps = ATTACH_FPS
    void (async () => {
      let codec: 'jpeg' | 'h264' = 'jpeg'
      let h264Codec: string | null = null
      if (want === 'h264' && !forceJpeg && win.codec !== 'jpeg') {
        h264Codec = await probeH264()
        codec = h264Codec ? 'h264' : 'jpeg'
      }
      if (dead) return
      try {
        const r = await client.attach(win.id, attachFps, codec)
        if (dead) return
        setInfo(r)
        if ((r.codec as 'jpeg' | 'h264') === 'h264') {
          const self: { decoder: H264Decoder | null } = { decoder: null }
          self.decoder = new H264Decoder(
            h264Codec ?? 'avc1.42e01e',
            (vf) => {
              decodedRef.current = vf
              scheduleDraw()
            },
            () => setForceJpeg(true),
          )
          decoderRef.current = self.decoder
        }
        client.registerAttached(win.id, attachFps, r.codec as 'jpeg' | 'h264')
      } catch (err) {
        if (!dead) setError(msg(err))
      }
    })()

    const prevHandler = client.onFrame
    client.onFrame = (f) => {
      if (f.windowId !== win.id) return
      // 快照冻结状态帧：不喂解码器，保留画布最后一帧，只切 UI 状态。
      if (f.frozen) {
        if (!frozenRef.current) {
          frozenRef.current = true
          setFrozen(true)
        }
        return
      }
      if (frozenRef.current) {
        frozenRef.current = false
        setFrozen(false)
      }
      if (f.codec === 'h264') {
        decoderRef.current?.handle(f)
        latestRef.current = f // 仅取帧头元数据(光标/尺寸),像素走解码输出
      } else {
        latestRef.current = f
      }
      statsRef.current.frames += 1
      statsRef.current.lastTs = f.ts
      scheduleDraw()
    }

    return () => {
      dead = true
      client.onFrame = prevHandler
      decoderRef.current?.close()
      decoderRef.current = null
      void client.detach(win.id)
    }
  }, [win.id, win.codec, forceJpeg])

  // 统计低频刷新
  useEffect(() => {
    const t = setInterval(() => setStats({ ...statsRef.current }), 500)
    return () => clearInterval(t)
  }, [])

  // 音频:audio.attach 后按 20ms 块播放 s16le;落后时重置调度点。
  useEffect(() => {
    const prev = client.onAudio
    client.onAudio = (chunk) => {
      const a = audioRef.current
      if (!a) return
      const frames = chunk.pcm.length / chunk.channels
      const buf = a.ctx.createBuffer(chunk.channels, frames, chunk.sampleRate)
      for (let ch = 0; ch < chunk.channels; ch++) {
        const out = buf.getChannelData(ch)
        for (let i = 0; i < frames; i++) {
          out[i] = chunk.pcm[i * chunk.channels + ch] / 32768
        }
      }
      if (a.lastSeq >= 0 && chunk.seq !== a.lastSeq + 1) a.drops += 1
      a.lastSeq = chunk.seq
      const src = a.ctx.createBufferSource()
      src.buffer = buf
      src.connect(a.gain)
      const now = a.ctx.currentTime
      if (a.next < now + 0.02 || a.next > now + 0.5) a.next = now + 0.05
      src.start(a.next)
      a.next += buf.duration
      a.frames += 1
      if (a.frames % 25 === 0) {
        setAudioStats({
          frames: a.frames,
          drops: a.drops,
          lead: Math.round((a.next - a.ctx.currentTime) * 1000),
        })
      }
    }
    return () => {
      client.onAudio = prev
    }
  }, [])

  useEffect(() => {
    return () => {
      const a = audioRef.current
      audioRef.current = null
      if (a) {
        void a.ctx.close()
        void client.audioDetach()
      }
    }
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
    const canvas = canvasRef.current
    if (!canvas) return
    const f = latestRef.current
    latestRef.current = null // 取走即丢:绘制期间来的新帧下一轮画
    if (!f) return
    let vf: VideoFrame | null = null
    if (f.codec === 'h264') {
      vf = decodedRef.current
      decodedRef.current = null
      if (!vf) return // 解码尚未产出,丢这一拍
    }
    physRef.current = { w: f.width, h: f.height }
    if (canvas.width !== f.width || canvas.height !== f.height) {
      canvas.width = f.width
      canvas.height = f.height
    }
    const seq = ++drawSeqRef.current
    try {
      const ctx = canvas.getContext('2d')
      if (!ctx) return
      if (vf) {
        if (seq !== drawSeqRef.current) {
          vf.close() // 已有更新帧,静默丢弃这帧
          return
        }
        ctx.drawImage(vf, 0, 0)
        vf.close()
      } else {
        const bmp = await createImageBitmap(
          new Blob([f.payload as BlobPart], { type: 'image/jpeg' }),
        )
        if (seq !== drawSeqRef.current) {
          bmp.close() // 已有更新帧,静默丢弃这帧
          return
        }
        ctx.drawImage(bmp, 0, 0)
        bmp.close()
      }
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
    // 把隐藏输入框放到点击处:系统输入法候选窗跟随指针,组合结果
    // 通过 compositionend/input 以 text 事件发给远端。
    const ime = imeRef.current
    const canvas = canvasRef.current
    if (action === 'press' && ime && canvas) {
      const r = canvas.getBoundingClientRect()
      ime.style.left = `${e.clientX - r.left}px`
      ime.style.top = `${e.clientY - r.top}px`
      ime.focus({ preventScroll: true })
    }
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
    // 输入法组合中:按键交给 IME,不发给远端
    if (e.nativeEvent.isComposing || e.key === 'Process') return
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

  function sendText(text: string) {
    if (!text || modeRef.current !== 'user') return
    void client.input({ window: win.id, type: 'text', text }).catch(() => {})
  }

  function onImeCompositionEnd(e: React.CompositionEvent<HTMLTextAreaElement>) {
    const text = e.data
    e.currentTarget.value = ''
    sendText(text)
  }

  function onImeInput(e: React.FormEvent<HTMLTextAreaElement>) {
    const el = e.currentTarget
    const text = el.value
    el.value = ''
    const composing = (e.nativeEvent as InputEvent).isComposing
    // ASCII 单字符已由 keydown 以按键形式发送,这里只补非 ASCII/多字符
    // (输入法提交、粘贴、语音输入等)。
    if (composing || !text) return
    if (text.length === 1 && text.charCodeAt(0) < 0x7f) return
    sendText(text)
  }

  function onDrop(e: React.DragEvent) {
    e.preventDefault()
    setDragging(false)
    const file = e.dataTransfer.files?.[0]
    if (!file) return
    showToast(`上传 ${file.name}…`)
    void client
      .putFile(file)
      .then((r) => showToast(`已上传到 ${r.path}`))
      .catch((err) => showToast(`上传失败: ${msg(err)}`, 'error'))
  }

  async function toggleAudio() {
    if (audioOn) {
      setAudioOn(false)
      setAudioStats({ frames: 0, drops: 0, lead: 0 })
      const a = audioRef.current
      audioRef.current = null
      await client.audioDetach()
      void a?.ctx.close()
      return
    }
    try {
      const fmt = await client.audioAttach()
      const ctx = new AudioContext({ sampleRate: fmt.sampleRate })
      await ctx.resume()
      const gain = ctx.createGain()
      gain.gain.value = volumeRef.current
      gain.connect(ctx.destination)
      audioRef.current = { ctx, gain, next: 0, frames: 0, lastSeq: -1, drops: 0 }
      setAudioOn(true)
      setAudioStats({ frames: 0, drops: 0, lead: 0 })
      showToast(`音频已开启 ${fmt.sampleRate}Hz / ${fmt.channels}ch`)
    } catch (err) {
      showToast(`音频开启失败: ${msg(err)}`, 'error')
    }
  }

  async function takeover(action: 'claim' | 'release', force = false) {
    try {
      if (force && !window.confirm('agent 正持有输入,强制接管会让 agent 失去输入。继续?')) return
      await client.takeover(win.id, action, force)
      await refreshWindows()
      showToast(action === 'claim' ? '已接管,点击画布聚焦后可打字' : '已释放')
      if (action === 'claim') imeRef.current?.focus()
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
          {audioOn
            ? ` · 音频 ${audioStats.frames} · ${audioStats.lead}ms${audioStats.drops ? ` · 丢 ${audioStats.drops}` : ''}`
            : ''}
        </span>
        <span className={`mode-badge mode-${win.mode}`}>
          {win.mode === 'agent' ? 'agent 持有' : win.mode === 'user' ? '已接管' : '空闲'}
        </span>
        {frozen && <span className="mode-badge mode-frozen">快照冻结</span>}
        <button className="btn btn-small" onClick={() => void toggleAudio()}>
          {audioOn ? '静音' : '音频'}
        </button>
        {audioOn && (
          <input
            className="audio-vol"
            type="range"
            min={0}
            max={100}
            value={Math.round(volume * 100)}
            title="音量"
            onChange={(e) => {
              const v = Number(e.target.value) / 100
              setVolume(v)
              if (audioRef.current) audioRef.current.gain.gain.value = v
            }}
          />
        )}
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
      <div
        className="canvas-wrap"
        onContextMenu={(e) => canInput && e.preventDefault()}
        onKeyDown={(e) => onKey(e, 'press')}
        onKeyUp={(e) => onKey(e, 'release')}
        onDragOver={(e) => {
          e.preventDefault()
          if (!dragging) setDragging(true)
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={onDrop}
      >
        <canvas
          ref={canvasRef}
          tabIndex={0}
          className={canInput ? 'input-active' : ''}
          onPointerMove={onPointerMove}
          onPointerDown={(e) => onPointerButton(e, 'press')}
          onPointerUp={(e) => onPointerButton(e, 'release')}
          onWheel={onWheel}
        />
        <textarea
          ref={imeRef}
          className="ime-input"
          aria-label="输入法输入"
          tabIndex={-1}
          autoComplete="off"
          autoCorrect="off"
          spellCheck={false}
          onCompositionEnd={onImeCompositionEnd}
          onInput={onImeInput}
          onPaste={(e) => e.preventDefault()}
        />
        {!info && !error && <div className="canvas-hint">等待第一帧…</div>}
        {info && !canInput && (
          <div className="canvas-hint">观看模式。点「接管」获得输入控制权。</div>
        )}
        {dragging && <div className="canvas-drop">松开上传到容器 transfers/</div>}
      </div>
    </div>
  )
}

// 光标按协议来自帧头(cursorX/Y + flags bit1),不依赖本机指针推算。
function drawCursor(ctx: CanvasRenderingContext2D, f: ProtocolVideoFrame) {
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
