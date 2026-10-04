// Package server 实现 mixumd 的 WebSocket 服务：TLS 1.3、能力令牌握手、
// 操作路由、任务/快照/显示三大域的处理器，以及跨设备快照同步的远端角色。
package server

import (
	"crypto/tls"
	"encoding/json"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"sync"
	"time"

	"github.com/gorilla/websocket"

	"github.com/mixumos/mixumos/internal/auth"
	"github.com/mixumos/mixumos/internal/display"
	"github.com/mixumos/mixumos/internal/snap"
	"github.com/mixumos/mixumos/internal/task"
	"github.com/mixumos/mixumos/internal/wire"
)

// Config 是 mixumd 的运行配置。
type Config struct {
	Listen     string `json:"listen"`     // WSS 监听地址，如 0.0.0.0:7601
	DataDir    string `json:"dataDir"`    // 身份/任务/快照存储根
	WorkRoot   string `json:"workRoot"`   // 被快照的工作区根
	Hostname   string `json:"hostname"`
	Encrypt    bool   `json:"encryptSnapshots"`
	TokenTTL   int    `json:"tokenTtlSec"`
	AdminToken string `json:"-"` // bootstrap 管理令牌（自动生成后写入 dataDir/admin-token）
	MaxMsgSize int64  `json:"-"`
}

// Server 聚合全部子系统。
type Server struct {
	cfg      Config
	log      *slog.Logger
	identity *auth.Identity
	signer   *auth.Signer
	verifier *auth.Verifier
	tasks    *task.Engine
	broker   *display.Broker
	snap     *snap.Engine

	mu       sync.Mutex
	sessions map[string]*session

	httpSrv *http.Server
	ln      net.Listener

	// allowPlainWS 仅供测试（进程内裸 TCP 会话）使用；生产强制 TLS 1.3。
	allowPlainWS bool
}

// New 装配一个 Server。调用方先准备好 task/snap/display 引擎。
func New(cfg Config, log *slog.Logger, id *auth.Identity, tasks *task.Engine, broker *display.Broker, sn *snap.Engine) *Server {
	signer := auth.NewSigner(id.Seed)
	verifier := auth.NewVerifier(signer.PublicKey(), 2*time.Minute)
	if cfg.TokenTTL <= 0 {
		cfg.TokenTTL = 3600
	}
	if cfg.MaxMsgSize == 0 {
		cfg.MaxMsgSize = 32 << 20
	}
	return &Server{
		cfg: cfg, log: log, identity: id,
		signer: signer, verifier: verifier,
		tasks: tasks, broker: broker, snap: sn,
		sessions: map[string]*session{},
	}
}

// MintToken 签发能力令牌（管理员用）。
func (s *Server) MintToken(sub string, caps []wire.Cap, ttl time.Duration) (string, error) {
	return s.signer.Mint(auth.Claims{
		Sub:  sub,
		Caps: caps,
		Exp:  time.Now().Add(ttl).Unix(),
	})
}

// Fingerprint 暴露证书指纹（TOFU 固定用）。
func (s *Server) Fingerprint() string { return s.identity.Fingerprint }

// Start 启动 WSS 与本地管理端点（127.0.0.1，健康检查/指标）。
func (s *Server) Start() error {
	mux := http.NewServeMux()
	mux.HandleFunc("/ws", s.handleWS)

	s.httpSrv = &http.Server{
		Handler:           mux,
		ReadHeaderTimeout: 10 * time.Second,
		TLSConfig:         auth.ServerTLSConfig(s.identity.TLS),
	}
	ln, err := net.Listen("tcp", s.cfg.Listen)
	if err != nil {
		return fmt.Errorf("server: 监听 %s 失败: %w", s.cfg.Listen, err)
	}
	s.ln = tls.NewListener(ln, s.httpSrv.TLSConfig)
	go func() {
		if err := s.httpSrv.Serve(s.ln); err != nil && err != http.ErrServerClosed {
			s.log.Error("wss 服务退出", "err", err)
		}
	}()
	s.log.Info("wss 已监听", "addr", s.cfg.Listen, "fingerprint", s.identity.Fingerprint)
	return nil
}

