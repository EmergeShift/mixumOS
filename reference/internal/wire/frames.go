package wire

import "encoding/binary"

// 二进制帧布局：首字节为通道标签，其后为通道数据。
// 文本帧一律是 Envelope JSON，二进制帧一律是流数据，二者互斥。
const (
	ChanVideo uint8 = 0x01 // 显示流：视频帧
	ChanAudio uint8 = 0x02 // 预留
	ChanChunk uint8 = 0x03 // 快照块传输
	ChanStdio uint8 = 0x04 // 任务 stdio 字节流
)

// video 二进制布局（小端）：
// [0]      chan = 0x01
// [1:5]    windowID u32
// [5:9]    width u32
// [9:13]   height u32
// [13:17]  size u32
// [17:19]  flags u16  (bit0: 关键帧, bit1: 携带光标)
// [19:21]  cursorX i16（相对窗口左上角，物理像素）
// [21:23]  cursorY i16
// [23:25]  cursorW u16（光标热区尺寸，用于主机端绘制）
// [25:27]  cursorH u16
// [27:]    编码帧数据
const videoHeaderSize = 27

// FrameFlags 标记一帧的属性。
type FrameFlags uint16

const (
	FrameKeyframe FrameFlags = 1 << 0
	FrameHasCursor FrameFlags = 1 << 1
)

// MarshalVideo 打包一帧视频。
func MarshalVideo(windowID uint32, w, h int, keyframe bool, curX, curY int, curW, curH int, hasCursor bool, data []byte) []byte {
	var flags FrameFlags
	if keyframe {
		flags |= FrameKeyframe
	}
	if hasCursor {
		flags |= FrameHasCursor
	}
	buf := make([]byte, videoHeaderSize+len(data))
	buf[0] = ChanVideo
	binary.LittleEndian.PutUint32(buf[1:5], windowID)
	binary.LittleEndian.PutUint32(buf[5:9], uint32(w))
	binary.LittleEndian.PutUint32(buf[9:13], uint32(h))
	binary.LittleEndian.PutUint32(buf[13:17], uint32(len(data)))
	binary.LittleEndian.PutUint16(buf[17:19], uint16(flags))
	binary.LittleEndian.PutUint16(buf[19:21], uint16(int16(curX)))
	binary.LittleEndian.PutUint16(buf[21:23], uint16(int16(curY)))
	binary.LittleEndian.PutUint16(buf[23:25], uint16(curW))
	binary.LittleEndian.PutUint16(buf[25:27], uint16(curH))
	copy(buf[videoHeaderSize:], data)
	return buf
}

// VideoFrame 是解包后的视频帧。
type VideoFrame struct {
	WindowID  uint32
	Width     int
	Height    int
	Keyframe  bool
	CursorX   int
	CursorY   int
	CursorW   int
	CursorH   int
	HasCursor bool
	Data      []byte
}

// UnmarshalVideo 解包一帧视频。
func UnmarshalVideo(buf []byte) (*VideoFrame, error) {
	if len(buf) < videoHeaderSize {
		return nil, ErrShortFrame
	}
	f := &VideoFrame{
		WindowID: binary.LittleEndian.Uint32(buf[1:5]),
		Width:    int(binary.LittleEndian.Uint32(buf[5:9])),
		Height:   int(binary.LittleEndian.Uint32(buf[9:13])),
	}
	size := binary.LittleEndian.Uint32(buf[13:17])
	flags := FrameFlags(binary.LittleEndian.Uint16(buf[17:19]))
	f.Keyframe = flags&FrameKeyframe != 0
	f.HasCursor = flags&FrameHasCursor != 0
	f.CursorX = int(int16(binary.LittleEndian.Uint16(buf[19:21])))
	f.CursorY = int(int16(binary.LittleEndian.Uint16(buf[21:23])))
	f.CursorW = int(binary.LittleEndian.Uint16(buf[23:25]))
	f.CursorH = int(binary.LittleEndian.Uint16(buf[25:27]))
	if int(size) > len(buf)-videoHeaderSize {
		return nil, ErrShortFrame
	}
	f.Data = buf[videoHeaderSize : videoHeaderSize+int(size)]
	return f, nil
}

// ErrShortFrame 表示二进制帧被截断。
var ErrShortFrame = frameError("binary frame truncated")

type frameError string

func (e frameError) Error() string { return string(e) }
