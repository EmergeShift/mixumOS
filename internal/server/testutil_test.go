package server

import (
	"encoding/json"
	"log/slog"
	"net"
	"net/http"
	"os"
	"testing"
	"time"

	"github.com/gorilla/websocket"

	"github.com/mixumos/mixumos/internal/wire"
)

// 测试辅助：无 TLS 的 WS 监听（TLS 行为在容器 e2e 中覆盖），
// 以及一个带请求-应答关联的最小客户端。

func testLogger() *slog.Logger {
	return slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelError}))
}

// startPlainWS 把 Server 的会话处理器挂在裸 TCP 上，返回地址。
func startPlainWS(t *testing.T, srv *Server) string {
	t.Helper()
	srv.allowPlainWS = true
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	mux := http.NewServeMux()
	mux.HandleFunc("/ws", srv.handleWS)
	go http.Serve(ln, mux)
	t.Cleanup(func() { ln.Close() })
	return "ws://" + ln.Addr().String()
}

type wsTestClient struct {
	conn    *websocket.Conn
	pending map[string]chan *wire.Envelope
	mu      chan struct{}
}

func dialWS(t *testing.T, addr string) *wsTestClient {
	t.Helper()
	conn, _, err := websocket.DefaultDialer.Dial(addr+"/ws", nil)
	if err != nil {
		t.Fatal(err)
	}
	cl := &wsTestClient{conn: conn, pending: map[string]chan *wire.Envelope{}, mu: make(chan struct{}, 1)}
	go cl.readLoop()
	return cl
}

func (c *wsTestClient) readLoop() {
	for {
		mt, data, err := c.conn.ReadMessage()
		if err != nil {
			return
		}
		_ = mt
		env, err := wire.Decode(data)
		if err != nil {
			continue
		}
		if env.ReplyTo == "" {
			continue
		}
		c.mu <- struct{}{}
		if ch, ok := c.pending[env.ReplyTo]; ok {
			ch <- env
			delete(c.pending, env.ReplyTo)
		}
		<-c.mu
	}
}

func (c *wsTestClient) call(deadline interface{ Done() <-chan struct{} }, op string, payload any) (*wire.Envelope, error) {
	env, err := wire.NewEnvelope(op, payload)
	if err != nil {
		return nil, err
	}
	ch := make(chan *wire.Envelope, 1)
	c.mu <- struct{}{}
	c.pending[env.ID] = ch
	<-c.mu
	raw, err := json.Marshal(env)
	if err != nil {
		return nil, err
	}
	if err := c.conn.WriteMessage(websocket.TextMessage, raw); err != nil {
		return nil, err
	}
	select {
	case got := <-ch:
		if len(got.Op) > 4 && got.Op[len(got.Op)-4:] == ".err" {
			var ep wire.ErrorPayload
			_ = got.ParsePayload(&ep)
			return got, &wireError{op: got.Op, msg: ep.Message}
		}
		return got, nil
	case <-time.After(15 * time.Second):
		return nil, &wireError{op: op, msg: "超时"}
	}
}

type wireError struct{ op, msg string }

func (e *wireError) Error() string { return e.op + ": " + e.msg }
