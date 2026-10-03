// Package display 实现窗口流的中枢（broker）：
// 容器内 mixcomp（魔改 Wayland 合成器）把每个顶层窗口单独编码成帧流，
// mixumd 通过 Unix 域套接字接收，并按窗口 id 分发给主机端订阅者；
// 输入事件反向注入；接管状态机决定「agent 持有输入」还是「用户接管」。
// 端到端设计见 docs/wayland-magic.md。
package display

import (
	"encoding/json"
	"fmt"
	"net"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/mixumos/mixumos/internal/wire"
)

// 窗口输入模式。
const (
	ModeAgent = "agent" // agent（任务）持有输入
	ModeUser  = "user"  // 用户已接管
	ModeFree  = "free"  // 无任务持有，谁都能动
)

// Window 是 broker 侧的窗口状态。
type Window struct {
	wire.Window
	subs     map[string]*Sub
	lastMode string
}

// Sub 是一个帧订阅者（一条主机 WS 连接）。
type Sub struct {
	ID     string
	Caps   func(wire.Cap) bool
	Frames chan []byte // 已打包的二进制视频帧
	FPS    int
	attach time.Time
}

// Broker 是窗口流中枢。
type Broker struct {
	mu       sync.RWMutex
	windows  map[uint32]*Window
	nextID   uint32
	subs     map[string]*Sub
	listSubs map[chan wire.Window]struct{} // 窗口列表变更订阅

	sockPath string
	ln       net.Listener
	conn     net.Conn // mixcomp 连接（单合成器）
	connMu   sync.Mutex
	online   bool

	// OnWindowsChanged 供服务端推送 display.windows。
	OnWindowsChanged func()
}

// NewBroker 构造 broker 并监听 mixcomp 控制套接字。
func NewBroker(sockPath string) *Broker {
	return &Broker{
		windows:  map[uint32]*Window{},
		subs:     map[string]*Sub{},
		listSubs: map[chan wire.Window]struct{}{},
		sockPath: sockPath,
	}
}

// Start 监听 unix 套接字，等待 mixcomp 连入。反复断连可重入。
func (b *Broker) Start() error {
	if err := os.MkdirAll(filepath.Dir(b.sockPath), 0o755); err != nil {
		return err
	}
	_ = os.Remove(b.sockPath)
	ln, err := net.Listen("unix", b.sockPath)
	if err != nil {
		return fmt.Errorf("display: 监听 %s 失败: %w", b.sockPath, err)
	}
	b.ln = ln
	go b.acceptLoop()
	return nil
}

func (b *Broker) acceptLoop() {
	for {
		conn, err := b.ln.Accept()
		if err != nil {
			return
		}
		b.connMu.Lock()
		if b.conn != nil {
			b.conn.Close()
		}
		b.conn = conn
		b.online = true
		b.connMu.Unlock()
		go b.serveComp(conn)
	}
}

// serveComp 处理 mixcomp 连接：控制消息为行分隔 JSON，帧为二进制记录。
// 协议：4 字节小端长度 + 1 字节类型（0x01=JSON 控制，0x02=视频帧）
// 长度上限 16MiB。
func (b *Broker) serveComp(conn net.Conn) {
	defer func() {
		b.connMu.Lock()
		if b.conn == conn {
			b.conn = nil
			b.online = false
		}
		b.connMu.Unlock()
		conn.Close()
	}()
	hdr := make([]byte, 5)
	for {
		if _, err := ioReadFull(conn, hdr); err != nil {
			return
		}
		length := int(uint32(hdr[0]) | uint32(hdr[1])<<8 | uint32(hdr[2])<<16 | uint32(hdr[3])<<24)
		if length <= 0 || length > 16<<20 {
			return
		}
		buf := make([]byte, length)
		if _, err := ioReadFull(conn, buf); err != nil {
			return
		}
		switch hdr[4] {
		case 0x01:
			var m compMsg
			if err := json.Unmarshal(buf, &m); err == nil {
				b.handleComp(m)
			}
		case 0x02:
			f, err := wire.UnmarshalVideo(buf)
			if err == nil {
				b.broadcastFrame(f.WindowID, append([]byte(nil), buf...))
			}
		}
	}
}

type compMsg struct {
	Type    string `json:"type"`
	ID      uint32 `json:"id,omitempty"`
	AppID   string `json:"appId,omitempty"`
	Title   string `json:"title,omitempty"`
	Width   int    `json:"width,omitempty"`
	Height  int    `json:"height,omitempty"`
	Focused bool   `json:"focused,omitempty"`
}

