package server

import (
	"os"
	"path/filepath"
	"testing"
	"time"

	"github.com/mixumos/mixumos/internal/auth"
	"github.com/mixumos/mixumos/internal/display"
	"github.com/mixumos/mixumos/internal/snap"
	"github.com/mixumos/mixumos/internal/task"
	"github.com/mixumos/mixumos/internal/wire"
)

// testNode 是一套进程内 mixumd（不含 TLS，直接挂 httptest 风格监听）。
type testNode struct {
	srv    *Server
	token  string
	addr   string
	roots  string
	client *wsTestClient
}

func startNode(t *testing.T, name string) *testNode {
	t.Helper()
	base := t.TempDir()
	dataDir := filepath.Join(base, "data")
	workRoot := filepath.Join(base, "work")
	if err := os.MkdirAll(workRoot, 0o755); err != nil {
		t.Fatal(err)
	}

	seed, _ := auth.GenerateSeed()
	signer := auth.NewSigner(seed)
	identity := &auth.Identity{Seed: seed}

	snapEngine, err := snap.Open(snap.Options{
		DataDir: filepath.Join(dataDir, "snapshots"),
		Root:    workRoot,
		Skip:    []string{".mixum"},
	})
	if err != nil {
		t.Fatal(err)
	}
	sandbox := task.NewLocalSandbox(snapEngine.SetTaskPid)
	persist, _ := task.OpenPersist(filepath.Join(dataDir, "tasks"))
	reg := task.NewRegistry()
	_ = reg.Register(task.ShellDriver{})
	tasks := task.NewEngine(reg, sandbox, persist)
	broker := display.NewBroker(filepath.Join(base, "display.sock"))

	srv := New(Config{
		Listen:   "127.0.0.1:0",
		DataDir:  dataDir,
		WorkRoot: workRoot,
	}, testLogger(), identity, tasks, broker, snapEngine)

	// 直接手搓 TLS：用 auth 包生成自签证书再启动。
	// 为简化测试，绕过 Start()，直接用裸 TCP + WS（不校验 TLS——TLS 由 e2e 覆盖）。
	addr := startPlainWS(t, srv)

	tok, err := signer.Mint(auth.Claims{Sub: name, Caps: []wire.Cap{wire.CapAdmin}})
	if err != nil {
		t.Fatal(err)
	}
	cl := dialWS(t, addr)
	t.Cleanup(func() {
		cl.conn.Close()
		snapEngine.Tree.Close()
		persist.Close()
	})
	n := &testNode{srv: srv, token: tok, addr: addr, roots: workRoot, client: cl}
	// 测试客户端直接以 admin 令牌握手。
	if _, err := cl.call(t.Context(), wire.OpHello, wire.HelloRequest{Token: tok, Client: "test"}); err != nil {
		t.Fatal(err)
	}
	return n
}

