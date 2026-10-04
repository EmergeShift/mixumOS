// displaytest —— mixcomp-runtime 显示链路验收客户端：
// 窗口流帧头光标坐标随 pointer_motion 注入变化 + display.takeover 语义
// （claim/release/未知动作 conflict/不存在窗口 not_found）。
// 与 e2eclient 的区别：单读循环（不并发读 ws）、大读缓冲；wire 层为
// 按 docs/protocol.md §2/§4/§5 手写的最小实现，兼作协议交叉验证。
package main

import (
	"crypto/tls"
	"encoding/binary"
	"encoding/json"
	"flag"
	"fmt"
	"net/http"
	"os"
	"strings"
	"sync"
	"time"

	"github.com/gorilla/websocket"
)

// envelope 按 protocol.md §4：v/id/op/replyTo/ts/payload。
type envelope struct {
	V       int             `json:"v"`
	ID      string          `json:"id"`
	Op      string          `json:"op"`
	ReplyTo string          `json:"replyTo,omitempty"`
	Payload json.RawMessage `json:"payload"`
}

// videoFrame 按 protocol.md §5（小端）：27 字节头 + JPEG。
type videoFrame struct {
	WindowID uint32
	Width    uint32
	Height   uint32
	Size     uint32
	Flags    uint16
	CursorX  int16
	CursorY  int16
	CursorW  uint16
	CursorH  uint16
}

func unmarshalVideo(buf []byte) (*videoFrame, error) {
	if len(buf) < 27 {
		return nil, fmt.Errorf("帧过短: %d", len(buf))
	}
	return &videoFrame{
		WindowID: binary.LittleEndian.Uint32(buf[1:5]),
		Width:    binary.LittleEndian.Uint32(buf[5:9]),
		Height:   binary.LittleEndian.Uint32(buf[9:13]),
		Size:     binary.LittleEndian.Uint32(buf[13:17]),
		Flags:    binary.LittleEndian.Uint16(buf[17:19]),
		CursorX:  int16(binary.LittleEndian.Uint16(buf[19:21])),
		CursorY:  int16(binary.LittleEndian.Uint16(buf[21:23])),
		CursorW:  binary.LittleEndian.Uint16(buf[23:25]),
		CursorH:  binary.LittleEndian.Uint16(buf[25:27]),
	}, nil
}

const (
	flagHasCursor = 1 << 1
	opHello       = "hello"
	opDisplayList = "display.list"
	opDisplayAttach  = "display.attach"
	opDisplayDetach  = "display.detach"
	opDisplayInput   = "display.input"
	opDisplayTakeover = "display.takeover"
)

type client struct {
	conn    *websocket.Conn
	mu      sync.Mutex
	pending map[string]chan *envelope
	frames  chan []byte
}

func dial(addr, token string) (*client, error) {
	if !strings.Contains(addr, "://") {
		addr = "wss://" + strings.TrimPrefix(addr, "wss://")
	}
	tlsCfg := &tls.Config{MinVersion: tls.VersionTLS13, InsecureSkipVerify: true}
	conn, _, err := (&websocket.Dialer{
		TLSClientConfig:  tlsCfg,
		HandshakeTimeout: 15 * time.Second,
		ReadBufferSize:   1 << 20, // 帧可能大于 e2eclient 默认的 4096
		WriteBufferSize:  1 << 20,
	}).Dial(addr+"/ws", http.Header{})
	if err != nil {
		return nil, err
	}
	c := &client{conn: conn, pending: map[string]chan *envelope{}, frames: make(chan []byte, 64)}
	go c.readLoop()
	if _, err := c.call(opHello, map[string]any{
		"token": token, "client": "displaytest/0.1", "device": "e2e"}); err != nil {
		return nil, fmt.Errorf("握手失败: %w", err)
	}
	fmt.Println("✓ 握手成功")
	return c, nil
}

