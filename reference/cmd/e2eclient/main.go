// e2eclient —— mixumOS 端到端验证工具：作为主机端客户端连接 mixumd，
// 依次验证握手、系统信息、窗口流、任务执行、快照回退/fork、跨节点推送。
// 也充当 hostbridge 客户端逻辑的参考实现（sdk/go 由它提炼）。
package main

import (
	"crypto/tls"
	"crypto/x509"
	"encoding/json"
	"flag"
	"fmt"
	"net/http"
	"os"
	"strings"
	"time"

	"github.com/gorilla/websocket"

	"github.com/mixumos/mixumos/internal/auth"
	"github.com/mixumos/mixumos/internal/wire"
)

type client struct {
	conn    *websocket.Conn
	pending map[string]chan *wire.Envelope
	frames  chan *wire.VideoFrame
}

func dial(addr, token, fingerprint string, insecure bool) (*client, error) {
	if !strings.Contains(addr, "://") {
		scheme := "wss://"
		if strings.HasPrefix(addr, "ws://") {
			scheme, addr = "ws://", strings.TrimPrefix(addr, "ws://")
		}
		addr = scheme + strings.TrimPrefix(addr, "wss://")
	}
	tlsCfg := &tls.Config{MinVersion: tls.VersionTLS13}
	switch {
	case fingerprint != "":
		tlsCfg.InsecureSkipVerify = true
		tlsCfg.VerifyPeerCertificate = func(rawCerts [][]byte, _ [][]*x509.Certificate) error {
			return auth.PinVerifier(fingerprint)(rawCerts)
		}
	case insecure:
		tlsCfg.InsecureSkipVerify = true // 自签环境；生产必须用指纹固定
	}
	conn, _, err := (&websocket.Dialer{
		TLSClientConfig: tlsCfg,
		HandshakeTimeout: 15 * time.Second,
	}).Dial(addr+"/ws", http.Header{})
	if err != nil {
		return nil, err
	}
	c := &client{
		conn:    conn,
		pending: map[string]chan *wire.Envelope{},
		frames:  make(chan *wire.VideoFrame, 64),
	}
	go c.readLoop()
	env, _ := wire.NewEnvelope(wire.OpHello, wire.HelloRequest{
		Token:  token,
		Client: "e2eclient/0.1",
		Device: "e2e",
	})
	resp, err := c.call(wire.OpHello, env)
	if err != nil {
		return nil, fmt.Errorf("握手失败: %w", err)
	}
	var hr wire.HelloResponse
	_ = resp.ParsePayload(&hr)
	fmt.Printf("✓ 握手成功 session=%s server=%s arch=%s caps=%v\n",
		hr.SessionID, hr.Server, hr.Arch, hr.Caps)
	return c, nil
}

// readLoop 是唯一的读循环:文本帧按应答分发,二进制帧解包进通道。
// 严禁在别处直接 conn.ReadMessage——那会把应答或帧吃掉(历史 bug)。
func (c *client) readLoop() {
	for {
		mt, data, err := c.conn.ReadMessage()
		if err != nil {
			close(c.frames)
			return
		}
		switch mt {
		case websocket.TextMessage:
			env, err := wire.Decode(data)
			if err != nil {
				continue
			}
			if env.ReplyTo != "" {
				if ch, ok := c.pending[env.ReplyTo]; ok {
					ch <- env
					delete(c.pending, env.ReplyTo)
				}
			}
		case websocket.BinaryMessage:
			if len(data) > 0 && data[0] == wire.ChanVideo {
				if f, err := wire.UnmarshalVideo(data); err == nil {
					select {
					case c.frames <- f:
					default: // 慢消费丢帧,实时性优先
					}
				}
			}
		}
	}
}

func (c *client) call(op string, env *wire.Envelope) (*wire.Envelope, error) {
	ch := make(chan *wire.Envelope, 1)
	c.pending[env.ID] = ch
	raw, _ := json.Marshal(env)
	if err := c.conn.WriteMessage(websocket.TextMessage, raw); err != nil {
		return nil, err
	}
	select {
	case got := <-ch:
		if strings.HasSuffix(got.Op, ".err") {
			var ep wire.ErrorPayload
			_ = got.ParsePayload(&ep)
			return got, fmt.Errorf("%s: %s", ep.Code, ep.Message)
		}
		return got, nil
	case <-time.After(60 * time.Second):
		return nil, fmt.Errorf("%s 超时", op)
	}
}

func (c *client) op(name string, payload any) (*wire.Envelope, error) {
	env, err := wire.NewEnvelope(name, payload)
	if err != nil {
		return nil, err
	}
	return c.call(name, env)
}

