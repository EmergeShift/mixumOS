package server

import (
	"fmt"

	"github.com/mixumos/mixumos/internal/snap"
	"github.com/mixumos/mixumos/internal/wire"
)

// ---------- 快照 ----------

func (sess *session) handleSnapCreate(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapSnapManage) {
		return
	}
	var req wire.SnapCreateRequest
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	var node *snap.TreeNode
	var err error
	if req.Parent != "" {
		node, err = sess.srv.snap.SnapshotOnBranch(req.Kind, req.Label, req.Auto, req.Parent)
	} else {
		node, err = sess.srv.snap.Snapshot(req.Kind, req.Label, req.Auto)
	}
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrInternal, err.Error())
		return
	}
	sess.replyOK(env, toSnapNode(node))
}

func toSnapNode(n *snap.TreeNode) wire.SnapNode {
	return wire.SnapNode{
		ID: n.ID, Parent: n.Parent, Branch: n.Branch, Label: n.Label,
		Kind: n.Kind, Auto: n.Auto, CreatedAt: n.CreatedAt, Origin: n.Origin,
		SizeBytes: n.SizeBytes, Chunks: n.Chunks, Kernel: n.Kernel, Arch: n.Arch,
	}
}

func (sess *session) handleSnapTree(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapTaskObserve) {
		return
	}
	branch, head, err := sess.srv.snap.Tree.Head()
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrInternal, err.Error())
		return
	}
	all, err := sess.srv.snap.Tree.All()
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrInternal, err.Error())
		return
	}
	nodes := make([]wire.SnapNode, 0, len(all))
	for _, n := range all {
		nodes = append(nodes, toSnapNode(n))
	}
	headID := ""
	if head != nil {
		headID = head.ID
	}
	sess.replyOK(env, wire.SnapTreeResponse{Head: headID, Branch: branch, Nodes: nodes})
}

func (sess *session) handleSnapRestore(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapSnapManage) {
		return
	}
	var req wire.SnapRestoreRequest
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	strategy := req.Strategy
	if strategy == "" {
		strategy = "inplace"
	}
	branch, _, err := sess.srv.snap.Restore(req.Node, strategy, req.Label)
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrSnapshotInvalid, err.Error())
		return
	}
	resp := wire.SnapRestoreResponse{RestoredTo: req.Node}
	if strategy == "fork" {
		resp.NewBranch = branch
	}
	sess.srv.log.Info("快照恢复", "node", req.Node, "strategy", strategy, "branch", branch)
	sess.replyOK(env, resp)
}

func (sess *session) handleSnapFork(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapSnapManage) {
		return
	}
	var req wire.SnapForkRequest
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	branch, err := sess.srv.snap.Fork(req.Node, req.Branch)
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrNotFound, err.Error())
		return
	}
	sess.replyOK(env, map[string]string{"branch": branch})
}

func (sess *session) handleSnapDelete(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapSnapManage) {
		return
	}
	var req struct {
		Node string `json:"node"`
	}
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	if err := sess.srv.snap.Tree.Delete(req.Node); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	sess.replyOK(env, map[string]string{"deleted": req.Node})
}

func (sess *session) handleSnapGc(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapSnapManage) {
		return
	}
	n, err := sess.srv.snap.Gc()
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrInternal, err.Error())
		return
	}
	sess.replyOK(env, map[string]int{"removed": n})
}

// handleSnapPush 把节点推送到远端 mixumd（跨设备流转：mac 盒盖前 → 云端继续跑）。
// 流程：拨号远端 → hello(snap.sync) → snap.push.import（元数据+清单+块表）
// → 远端回 missing 列表 → 二进制块流（去重后只传缺失块）→ snap.push.commit。
func (sess *session) handleSnapPush(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapSnapSync) {
		return
	}
	var req wire.SnapPushRequest
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	node, err := sess.srv.snap.Tree.Get(req.Node)
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrNotFound, err.Error())
		return
	}
	if node.Disk == nil {
		sess.replyErr(env, env.Op, wire.ErrUnsupported, "纯内存快照暂不支持流转")
		return
	}
	stat, err := pushNodeToRemote(sess.srv, node, req.Remote, req.Token)
	if err != nil {
		sess.replyErr(env, env.Op, wire.ErrInternal, "推送失败: "+err.Error())
		return
	}
	sess.srv.log.Info("快照已推送", "node", node.ID, "remote", req.Remote, "chunks", stat.ChunksSent)
	sess.replyOK(env, stat)
}