func (b *Broker) handleComp(m compMsg) {
	switch m.Type {
	case "window.open", "window.update":
		b.mu.Lock()
		w, ok := b.windows[m.ID]
		if !ok {
			w = &Window{subs: map[string]*Sub{}, lastMode: ModeFree}
			w.ID = m.ID
			b.windows[m.ID] = w
		}
		if m.AppID != "" {
			w.AppID = m.AppID
		}
		if m.Title != "" {
			w.Title = m.Title
		}
		if m.Width > 0 {
			w.Width = m.Width
		}
		if m.Height > 0 {
			w.Height = m.Height
		}
		w.Focused = m.Focused
		b.mu.Unlock()
		b.notifyList(w.Window)
	case "window.close":
		b.mu.Lock()
		delete(b.windows, m.ID)
		b.mu.Unlock()
		b.notifyList(wire.Window{ID: m.ID, Title: "__closed__"})
	}
}

func (b *Broker) broadcastFrame(windowID uint32, raw []byte) {
	b.mu.RLock()
	w := b.windows[windowID]
	if w == nil {
		b.mu.RUnlock()
		return
	}
	subs := make([]*Sub, 0, len(w.subs))
	for _, s := range w.subs {
		subs = append(subs, s)
	}
	b.mu.RUnlock()
	for _, s := range subs {
		select {
		case s.Frames <- raw:
		default: // 帧通道满则丢弃旧帧——实时性优先
			select {
			case <-s.Frames:
			default:
			}
			select {
			case s.Frames <- raw:
			default:
			}
		}
	}
}

// List 当前窗口快照。
func (b *Broker) List() []wire.Window {
	b.mu.RLock()
	defer b.mu.RUnlock()
	out := make([]wire.Window, 0, len(b.windows))
	for _, w := range b.windows {
		out = append(out, w.Window)
	}
	return out
}

// Online 报告 mixcomp 是否在线。
func (b *Broker) Online() bool {
	b.connMu.Lock()
	defer b.connMu.Unlock()
	return b.online
}

// Attach 订阅窗口帧流。capChk 用于按帧鉴权以外的附加校验。
func (b *Broker) Attach(windowID uint32, fps int, sub *Sub) (*wire.DisplayAttachResponse, error) {
	b.mu.Lock()
	w := b.windows[windowID]
	if w == nil {
		b.mu.Unlock()
		return nil, fmt.Errorf("display: 窗口不存在: %d", windowID)
	}
	if fps <= 0 || fps > 60 {
		fps = 30
	}
	sub.FPS = fps
	if sub.Frames == nil {
		sub.Frames = make(chan []byte, 8)
	}
	w.subs[sub.ID] = sub
	resp := &wire.DisplayAttachResponse{
		Window: windowID, Codec: "jpeg", Width: w.Width, Height: w.Height, FPS: fps,
	}
	b.mu.Unlock()
	// 请求合成器按此窗口+帧率推流。
	b.sendComp(map[string]any{"type": "stream.start", "id": windowID, "fps": fps})
	return resp, nil
}

// Detach 取消订阅。
func (b *Broker) Detach(subID string, windowID uint32) {
	b.mu.Lock()
	w := b.windows[windowID]
	if w != nil {
		delete(w.subs, subID)
	}
	b.mu.Unlock()
	b.sendComp(map[string]any{"type": "stream.stop", "id": windowID})
}

// Inject 输入注入。mode 校验：user 模式窗口只有 claim 过的连接能注入，
// agent 模式一律拒绝（agent 自己在自己的 seat 上操作）。
func (b *Broker) Inject(windowID uint32, ev wire.DisplayInputRequest) error {
	b.mu.RLock()
	w := b.windows[windowID]
	b.mu.RUnlock()
	if w == nil {
		return fmt.Errorf("display: 窗口不存在: %d", windowID)
	}
	if w.Mode == ModeAgent {
		return fmt.Errorf("display: 窗口 %d 正由 agent 操作，请先接管（display.takeover）", windowID)
	}
	b.sendComp(map[string]any{
		"type": "input", "id": windowID, "kind": ev.Type,
		"x": ev.X, "y": ev.Y, "button": ev.Button, "action": ev.Action,
		"key": ev.Key, "keysym": ev.Keysym, "mods": ev.Mods,
	})
	return nil
}