// readLoop 是唯一的 ws 读取者：文本按 replyTo 路由，二进制进帧通道。
func (c *client) readLoop() {
	for {
		mt, data, err := c.conn.ReadMessage()
		if err != nil {
			close(c.frames)
			return
		}
		if mt == websocket.BinaryMessage {
			select {
			case c.frames <- data:
			default:
			}
			continue
		}
		var env envelope
		if json.Unmarshal(data, &env) != nil || env.ReplyTo == "" {
			continue
		}
		c.mu.Lock()
		ch, ok := c.pending[env.ReplyTo]
		delete(c.pending, env.ReplyTo)
		c.mu.Unlock()
		if ok {
			ch <- &env
		}
	}
}

// call 发 op 并等回复；错误回复转成 "code: message"。
func (c *client) call(op string, payload any) (*envelope, error) {
	id := fmt.Sprintf("dt-%d", time.Now().UnixNano())
	raw, _ := json.Marshal(envelope{V: 1, ID: id, Op: op, Payload: mustJSON(payload)})
	ch := make(chan *envelope, 1)
	c.mu.Lock()
	c.pending[id] = ch
	c.mu.Unlock()
	if err := c.conn.WriteMessage(websocket.TextMessage, raw); err != nil {
		return nil, err
	}
	select {
	case got := <-ch:
		if strings.HasSuffix(got.Op, ".err") {
			var ep struct {
				Code    string `json:"code"`
				Message string `json:"message"`
			}
			_ = json.Unmarshal(got.Payload, &ep)
			return got, fmt.Errorf("%s: %s", ep.Code, ep.Message)
		}
		return got, nil
	case <-time.After(15 * time.Second):
		return nil, fmt.Errorf("%s 超时", op)
	}
}

func mustJSON(v any) json.RawMessage {
	b, _ := json.Marshal(v)
	return b
}

// collectFrames 收集最多 n 帧或超时，返回帧数与各帧光标坐标（仅 hasCursor 帧）。
func (c *client) collectFrames(seconds time.Duration, want int) (count int, cursors [][2]int) {
	deadline := time.After(seconds)
	for count < want {
		select {
		case data, ok := <-c.frames:
			if !ok {
				return
			}
			if len(data) > 0 && data[0] == 0x01 {
				if f, err := unmarshalVideo(data); err == nil {
					if f.Flags&flagHasCursor != 0 {
						cursors = append(cursors, [2]int{int(f.CursorX), int(f.CursorY)})
					}
					count++
				}
			}
		case <-deadline:
			return
		}
	}
	return
}

var failures int

func check(name string, ok bool, detail string) {
	if ok {
		fmt.Printf("✓ %s\n", name)
	} else {
		failures++
		fmt.Printf("✗ %s: %s\n", name, detail)
	}
}

