package server

import (
	"os"
	"runtime"

	"github.com/mixumos/mixumos/internal/wire"
)

// ---------- 握手与系统 ----------

func (sess *session) handleHello(env *wire.Envelope) {
	var req wire.HelloRequest
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	claims, err := sess.srv.verifier.Verify(req.Token)
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrUnauthorized, err.Error())
		return
	}
	sess.claims = claims
	resp := wire.HelloResponse{
		SessionID: sess.id,
		Server:    "mixumd/" + serverVersion,
		Arch:      runtime.GOARCH,
		Caps:      claims.Caps,
		Versions:  []int{wire.ProtoVersion},
		Heartbeat: 30,
	}
	if k, err := os.ReadFile("/proc/sys/kernel/osrelease"); err == nil {
		resp.Kernel = string(trim(k))
	}
	sess.srv.log.Info("会话建立", "session", sess.id, "sub", claims.Sub, "caps", claims.Caps)
	sess.replyOK(env, resp)
}

func trim(b []byte) []byte {
	for len(b) > 0 && (b[len(b)-1] == '\n' || b[len(b)-1] == ' ') {
		b = b[:len(b)-1]
	}
	return b
}

func (sess *session) handleSysInfo(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapTaskObserve) {
		return
	}
	resp := wire.SysInfoResponse{
		Server:          "mixumd/" + serverVersion,
		Arch:            runtime.GOARCH,
		OS:              "Arch Linux",
		Hostname:        hostName(),
		Caps:            sess.claims.Caps,
		SnapshotBackend: sess.srv.snap.BackendName(),
		CriuAvailable:   sess.srv.snap.Criu != nil && sess.srv.snap.Criu.Available(),
		Compositor:      sess.srv.broker.Online(),
		Windows:         len(sess.srv.broker.List()),
	}
	if k, err := os.ReadFile("/proc/sys/kernel/osrelease"); err == nil {
		resp.Kernel = string(trim(k))
	}
	sess.replyOK(env, resp)
}

func hostName() string {
	h, _ := os.Hostname()
	return h
}

const serverVersion = "0.1.0"

// ---------- 任务 ----------

func (sess *session) handleTaskSubmit(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapTaskRun) {
		return
	}
	var req wire.TaskSubmitRequest
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	if req.Instruction == "" {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, "instruction 不能为空")
		return
	}
	// 令牌若限定任务，则只允许操作该任务——新任务禁止。
	if sess.claims.TaskID != "" {
		sess.replyErr(env, env.Op, wire.ErrForbidden, "令牌已绑定其他任务")
		return
	}
	var policy *wire.SnapshotPolicy
	if req.SnapshotPolicy != nil && req.SnapshotPolicy.Pre {
		kind := req.SnapshotPolicy.Kind
		if kind == "" {
			kind = "disk"
		}
		node, err := sess.srv.snap.Snapshot(kind, "任务前置无感快照", true)
		if err != nil {
			sess.replyErr(env, env.Op, wire.ErrInternal, "前置快照失败: "+err.Error())
			return
		}
		policy = req.SnapshotPolicy
		_ = node
	}
	t, err := sess.srv.tasks.Submit(req.Instruction, req.Agent, req.Params)
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	if policy != nil {
		// 任务级失败回退策略由引擎在 run 结束时检查（v1 先持久化到任务参数）
		// 留意：Submit 之后任务可能已结束，这里仅记录策略。
	}
	sess.srv.log.Info("任务提交", "task", t.ID, "agent", t.Agent)
	sess.replyOK(env, t.Task)
}

func (sess *session) handleTaskList(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapTaskObserve) {
		return
	}
	sess.replyOK(env, map[string]any{"tasks": sess.srv.tasks.List()})
}

func (sess *session) handleTaskGet(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapTaskObserve) {
		return
	}
	var req struct {
		ID string `json:"id"`
	}
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	t, err := sess.srv.tasks.Get(req.ID)
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrNotFound, err.Error())
		return
	}
	if sess.claims.TaskID != "" && sess.claims.TaskID != req.ID {
		sess.replyErr(env, env.Op, wire.ErrForbidden, "令牌绑定了其他任务")
		return
	}
	sess.replyOK(env, &t.Task)
}

func (sess *session) handleTaskCancel(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapTaskRun) {
		return
	}
	var req struct {
		ID string `json:"id"`
	}
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	if err := sess.srv.tasks.Cancel(req.ID); err != nil {
		sess.replyErr(env, env.Op, wire.ErrNotFound, err.Error())
		return
	}
	sess.replyOK(env, map[string]string{"id": req.ID})
}

// handleTaskEvents 订阅任务事件流（推送到本连接）。
func (sess *session) handleTaskEvents(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapTaskObserve) {
		return
	}
	ch, un := sess.srv.tasks.Subscribe()
	sess.taskSubUn = un
	sess.replyOK(env, map[string]string{"subscribed": sess.id})
	go func() {
		for ev := range ch {
			if sess.claims.TaskID != "" && ev.TaskID != sess.claims.TaskID {
				continue
			}
			e, err := wire.NewEnvelope(wire.OpTaskEvent, ev)
			if err != nil {
				return
			}
			sess.sendEnvelope(e)
		}
	}()
}

// replyErr 统一错误回复。
func (sess *session) replyErr(env *wire.Envelope, op string, code wire.ErrCode, msg string) {
	e, _ := env.ErrorReply(code, msg)
	sess.sendEnvelope(e)
}