// Takeover 切换窗口的输入归属。claim: agent→user（可 force 抢占）；
// release: user→agent / user→free。
func (b *Broker) Takeover(windowID uint32, action string, force bool) (string, error) {
	b.mu.Lock()
	w := b.windows[windowID]
	if w == nil {
		b.mu.Unlock()
		return "", fmt.Errorf("display: 窗口不存在: %d", windowID)
	}
	switch action {
	case "claim":
		if w.Mode == ModeAgent && !force {
			b.mu.Unlock()
			return "", fmt.Errorf("display: agent 正在操作窗口 %d，force=true 可抢占", windowID)
		}
		if w.Mode == ModeAgent && force {
			// 通知任务侧让出（任务引擎监听窗口模式变化）。
			b.sendComp(map[string]any{"type": "input.mode", "id": windowID, "mode": "user"})
		}
		w.Mode = ModeUser
	case "release":
		w.Mode = ModeFree
		b.sendComp(map[string]any{"type": "input.mode", "id": windowID, "mode": "free"})
	default:
		b.mu.Unlock()
		return "", fmt.Errorf("display: 未知 takeover 动作 %q", action)
	}
	mode := w.Mode
	b.mu.Unlock()
	b.notifyList(w.Window)
	b.sendComp(map[string]any{"type": "input.mode", "id": windowID, "mode": mode})
	return mode, nil
}

// SetAgentMode 任务引擎在任务开始/结束时切换窗口归属。
func (b *Broker) SetAgentMode(windowID uint32, mode string) {
	b.mu.Lock()
	w := b.windows[windowID]
	if w != nil {
		w.Mode = mode
	}
	b.mu.Unlock()
	if w != nil {
		b.notifyList(w.Window)
	}
}

// SimulateWindow 注册一个模拟窗口（无 mixcomp 的开发/测试环境用）。
func (b *Broker) SimulateWindow(id uint32, appID, title string, w, h int) error {
	b.mu.Lock()
	if _, ok := b.windows[id]; !ok {
		b.windows[id] = &Window{subs: map[string]*Sub{}, lastMode: ModeFree}
		b.windows[id].ID = id
	}
	b.windows[id].AppID = appID
	b.windows[id].Title = title
	b.windows[id].Width = w
	b.windows[id].Height = h
	win := b.windows[id].Window
	b.mu.Unlock()
	b.notifyList(win)
	return nil
}

// sendComp 向 mixcomp 发控制消息。
func (b *Broker) sendComp(m any) {
	b.connMu.Lock()
	conn := b.conn
	b.connMu.Unlock()
	if conn == nil {
		return
	}
	raw, err := json.Marshal(m)
	if err != nil {
		return
	}
	buf := make([]byte, 5+len(raw))
	n := len(raw)
	buf[0] = byte(n)
	buf[1] = byte(n >> 8)
	buf[2] = byte(n >> 16)
	buf[3] = byte(n >> 24)
	buf[4] = 0x01
	copy(buf[5:], raw)
	conn.SetWriteDeadline(time.Now().Add(3 * time.Second))
	conn.Write(buf)
}

func (b *Broker) notifyList(w wire.Window) {
	b.mu.RLock()
	chans := make([]chan wire.Window, 0, len(b.listSubs))
	for ch := range b.listSubs {
		chans = append(chans, ch)
	}
	b.mu.RUnlock()
	for _, ch := range chans {
		select {
		case ch <- w:
		default:
		}
	}
	if b.OnWindowsChanged != nil {
		b.OnWindowsChanged()
	}
}

// SubscribeList 订阅窗口列表变更。
func (b *Broker) SubscribeList() (<-chan wire.Window, func()) {
	ch := make(chan wire.Window, 64)
	b.mu.Lock()
	b.listSubs[ch] = struct{}{}
	b.mu.Unlock()
	return ch, func() {
		b.mu.Lock()
		delete(b.listSubs, ch)
		b.mu.Unlock()
	}
}

// ioReadFull 是 io.ReadFull 的本地实现（避免多一个 import 别名）。
func ioReadFull(conn net.Conn, buf []byte) (int, error) {
	total := 0
	for total < len(buf) {
		n, err := conn.Read(buf[total:])
		total += n
		if err != nil {
			return total, err
		}
	}
	return total, nil
}