func TestFullFlow(t *testing.T) {
	a := startNode(t, "mac")
	b := startNode(t, "cloud")
	ctx := t.Context()

	// 1. 握手
	hello, err := a.client.call(ctx, wire.OpHello, wire.HelloRequest{Token: a.token, Client: "test"})
	if err != nil {
		t.Fatal(err)
	}
	var hr wire.HelloResponse
	if err := hello.ParsePayload(&hr); err != nil {
		t.Fatal(err)
	}
	if hr.SessionID == "" {
		t.Fatal("握手未返回 sessionId")
	}

	// 2. 写工作区 → 任务前置快照由服务端打；这里直接建任务
	if err := os.WriteFile(filepath.Join(a.roots, "report.txt"), []byte("第一版"), 0o644); err != nil {
		t.Fatal(err)
	}
	sub, err := a.client.call(ctx, wire.OpTaskSubmit, wire.TaskSubmitRequest{
		Instruction:    "生成季度报告",
		Agent:          "shell",
		Params:         map[string]string{"cmd": "echo done > /dev/null"},
		SnapshotPolicy: &wire.SnapshotPolicy{Pre: true, Kind: "disk"},
	})
	if err != nil {
		t.Fatal(err)
	}
	var submitted wire.Task
	if err := sub.ParsePayload(&submitted); err != nil {
		t.Fatal(err)
	}
	if submitted.PreSnapNode == "" && submitted.State == "" {
		t.Fatal("任务对象异常")
	}
	// 等任务结束
	deadline := time.Now().Add(10 * time.Second)
	for {
		got, err := a.client.call(ctx, wire.OpTaskGet, map[string]string{"id": submitted.ID})
		if err != nil {
			t.Fatal(err)
		}
		var tk wire.Task
		_ = got.ParsePayload(&tk)
		if tk.State == task.StateCompleted || tk.State == task.StateFailed {
			if tk.State == task.StateFailed {
				t.Fatalf("任务失败: %s", tk.Error)
			}
			break
		}
		if time.Now().After(deadline) {
			t.Fatal("任务超时未完成")
		}
		time.Sleep(50 * time.Millisecond)
	}

	// 3. 快照树应有 genesis + 前置快照
	tree, err := a.client.call(ctx, wire.OpSnapTree, nil)
	if err != nil {
		t.Fatal(err)
	}
	var tr wire.SnapTreeResponse
	if err := tree.ParsePayload(&tr); err != nil {
		t.Fatal(err)
	}
	if len(tr.Nodes) < 2 {
		t.Fatalf("快照树节点过少: %d", len(tr.Nodes))
	}

	// 4. 修改工作区 → 打快照 → 回退 → 内容应还原
	if err := os.WriteFile(filepath.Join(a.roots, "report.txt"), []byte("第二版（22:00）"), 0o644); err != nil {
		t.Fatal(err)
	}
	created, err := a.client.call(ctx, wire.OpSnapCreate, wire.SnapCreateRequest{Kind: "disk", Label: "22:00"})
	if err != nil {
		t.Fatal(err)
	}
	var node wire.SnapNode
	if err := created.ParsePayload(&node); err != nil {
		t.Fatal(err)
	}
	// 回退到任务前置快照（包含 report.txt 第一版）
	var preSnap *wire.SnapNode
	for i := range tr.Nodes {
		if tr.Nodes[i].Label == "任务前置无感快照" {
			preSnap = &tr.Nodes[i]
			break
		}
	}
	if preSnap == nil {
		t.Fatal("找不到任务前置快照节点")
	}
	if _, err := a.client.call(ctx, wire.OpSnapRestore, wire.SnapRestoreRequest{Node: preSnap.ID}); err != nil {
		t.Fatal(err)
	}
	content, _ := os.ReadFile(filepath.Join(a.roots, "report.txt"))
	if string(content) != "第一版" {
		t.Fatalf("回退后内容错误: %q", content)
	}

	// 5. fork：从 22:00 节点叉出参数修改支线
	forked, err := a.client.call(ctx, wire.OpSnapRestore, wire.SnapRestoreRequest{
		Node: node.ID, Strategy: "fork", Label: "调参-激进",
	})
	if err != nil {
		t.Fatal(err)
	}
	var fr wire.SnapRestoreResponse
	if err := forked.ParsePayload(&fr); err != nil {
		t.Fatal(err)
	}
	if fr.NewBranch == "" {
		t.Fatal("fork 未返回新分支")
	}
	content, _ = os.ReadFile(filepath.Join(a.roots, "report.txt"))
	if string(content) != "第二版（22:00）" {
		t.Fatalf("fork 恢复后内容错误: %q", content)
	}

	// 6. 跨设备流转：mac(a) 推送节点到 cloud(b)
	push, err := a.client.call(ctx, wire.OpSnapPush, wire.SnapPushRequest{
		Node:   node.ID,
		Remote: b.addr,
		Token:  b.token,
	})
	if err != nil {
		t.Fatal(err)
	}
	var pr wire.SnapPushResponse
	if err := push.ParsePayload(&pr); err != nil {
		t.Fatal(err)
	}
	if pr.ChunksSent == 0 {
		t.Fatal("云端无任何块却未传输，同步对账可能失效")
	}
	// 再推一次：应全部去重
	push2, err := a.client.call(ctx, wire.OpSnapPush, wire.SnapPushRequest{
		Node: node.ID, Remote: b.addr, Token: b.token,
	})
	if err != nil {
		t.Fatal(err)
	}
	var pr2 wire.SnapPushResponse
	if err := push2.ParsePayload(&pr2); err != nil {
		t.Fatal(err)
	}
	if pr2.ChunksSent != 0 {
		t.Fatalf("第二次推送应零传输，实际 %d 块", pr2.ChunksSent)
	}
	// 云端树里能查到节点，且 origin 标记了来源
	treeB, err := b.client.call(ctx, wire.OpSnapTree, nil)
	if err != nil {
		t.Fatal(err)
	}
	var trB wire.SnapTreeResponse
	if err := treeB.ParsePayload(&trB); err != nil {
		t.Fatal(err)
	}
	found := false
	for _, n := range trB.Nodes {
		// 云端导入的节点带 origin 标记（本地父节点在云端不存在时成为远端根）。
		if n.Origin != "" && n.Kind == "disk" {
			found = true
		}
	}
	if !found {
		t.Fatal("云端未导入节点或缺少 origin 标记")
	}
}

func TestTakeoverFlow(t *testing.T) {
	// 接管状态机：agent 模式注入被拒 → claim 需 force → 接管后可注入 → release 复原。
	n := startNode(t, "viewer")
	w := n.srv.broker
	if err := w.SimulateWindow(7, "org.chromium.Chromium", "Chrome", 800, 600); err != nil {
		t.Fatal(err)
	}
	w.SetAgentMode(7, "agent")

	err := n.srv.broker.Inject(7, wire.DisplayInputRequest{Window: 7, Type: "pointer_button", Button: "left", Action: "click"})
	if err == nil {
		t.Fatal("agent 模式下注入应被拒绝")
	}
	if _, err := w.Takeover(7, "claim", false); err == nil {
		t.Fatal("agent 操作中，不带 force 的 claim 应被拒绝")
	}
	mode, err := w.Takeover(7, "claim", true)
	if err != nil || mode != "user" {
		t.Fatalf("force 接管失败: %s %v", mode, err)
	}
	if err := w.Inject(7, wire.DisplayInputRequest{Window: 7, Type: "pointer_motion", X: 10, Y: 20}); err != nil {
		t.Fatalf("接管后注入失败: %v", err)
	}
	mode, err = w.Takeover(7, "release", false)
	if err != nil || mode != "free" {
		t.Fatalf("释放失败: %s %v", mode, err)
	}
}
