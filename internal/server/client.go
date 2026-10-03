package server

// RemoteClient 是 mixumd 之间的远端客户端连接（跨设备快照流转用）。
// 快照同步是严格顺序的协议（import → 块流 → commit），因此客户端实现为
// 单飞行（一次一条等待应答）模式，简单可靠。

import (
	"crypto/sha256"
	"crypto/tls"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"sync"
	"time"

	"github.com/gorilla/websocket"

	"github.com/mixumos/mixumos/internal/wire"
)

// RemoteClient 是到另一台 mixumd 的连接。
type RemoteClient struct {
	conn *websocket.Conn
	mu   sync.Mutex // 序列化全部写

	replyCh chan *wire.Envelope
	errCh   chan error
	done    chan struct{}
}

// DialRemote 拨号远端 mixumd（wss://host:port）。
// token 为远端签发的能力令牌（至少 snap.sync）。
func DialRemote(addr, token string) (*RemoteClient, error) {
	if !strings.Contains(addr, "://") {
		addr = "wss://" + addr
	}
	dialer := &websocket.Dialer{
		TLSClientConfig: &tls.Config{
			MinVersion:         tls.VersionTLS13,
			InsecureSkipVerify: true, // 云端自签证书；指纹固定机制见 docs/security.md
		},
		HandshakeTimeout: 15 * time.Second,
	}
	conn, _, err := dialer.Dial(addr+"/ws", http.Header{})
	if err != nil {
		return nil, fmt.Errorf("server: 连接远端失败: %w", err)
	}
	cl := &RemoteClient{
		conn:    conn,
		replyCh: make(chan *wire.Envelope, 4),
		errCh:   make(chan error, 1),
		done:    make(chan struct{}),
	}
	if err := cl.hello(token); err != nil {
		conn.Close()
		return nil, err
	}
	go cl.readLoop()
	return cl, nil
}

func (c *RemoteClient) hello(token string) error {
	req, _ := wire.NewEnvelope(wire.OpHello, wire.HelloRequest{
		Token:  token,
		Client: "mixumd-sync/0.1",
	})
	if err := c.writeEnvelope(req); err != nil {
		return err
	}
	c.conn.SetReadDeadline(time.Now().Add(15 * time.Second))
	_, data, err := c.conn.ReadMessage()
	c.conn.SetReadDeadline(time.Time{})
	if err != nil {
		return err
	}
	resp, err := wire.Decode(data)
	if err != nil {
		return err
	}
	if strings.HasSuffix(resp.Op, ".err") {
		var ep wire.ErrorPayload
		_ = resp.ParsePayload(&ep)
		return fmt.Errorf("远端拒绝握手: %s %s", ep.Code, ep.Message)
	}
	return nil
}

func (c *RemoteClient) readLoop() {
	for {
		mt, data, err := c.conn.ReadMessage()
		if err != nil {
			select {
			case c.errCh <- err:
			default:
			}
			close(c.done)
			return
		}
		if mt != websocket.TextMessage {
			continue
		}
		env, err := wire.Decode(data)
		if err != nil {
			continue
		}
		select {
		case c.replyCh <- env:
		default: // 无人等待的消息（如事件推送）丢弃
		}
	}
}

func (c *RemoteClient) writeEnvelope(e *wire.Envelope) error {
	raw, err := json.Marshal(e)
	if err != nil {
		return err
	}
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.conn.WriteMessage(websocket.TextMessage, raw)
}

// Call 发送一条控制消息并等待回复。resp 为 nil 时忽略负载。
func (c *RemoteClient) Call(op string, payload any, resp any) error {
	req, err := wire.NewEnvelope(op, payload)
	if err != nil {
		return err
	}
	if err := c.writeEnvelope(req); err != nil {
		return err
	}
	select {
	case got := <-c.replyCh:
		if strings.HasSuffix(got.Op, ".err") {
			var ep wire.ErrorPayload
			_ = got.ParsePayload(&ep)
			return fmt.Errorf("%s: %s", ep.Code, ep.Message)
		}
		if resp != nil {
			return got.ParsePayload(resp)
		}
		return nil
	case err := <-c.errCh:
		return fmt.Errorf("远端连接中断: %w", err)
	case <-time.After(180 * time.Second):
		return fmt.Errorf("远端响应超时: %s", op)
	}
}

// SendChunk 发送一个块（ChanChunk：[1:33] sha256，[33:] 数据）。
func (c *RemoteClient) SendChunk(hashStr string, data []byte) error {
	sum := sha256.Sum256(data)
	if fmt.Sprintf("%x", sum) != hashStr {
		return fmt.Errorf("server: 块 %s 数据不匹配", hashStr[:12])
	}
	buf := make([]byte, 33+len(data))
	buf[0] = wire.ChanChunk
	copy(buf[1:33], sum[:])
	copy(buf[33:], data)
	c.mu.Lock()
	defer c.mu.Unlock()
	return c.conn.WriteMessage(websocket.BinaryMessage, buf)
}

// Close 关闭连接。
func (c *RemoteClient) Close() { c.conn.Close() }