// Stop 优雅关闭。
func (s *Server) Stop() {
	if s.httpSrv != nil {
		_ = s.httpSrv.Close()
	}
}

// ---------- WS 会话 ----------

var upgrader = websocket.Upgrader{
	ReadBufferSize:  64 * 1024,
	WriteBufferSize: 64 * 1024,
	Subprotocols:    []string{"mixum.v1"},
}

type session struct {
	id     string
	srv    *Server
	conn   *websocket.Conn
	claims *auth.Claims
	send   chan outMsg
	closed bool
	mu     sync.Mutex

	limiter   *rateLimiter
	taskSubUn func()
	listUn    func()

	importStage *stageImport // 远端推送导入的暂存区（snap.sync 角色）
}

// outMsg 是写循环的消息：文本（控制）或二进制（流）。
type outMsg struct {
	text []byte
	bin  []byte
}

func (s *Server) handleWS(w http.ResponseWriter, r *http.Request) {
	if r.TLS == nil {
		if !s.allowPlainWS {
			http.Error(w, "TLS required", http.StatusUpgradeRequired)
			return
		}
	} else if r.TLS.Version != tls.VersionTLS13 {
		http.Error(w, "TLS 1.3 required", http.StatusUpgradeRequired)
		return
	}
	conn, err := upgrader.Upgrade(w, r, nil)
	if err != nil {
		return
	}
	sess := &session{
		id:      wire.NewID(),
		srv:     s,
		conn:    conn,
		send:    make(chan outMsg, 512),
		limiter: newRateLimiter(50, 100), // 每秒 50 条，突发 100
	}
	s.mu.Lock()
	s.sessions[sess.id] = sess
	s.mu.Unlock()
	go sess.writeLoop()
	sess.readLoop()
	sess.close()
}

func (sess *session) close() {
	sess.mu.Lock()
	if sess.closed {
		sess.mu.Unlock()
		return
	}
	sess.closed = true
	sess.mu.Unlock()
	if sess.taskSubUn != nil {
		sess.taskSubUn()
	}
	if sess.listUn != nil {
		sess.listUn()
	}
	sess.conn.Close()
	sess.srv.mu.Lock()
	delete(sess.srv.sessions, sess.id)
	sess.srv.mu.Unlock()
}

func (sess *session) readLoop() {
	sess.conn.SetReadLimit(sess.srv.cfg.MaxMsgSize)
	for {
		mt, data, err := sess.conn.ReadMessage()
		if err != nil {
			return
		}
		if !sess.limiter.allow() {
			sess.replyErrRaw("rate_limited", "请求过于频繁")
			continue
		}
		switch mt {
		case websocket.TextMessage:
			sess.dispatchText(data)
		case websocket.BinaryMessage:
			sess.dispatchBinary(data)
		}
	}
}

// dispatchBinary 处理二进制帧：目前仅 ChanChunk（远端推块）。
func (sess *session) dispatchBinary(data []byte) {
	if len(data) == 0 {
		return
	}
	switch data[0] {
	case wire.ChanChunk:
		sess.handleRemoteChunk(data)
	default:
		sess.replyErrRaw(string(wire.ErrBadRequest), "未知二进制通道")
	}
}

func (sess *session) writeLoop() {
	for m := range sess.send {
		var err error
		if m.bin != nil {
			err = sess.conn.WriteMessage(websocket.BinaryMessage, m.bin)
		} else {
			err = sess.conn.WriteMessage(websocket.TextMessage, m.text)
		}
		if err != nil {
			return
		}
	}
}

// enqueue 发送消息（控制或二进制），通道满时阻塞短暂时间后放弃。
func (sess *session) enqueue(m outMsg) {
	sess.mu.Lock()
	closed := sess.closed
	sess.mu.Unlock()
	if closed {
		return
	}
	select {
	case sess.send <- m:
	default:
		// 满队列：等待 2 秒，仍满则断开（写死连接不拖累别人）。
		select {
		case sess.send <- m:
		case <-time.After(2 * time.Second):
			sess.close()
		}
	}
}

