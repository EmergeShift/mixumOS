// Package wire 定义 mixumOS 主机与容器之间的 WebSocket 消息格式。
// 所有控制消息为 JSON 文本帧；流数据（视频帧、快照块）为带 1 字节通道标签的二进制帧。
// 协议规范见 docs/protocol.md。
package wire

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"time"
)

// ProtoVersion 是协议主版本。不兼容变更必须递增。
const ProtoVersion = 1

// Envelope 是所有控制消息的统一外壳。
type Envelope struct {
	V       int             `json:"v"`                 // 协议版本
	ID      string          `json:"id"`                // 消息唯一 id（uuidv7）
	Op      string          `json:"op"`                // 操作名，如 "task.submit"
	Seq     int64           `json:"seq,omitempty"`     // 连接内单调递增序号
	ReplyTo string          `json:"replyTo,omitempty"` // 回复对应的消息 id
	TS      int64           `json:"ts"`                // 发送时刻 unix 毫秒
	Payload json.RawMessage `json:"payload,omitempty"`
}

// NewEnvelope 构造一条外发消息，自动填充版本、id 与时间戳。
func NewEnvelope(op string, payload any) (*Envelope, error) {
	e := &Envelope{
		V:  ProtoVersion,
		ID: NewID(),
		Op: op,
		TS: time.Now().UnixMilli(),
	}
	if payload != nil {
		b, err := json.Marshal(payload)
		if err != nil {
			return nil, err
		}
		e.Payload = b
	}
	return e, nil
}

// Decode 解析并校验一条入站消息。
func Decode(data []byte) (*Envelope, error) {
	var e Envelope
	if err := json.Unmarshal(data, &e); err != nil {
		return nil, fmt.Errorf("wire: 消息不是合法 JSON: %w", err)
	}
	if e.V != ProtoVersion {
		return nil, fmt.Errorf("wire: 协议版本不支持: got %d want %d", e.V, ProtoVersion)
	}
	if e.ID == "" || e.Op == "" {
		return nil, fmt.Errorf("wire: 缺少 id 或 op")
	}
	return &e, nil
}

// ParsePayload 将负载反序列化到 out。
func (e *Envelope) ParsePayload(out any) error {
	if len(e.Payload) == 0 {
		return nil
	}
	return json.Unmarshal(e.Payload, out)
}

// Reply 以 ok 形式回复 e，负载为 resp。
func (e *Envelope) Reply(op string, resp any) (*Envelope, error) {
	r, err := NewEnvelope(op, resp)
	if err != nil {
		return nil, err
	}
	r.ReplyTo = e.ID
	return r, nil
}

// ErrorReply 回复一条带错误码的响应。
func (e *Envelope) ErrorReply(code ErrCode, msg string) (*Envelope, error) {
	return e.Reply(e.Op+".err", ErrorPayload{Code: code, Message: msg})
}

// ErrorPayload 是 *.err 消息的负载。
type ErrorPayload struct {
	Code    ErrCode `json:"code"`
	Message string  `json:"message"`
}

// NewID 生成 uuidv7 形式的 id（时间戳前缀 + 随机），便于排序与去重。
func NewID() string {
	var b [16]byte
	ts := uint64(time.Now().UnixMilli())
	b[0] = byte(ts >> 40)
	b[1] = byte(ts >> 32)
	b[2] = byte(ts >> 24)
	b[3] = byte(ts >> 16)
	b[4] = byte(ts >> 8)
	b[5] = byte(ts)
	if _, err := rand.Read(b[6:]); err != nil {
		panic("wire: 系统熵源不可用: " + err.Error())
	}
	b[6] = (b[6] & 0x0f) | 0x70 // version 7
	b[8] = (b[8] & 0x3f) | 0x80 // RFC 4122 variant
	return formatHex(b)
}

func formatHex(b [16]byte) string {
	h := hex.EncodeToString(b[:])
	return h[0:8] + "-" + h[8:12] + "-" + h[12:16] + "-" + h[16:20] + "-" + h[20:32]
}
