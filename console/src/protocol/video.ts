// video 帧头解析(协议 §5,小端,27 字节)。
// [0]=0x01 [1:5]windowID [5:9]width [9:13]height [13:17]size
// [17:19]flags(bit0 关键帧/IDR,bit1 携带光标) [19:21]cursorX [21:23]cursorY
// [23:25]cursorW [25:27]cursorH [27:]编码数据(JPEG 或 H.264 Annex-B,由
// attach 协商决定;codec 由 client 按订阅信息标注)
import type { VideoFrame, VideoFrameHeader } from './types'

export const VIDEO_HEADER_LEN = 27
export const CHAN_VIDEO = 0x01

export function parseVideoFrame(data: Uint8Array): VideoFrame | null {
  if (data.length < VIDEO_HEADER_LEN || data[0] !== CHAN_VIDEO) return null
  const dv = new DataView(data.buffer, data.byteOffset, data.byteLength)
  const header: VideoFrameHeader = {
    windowId: dv.getUint32(1, true),
    width: dv.getUint32(5, true),
    height: dv.getUint32(9, true),
    size: dv.getUint32(13, true),
    keyframe: (dv.getUint16(17, true) & 1) !== 0,
    hasCursor: (dv.getUint16(17, true) & 2) !== 0,
    cursorX: dv.getInt16(19, true),
    cursorY: dv.getInt16(21, true),
    cursorW: dv.getUint16(23, true),
    cursorH: dv.getUint16(25, true),
  }
  if (VIDEO_HEADER_LEN + header.size > data.length) return null
  return {
    ...header,
    payload: data.subarray(VIDEO_HEADER_LEN, VIDEO_HEADER_LEN + header.size),
    // client 按 attach 协商覆盖实际编码
    codec: 'jpeg',
    ts: Date.now(),
  }
}
