package server

import (
	"sync"

	"github.com/mixumos/mixumos/internal/auth"
	"github.com/mixumos/mixumos/internal/display"
	"github.com/mixumos/mixumos/internal/wire"
)

// ---------- 显示流 ----------

var (
	subMu       sync.Mutex
	nextSubSeq  uint64
)

func newSubID() string {
	subMu.Lock()
	defer subMu.Unlock()
	nextSubSeq++
	return wire.NewID()
}

func (sess *session) handleDisplayList(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapDisplayView) {
		return
	}
	sess.replyOK(env, wire.DisplayListResponse{Windows: sess.srv.broker.List()})
}

func (sess *session) handleDisplayAttach(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapDisplayView) {
		return
	}
	var req wire.DisplayAttachRequest
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	if !auth.WindowAllowed(sess.claims, req.Window) {
		sess.replyErr(env, env.Op, wire.ErrForbidden, "令牌未授权该窗口")
		return
	}
	sub := &display.Sub{ID: newSubID(), Caps: func(c wire.Cap) bool { return auth.HasCap(sess.claims, c) }}
	resp, err := sess.srv.broker.Attach(req.Window, req.FPS, sub)
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrNotFound, err.Error())
		return
	}
	// 帧泵：把 broker 的帧转发到本连接（二进制通道）。
	go func() {
		for raw := range sub.Frames {
			sess.enqueue(outMsg{bin: raw})
		}
	}()
	sess.replyOK(env, resp)
}

func (sess *session) handleDisplayDetach(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapDisplayView) {
		return
	}
	var req wire.DisplayAttachRequest
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	sess.srv.broker.Detach(sess.id, req.Window)
	sess.replyOK(env, map[string]uint32{"window": req.Window})
}

func (sess *session) handleDisplayInput(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapDisplayInput) {
		return
	}
	var req wire.DisplayInputRequest
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	if !auth.WindowAllowed(sess.claims, req.Window) {
		sess.replyErr(env, env.Op, wire.ErrForbidden, "令牌未授权该窗口")
		return
	}
	if err := sess.srv.broker.Inject(req.Window, req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrConflict, err.Error())
		return
	}
	sess.replyOK(env, map[string]any{"ok": true})
}

func (sess *session) handleDisplayTakeover(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapDisplayInput) {
		return
	}
	var req wire.DisplayTakeoverRequest
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	mode, err := sess.srv.broker.Takeover(req.Window, req.Action, req.Force)
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrConflict, err.Error())
		return
	}
	sess.replyOK(env, map[string]string{"mode": mode})
}

// handleDisplayWindows 订阅窗口增删/标题变化推送。
func (sess *session) handleDisplayWindows(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapDisplayView) {
		return
	}
	ch, un := sess.srv.broker.SubscribeList()
	sess.listUn = un
	sess.replyOK(env, map[string]string{"subscribed": sess.id})
	go func() {
		for w := range ch {
			e, err := wire.NewEnvelope(wire.OpDisplayWindows, w)
			if err != nil {
				return
			}
			sess.sendEnvelope(e)
		}
	}()
}