func pushNodeToRemote(s *Server, node *snap.TreeNode, remote, token string) (*wire.SnapPushResponse, error) {
	cl, err := DialRemote(remote, token)
	if err != nil {
		return nil, err
	}
	defer cl.Close()

	chunks := snap.ManifestChunks(node.Disk)
	importPayload := map[string]any{
		"node": map[string]any{
			"id": node.ID, "parent": node.Parent, "branch": node.Branch,
			"label": node.Label, "kind": node.Kind, "createdAt": node.CreatedAt,
			"kernel": node.Kernel, "arch": node.Arch, "origin": hostName(),
		},
		"manifest": node.Disk,
		"chunks":   chunks,
	}
	var importResp struct {
		Missing []string `json:"missing"`
	}
	if err := cl.Call(wire.OpSnapPush+".import", importPayload, &importResp); err != nil {
		return nil, err
	}
	need := map[string]struct{}{}
	for _, h := range importResp.Missing {
		need[h] = struct{}{}
	}
	var sent int
	var bytesSent int64
	for _, h := range chunks {
		if _, ok := need[h]; !ok {
			continue
		}
		data, err := s.snap.Store.Get(h)
		if err != nil {
			return nil, err
		}
		if err := cl.SendChunk(h, data); err != nil {
			return nil, err
		}
		sent++
		bytesSent += int64(len(data))
	}
	if sent > 0 {
		if err := cl.Call(wire.OpSnapPush+".commit", map[string]int{"count": sent}, nil); err != nil {
			return nil, err
		}
	}
	dedup := "1.00"
	if sent > 0 && sent < len(chunks) {
		dedup = fmt.Sprintf("%.2f", float64(len(chunks))/float64(sent))
	}
	return &wire.SnapPushResponse{
		Node: node.ID, ChunksSent: sent, BytesSent: bytesSent, DedupRatio: dedup,
	}, nil
}

// ---------- 本端作为推送目标（持有 snap.sync 能力的连接） ----------

// stageImport 暂存一次进行中的远端导入。
type stageImport struct {
	meta     map[string]any
	manifest *snap.Manifest
	chunks   []string
	missing  []string
	received map[string]struct{}
}

// handleRemoteImport 处理 snap.push.import：对账并回给缺失块列表。
func (sess *session) handleRemoteImport(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapSnapSync) {
		return
	}
	var req struct {
		Node     map[string]any `json:"node"`
		Manifest *snap.Manifest `json:"manifest"`
		Chunks   []string       `json:"chunks"`
	}
	if err := env.ParsePayload(&req); err != nil {
		sess.replyErr(env, env.Op, wire.ErrBadRequest, err.Error())
		return
	}
	var missing []string
	for _, h := range req.Chunks {
		if !sess.srv.snap.Store.Has(h) {
			missing = append(missing, h)
		}
	}
	sess.importStage = &stageImport{
		meta: req.Node, manifest: req.Manifest, chunks: req.Chunks,
		missing: missing, received: map[string]struct{}{},
	}
	sess.replyOK(env, map[string]any{"missing": missing})
}

// handleRemoteChunk 处理二进制块帧（ChanChunk：[1:33] 哈希，[33:] 数据）。
func (sess *session) handleRemoteChunk(data []byte) {
	if sess.importStage == nil {
		return
	}
	if len(data) < 1+32 {
		return
	}
	hashStr := fmt.Sprintf("%x", data[1:33])
	if _, err := sess.srv.snap.Store.Put(data[33:]); err != nil {
		sess.srv.log.Warn("远端块写入失败", "err", err)
		return
	}
	sess.importStage.received[hashStr] = struct{}{}
}

// handleRemoteCommit 校验收到的块并落树。
func (sess *session) handleRemoteCommit(env *wire.Envelope) {
	if !sess.needCap(env, wire.CapSnapSync) || sess.importStage == nil {
		return
	}
	st := sess.importStage
	for _, h := range st.missing {
		if _, ok := st.received[h]; !ok {
			sess.replyErr(env, env.Op, wire.ErrConflict, "缺失块未送达: "+h[:12])
			return
		}
	}
	node := &snap.TreeNode{
		ID: strOr(st.meta["id"], "snap_"+wire.NewID()),
		Parent: strOr(st.meta["parent"], ""),
		Branch: strOr(st.meta["branch"], "imported"),
		Label:  strOr(st.meta["label"], ""),
		Kind:   strOr(st.meta["kind"], snap.KindDisk),
		Kernel: strOr(st.meta["kernel"], ""),
		Arch:   strOr(st.meta["arch"], ""),
		Origin: strOr(st.meta["origin"], "remote"),
		Disk:   st.manifest,
	}
	// 远端节点与本地树合并：若父节点不在本地，视作新分支根。
	if node.Parent != "" {
		if _, err := sess.srv.snap.Tree.Get(node.Parent); err != nil {
			node.Parent = ""
			node.Label = node.Label + "（远端根）"
		}
	}
	if err := sess.srv.snap.Tree.Add(node); err != nil {
		sess.replyErr(env, env.Op, wire.ErrConflict, "落树失败: "+err.Error())
		return
	}
	sess.importStage = nil
	sess.srv.log.Info("远端快照已导入", "node", node.ID, "origin", node.Origin)
	sess.replyOK(env, map[string]string{"node": node.ID})
}

func strOr(v any, def string) string {
	if s, ok := v.(string); ok && s != "" {
		return s
	}
	return def
}
