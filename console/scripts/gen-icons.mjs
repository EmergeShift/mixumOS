// 生成 PWA 图标(192/512/apple-touch):零依赖,手工编码 PNG。
// 图形:深灰圆角底 + 浅色终端窗(标题条 + 命令提示符 ">_"),克制平实。
import { deflateSync } from 'node:zlib'
import { writeFileSync, mkdirSync } from 'node:fs'
import { dirname, join } from 'node:path'
import { fileURLToPath } from 'node:url'

const root = dirname(fileURLToPath(import.meta.url))
const outDir = join(root, '..', 'public', 'icons')
mkdirSync(outDir, { recursive: true })

// ---------- 极小 PNG 编码器 ----------
const CRC_TABLE = (() => {
  const t = new Uint32Array(256)
  for (let n = 0; n < 256; n++) {
    let c = n
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1
    t[n] = c >>> 0
  }
  return t
})()

function crc32(buf) {
  let c = 0xffffffff
  for (let i = 0; i < buf.length; i++) c = CRC_TABLE[(c ^ buf[i]) & 0xff] ^ (c >>> 8)
  return (c ^ 0xffffffff) >>> 0
}

function chunk(type, data) {
  const out = Buffer.alloc(12 + data.length)
  out.writeUInt32BE(data.length, 0)
  out.write(type, 4, 'ascii')
  data.copy(out, 8)
  out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length)
  return out
}

function encodePNG(width, height, rgba) {
  const sig = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a])
  const ihdr = Buffer.alloc(13)
  ihdr.writeUInt32BE(width, 0)
  ihdr.writeUInt32BE(height, 4)
  ihdr[8] = 8 // 位深
  ihdr[9] = 6 // RGBA
  const raw = Buffer.alloc((width * 4 + 1) * height)
  for (let y = 0; y < height; y++) {
    raw[y * (width * 4 + 1)] = 0 // filter: none
    rgba.copy(raw, y * (width * 4 + 1) + 1, y * width * 4, (y + 1) * width * 4)
  }
  return Buffer.concat([
    sig,
    chunk('IHDR', ihdr),
    chunk('IDAT', deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ])
}

// ---------- 光栅化(4x 超采样后盒滤波) ----------
function drawIcon(size) {
  const S = size * 4 // 超采样边长
  const px = new Float64Array(S * S * 3)

  const put = (x, y, r, g, b) => {
    if (x < 0 || y < 0 || x >= S || y >= S) return
    const i = (y * S + x) * 3
    px[i] = r
    px[i + 1] = g
    px[i + 2] = b
  }
  const fillRect = (x0, y0, x1, y1, r, g, b) => {
    for (let y = Math.round(y0); y < Math.round(y1); y++)
      for (let x = Math.round(x0); x < Math.round(x1); x++) put(x, y, r, g, b)
  }
  const fillRound = (x0, y0, x1, y1, rad, r, g, b) => {
    for (let y = Math.round(y0); y < Math.round(y1); y++) {
      for (let x = Math.round(x0); x < Math.round(x1); x++) {
        const cx = Math.max(x0 + rad - x, x - (x1 - rad), 0)
        const cy = Math.max(y0 + rad - y, y - (y1 - rad), 0)
        if (Math.hypot(cx, cy) <= rad) put(x, y, r, g, b)
      }
    }
  }
  const line = (x0, y0, x1, y1, w, r, g, b) => {
    const steps = Math.ceil(Math.hypot(x1 - x0, y1 - y0)) * 2
    for (let i = 0; i <= steps; i++) {
      const t = i / steps
      const cx = x0 + (x1 - x0) * t
      const cy = y0 + (y1 - y0) * t
      fillRect(cx - w / 2, cy - w / 2, cx + w / 2, cy + w / 2, r, g, b)
    }
  }

  const BG = [0x2b, 0x2b, 0x28]
  const WIN = [0xf4, 0xf4, 0xf2]
  const BAR = [0x3a, 0x6e, 0xa5] // 克制的钢蓝
  const FG = [0x2b, 0x2b, 0x28]

  fillRound(0, 0, S, S, S * 0.18, ...BG)
  // 终端窗体
  const wx0 = S * 0.16,
    wy0 = S * 0.2,
    wx1 = S * 0.84,
    wy1 = S * 0.8
  fillRound(wx0, wy0, wx1, wy1, S * 0.05, ...WIN)
  // 标题条
  fillRound(wx0, wy0, wx1, wy0 + S * 0.09, S * 0.05, ...BAR)
  fillRect(wx0, wy0 + S * 0.045, wx1, wy0 + S * 0.09, ...BAR)
  // 提示符 ">_"
  const bx = wx0 + S * 0.08,
    by = wy0 + S * 0.32,
    gw = S * 0.09,
    lw = S * 0.035
  line(bx, by, bx + gw * 0.8, by + gw * 0.75, lw, ...FG)
  line(bx + gw * 0.8, by + gw * 0.75, bx, by + gw * 1.5, lw, ...FG)
  fillRect(bx + gw * 1.5, by + gw * 1.35, bx + gw * 2.6, by + gw * 1.35 + lw, ...FG)

  // 盒滤波降采样
  const out = Buffer.alloc(size * size * 4)
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let r = 0,
        g = 0,
        b = 0
      for (let dy = 0; dy < 4; dy++) {
        for (let dx = 0; dx < 4; dx++) {
          const i = ((y * 4 + dy) * S + (x * 4 + dx)) * 3
          r += px[i]
          g += px[i + 1]
          b += px[i + 2]
        }
      }
      const o = (y * size + x) * 4
      out[o] = Math.round(r / 16)
      out[o + 1] = Math.round(g / 16)
      out[o + 2] = Math.round(b / 16)
      out[o + 3] = 255
    }
  }
  return encodePNG(size, size, out)
}

writeFileSync(join(outDir, 'icon-512.png'), drawIcon(512))
writeFileSync(join(outDir, 'icon-192.png'), drawIcon(192))
writeFileSync(join(outDir, 'apple-touch-icon.png'), drawIcon(180))
console.log('icons written to', outDir)