func (sess *session) sendEnvelope(e *wire.Envelope) {
	raw, err := json.Marshal(e)
	if err != nil {
		return
	}
	sess.enqueue(outMsg{text: raw})
}

func (sess *session) replyErrRaw(code, msg string) {
	e, _ := wire.NewEnvelope("session.err", wire.ErrorPayload{Code: wire.ErrCode(code), Message: msg})
	sess.sendEnvelope(e)
}

// dispatchText 路由一条控制消息。
func (sess *session) dispatchText(data []byte) {
	env, err := wire.Decode(data)
	if err != nil {
		sess.replyErrRaw(string(wire.ErrBadRequest), err.Error())
		return
	}
	// 握手之外的 op 需要 claims。
	if env.Op != wire.OpHello && sess.claims == nil {
		e, _ := env.ErrorReply(wire.ErrUnauthorized, "请先 hello 握手")
		sess.sendEnvelope(e)
		return
	}
	switch env.Op {
	case wire.OpHello:
		sess.handleHello(env)
	case wire.OpSysPing:
		sess.replyOK(env, map[string]string{"pong": env.ID})
	case wire.OpSysInfo:
		sess.handleSysInfo(env)

	case wire.OpTaskSubmit:
		sess.handleTaskSubmit(env)
	case wire.OpTaskList:
		sess.handleTaskList(env)
	case wire.OpTaskGet:
		sess.handleTaskGet(env)
	case wire.OpTaskCancel:
		sess.handleTaskCancel(env)
	case wire.OpTaskEvents:
		sess.handleTaskEvents(env)

	case wire.OpSnapCreate:
		sess.handleSnapCreate(env)
	case wire.OpSnapTree:
		sess.handleSnapTree(env)
	case wire.OpSnapRestore:
		sess.handleSnapRestore(env)
	case wire.OpSnapFork:
		sess.handleSnapFork(env)
	case wire.OpSnapDelete:
		sess.handleSnapDelete(env)
	case wire.OpSnapGc:
		sess.handleSnapGc(env)
	case wire.OpSnapPush:
		sess.handleSnapPush(env)
	case wire.OpSnapPush + ".import":
		sess.handleRemoteImport(env)
	case wire.OpSnapPush + ".commit":
		sess.handleRemoteCommit(env)

	case wire.OpDisplayList:
		sess.handleDisplayList(env)
	case wire.OpDisplayAttach:
		sess.handleDisplayAttach(env)
	case wire.OpDisplayDetach:
		sess.handleDisplayDetach(env)
	case wire.OpDisplayInput:
		sess.handleDisplayInput(env)
	case wire.OpDisplayTakeover:
		sess.handleDisplayTakeover(env)
	case wire.OpDisplayWindows:
		sess.handleDisplayWindows(env)

	default:
		e, _ := env.ErrorReply(wire.ErrUnsupported, "未知操作: "+env.Op)
		sess.sendEnvelope(e)
	}
}

func (sess *session) replyOK(env *wire.Envelope, payload any) {
	e, err := env.Reply(env.Op+".ok", payload)
	if err != nil {
		return
	}
	sess.sendEnvelope(e)
}

func (sess *session) needCap(env *wire.Envelope, c wire.Cap) bool {
	if !auth.HasCap(sess.claims, c) {
		e, _ := env.ErrorReply(wire.ErrForbidden, "令牌缺少能力: "+string(c))
		sess.sendEnvelope(e)
		return false
	}
	return true
}

// rateLimiter 是简单的令牌桶。
type rateLimiter struct {
	mu      sync.Mutex
	rate    float64
	burst   float64
	tokens  float64
	lastGet time.Time
}

func newRateLimiter(rate int, burst int) *rateLimiter {
	return &rateLimiter{rate: float64(rate), burst: float64(burst), tokens: float64(burst), lastGet: time.Now()}
}

func (r *rateLimiter) allow() bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	now := time.Now()
	r.tokens += now.Sub(r.lastGet).Seconds() * r.rate
	if r.tokens > r.burst {
		r.tokens = r.burst
	}
	r.lastGet = now
	if r.tokens >= 1 {
		r.tokens--
		return true
	}
	return false
}