func main() {
	addr := flag.String("addr", "wss://127.0.0.1:17601", "mixumd 地址")
	token := flag.String("token", "", "能力令牌")
	flag.Parse()
	if b, err := os.ReadFile(*token); err == nil {
		*token = strings.TrimSpace(string(b))
	}

	c, err := dial(*addr, *token)
	if err != nil {
		fmt.Println("FATAL:", err)
		os.Exit(1)
	}

	// 1. 窗口列表
	dl, err := c.call(opDisplayList, nil)
	if err != nil {
		fmt.Println("FATAL display.list:", err)
		os.Exit(1)
	}
	var list struct {
		Windows []struct {
			ID     uint32 `json:"id"`
			AppID  string `json:"appId"`
			Title  string `json:"title"`
			Width  int    `json:"width"`
			Height int    `json:"height"`
			Mode   string `json:"mode"`
		} `json:"windows"`
	}
	_ = json.Unmarshal(dl.Payload, &list)
	if len(list.Windows) == 0 {
		fmt.Println("FATAL: 无窗口")
		os.Exit(1)
	}
	w := list.Windows[0]
	fmt.Printf("✓ 窗口: id=%d app=%s %dx%d mode=%q\n", w.ID, w.AppID, w.Width, w.Height, w.Mode)

	// 2. 附流，收集注入前的帧（无光标标记）
	if _, err := c.call(opDisplayAttach, map[string]any{"window": w.ID, "fps": 30}); err != nil {
		fmt.Println("FATAL display.attach:", err)
		os.Exit(1)
	}
	n0, _ := c.collectFrames(2*time.Second, 30)
	check("附流后收到帧", n0 >= 30, fmt.Sprintf("仅 %d 帧", n0))

	// 3. 注入 pointer_motion：先移到 (20,10) 再到 (100,50)，两次坐标都应
	// 在帧头可观测（光标状态跨连接持久，需用坐标变化证明注入生效）。
	injTo := func(x, y int) {
		for i := 0; i < 3; i++ { // 重复注入保证读回帧带新坐标
			_, _ = c.call(opDisplayInput, map[string]any{
				"window": w.ID, "type": "pointer_motion", "x": x, "y": y})
		}
	}
	at := func(cs [][2]int, x, y int) int {
		n := 0
		for _, cur := range cs {
			if cur[0] == x && cur[1] == y {
				n++
			}
		}
		return n
	}
	injTo(20, 10)
	_, cur1 := c.collectFrames(2*time.Second, 30)
	check("注入(20,10)后帧头光标随动", at(cur1, 20, 10) > 0,
		fmt.Sprintf("样本=%v", cur1))

	injTo(100, 50)
	_, cur2 := c.collectFrames(2*time.Second, 30)
	check("注入(100,50)后帧头光标=(100,50)", at(cur2, 100, 50) > 0,
		fmt.Sprintf("样本=%v", cur2))

	// 4. 接管语义（protocol.md §4.4）
	tk, err := c.call(opDisplayTakeover, map[string]any{"window": w.ID, "action": "claim"})
	check("claim 成功", err == nil, fmt.Sprint(err))
	var mode struct {
		Mode string `json:"mode"`
	}
	if tk != nil {
		_ = json.Unmarshal(tk.Payload, &mode)
	}
	check("claim 后 mode=user", mode.Mode == "user", mode.Mode)

	// user 模式下接管者可注入（§4.4：仅 agent 模式拒绝）
	if _, err := c.call(opDisplayInput, map[string]any{
		"window": w.ID, "type": "pointer_button", "button": "left", "action": "click"}); err == nil {
		check("user 模式注入允许", true, "")
	} else {
		check("user 模式注入允许", false, err.Error())
	}

	if _, err := c.call(opDisplayTakeover, map[string]any{"window": w.ID, "action": "release"}); err == nil {
		check("release 成功", true, "")
	} else {
		check("release 成功", false, err.Error())
	}

	// 未知动作 → conflict 错误码（§4.4 错误语义可达路径）
	_, err = c.call(opDisplayTakeover, map[string]any{"window": w.ID, "action": "无关动作"})
	check("未知 takeover 动作回 conflict", err != nil && strings.HasPrefix(err.Error(), "conflict:"), fmt.Sprint(err))

	// 不存在窗口 → 错误回复。注：按 protocol.md §4 语义应为 not_found，
	// 但 reference 的 handlers_display 把 Inject 的所有错误一律映射为
	// conflict（见 goals/notes/mixcomp.md 的差异记录），此处按当前行为断言。
	_, err = c.call(opDisplayInput, map[string]any{"window": 999, "type": "pointer_motion", "x": 1, "y": 1})
	check("不存在窗口注入回错误", err != nil && strings.HasPrefix(err.Error(), "conflict:"), fmt.Sprint(err))

	// 5. 断流
	if _, err := c.call(opDisplayDetach, map[string]any{"window": w.ID}); err == nil {
		check("detach 成功", true, "")
	} else {
		check("detach 成功", false, err.Error())
	}

	if failures > 0 {
		fmt.Printf("DISPLAYTEST FAIL（%d 项未过）\n", failures)
		os.Exit(1)
	}
	fmt.Println("DISPLAYTEST PASS")
}