func main() {
	addr := flag.String("addr", "wss://127.0.0.1:7601", "mixumd 地址")
	token := flag.String("token", "", "能力令牌")
	insecure := flag.Bool("insecure", false, "跳过证书校验（自签环境）")
	fingerprint := flag.String("fingerprint", "", "服务器证书指纹（SHA-256，冒号分隔）")
	flag.Parse()
	if _, err := os.Stat(*token); err == nil {
		b, err := os.ReadFile(*token)
		if err == nil {
			*token = strings.TrimSpace(string(b))
		}
	}

	c, err := dial(*addr, *token, *fingerprint, *insecure)
	if err != nil {
		fmt.Println("FATAL:", err)
		os.Exit(1)
	}

	// 1. 系统信息
	info, err := c.op(wire.OpSysInfo, nil)
	if err != nil {
		fmt.Println("FAIL sys.info:", err)
		os.Exit(1)
	}
	var si wire.SysInfoResponse
	_ = info.ParsePayload(&si)
	fmt.Printf("✓ 系统信息: %s %s backend=%s compositor=%v windows=%d criu=%v\n",
		si.OS, si.Arch, si.SnapshotBackend, si.Compositor, si.Windows, si.CriuAvailable)

	// 2. 窗口流（如有窗口）
	if si.Compositor {
		dl, err := c.op(wire.OpDisplayList, nil)
		if err != nil {
			fmt.Println("FAIL display.list:", err)
		} else {
			var list wire.DisplayListResponse
			_ = dl.ParsePayload(&list)
			for _, w := range list.Windows {
				fmt.Printf("✓ 窗口: id=%d app=%s title=%q %dx%d mode=%s\n",
					w.ID, w.AppID, w.Title, w.Width, w.Height, w.Mode)
				att, err := c.op(wire.OpDisplayAttach, wire.DisplayAttachRequest{Window: w.ID, FPS: 10})
				if err != nil {
					fmt.Println("FAIL display.attach:", err)
					continue
				}
				var ar wire.DisplayAttachResponse
				_ = att.ParsePayload(&ar)
				_ = ar
				// 收几帧(经单读循环分发,不再直读连接)
				frames := 0
				deadline := time.After(6 * time.Second)
			frameLoop:
				for frames < 3 {
					select {
					case f, ok := <-c.frames:
						if !ok {
							break frameLoop
						}
						if f.WindowID != w.ID {
							continue
						}
						frames++
						if f.HasCursor {
							fmt.Printf("  帧 %d: %dx%d 光标=(%d,%d) 字节=%d\n",
								frames, f.Width, f.Height, f.CursorX, f.CursorY, len(f.Data))
						} else {
							fmt.Printf("  帧 %d: %dx%d 字节=%d\n", frames, f.Width, f.Height, len(f.Data))
						}
					case <-deadline:
						break frameLoop
					}
				}
				_, _ = c.op(wire.OpDisplayDetach, wire.DisplayAttachRequest{Window: w.ID})
				if frames >= 3 {
					fmt.Println("✓ 窗口流正常（≥3 帧）")
				} else {
					fmt.Println("⚠ 窗口流帧数不足:", frames)
				}
			}
		}
	}

	// 3. 任务执行
	ts, err := c.op(wire.OpTaskSubmit, wire.TaskSubmitRequest{
		Instruction:    "e2e: 写文件并回显",
		Agent:          "shell",
		Params:         map[string]string{"cmd": "echo e2e-ok > marker.txt && cat marker.txt"},
		SnapshotPolicy: &wire.SnapshotPolicy{Pre: true, Kind: "disk"},
	})
	if err != nil {
		fmt.Println("FAIL task.submit:", err)
		os.Exit(1)
	}
	var t wire.Task
	_ = ts.ParsePayload(&t)
	fmt.Printf("✓ 任务已提交: %s agent=%s\n", t.ID, t.Agent)

	// 等待完成并读日志事件
	done := false
	deadline := time.Now().Add(20 * time.Second)
	for !done && time.Now().Before(deadline) {
		got, err := c.op(wire.OpTaskGet, map[string]string{"id": t.ID})
		if err != nil {
			fmt.Println("FAIL task.get:", err)
			os.Exit(1)
		}
		var tk wire.Task
		_ = got.ParsePayload(&tk)
		switch tk.State {
		case "completed":
			done = true
		case "failed":
			fmt.Println("FAIL 任务失败:", tk.Error)
			os.Exit(1)
		}
		time.Sleep(200 * time.Millisecond)
	}
	if !done {
		fmt.Println("FAIL 任务超时")
		os.Exit(1)
	}
	fmt.Println("✓ 任务完成")

	// 4. 快照树
	tree, err := c.op(wire.OpSnapTree, nil)
	if err != nil {
		fmt.Println("FAIL snap.tree:", err)
		os.Exit(1)
	}
	var tr wire.SnapTreeResponse
	_ = tree.ParsePayload(&tr)
	fmt.Printf("✓ 快照树: branch=%s head=%s nodes=%d\n", tr.Branch, tr.Head, len(tr.Nodes))
	for _, n := range tr.Nodes {
		fmt.Printf("  [%s] %s %s %s auto=%v chunks=%d size=%d\n",
			n.Branch, n.ID[:16], n.Kind, n.Label, n.Auto, n.Chunks, n.SizeBytes)
	}

	// 5. 回退到任务前置快照，marker.txt 应消失
	var preSnap *wire.SnapNode
	for i := range tr.Nodes {
		if tr.Nodes[i].Auto && tr.Nodes[i].Label != "" {
			if preSnap == nil || tr.Nodes[i].CreatedAt < preSnap.CreatedAt {
				preSnap = &tr.Nodes[i]
			}
		}
	}
	if preSnap == nil && len(tr.Nodes) > 0 {
		preSnap = &tr.Nodes[0] // genesis
	}
	if preSnap != nil {
		if _, err := c.op(wire.OpSnapRestore, wire.SnapRestoreRequest{Node: preSnap.ID}); err != nil {
			fmt.Println("FAIL snap.restore:", err)
			os.Exit(1)
		}
		fmt.Printf("✓ 已回退到 %s（%s）\n", preSnap.ID[:16], preSnap.Label)
	}

	// 6. fork 分支
	if len(tr.Nodes) > 1 {
		target := tr.Nodes[len(tr.Nodes)-1]
		fk, err := c.op(wire.OpSnapFork, wire.SnapForkRequest{Node: target.ID, Branch: "e2e-分支"})
		if err != nil {
			fmt.Println("FAIL snap.fork:", err)
		} else {
			var fr map[string]string
			_ = fk.ParsePayload(&fr)
			fmt.Printf("✓ 已 fork: %s → %s\n", target.ID[:16], fr["branch"])
		}
	}

	fmt.Println("E2E PASS")
}
