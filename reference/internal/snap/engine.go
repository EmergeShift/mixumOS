package snap

import (
	"crypto/rand"
	"fmt"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
)

// FreezeFunc 在打快照前冻结任务进程、结束后解冻，保证「无感」：
// 任务进程无感知（冻结窗口内没有任何代码运行），快照点即冻结点。
// 实现方通常是任务引擎（SIGSTOP / cgroup.freeze）。
type FreezeFunc func(fn func() error) error

// Engine 是快照引擎的高层门面。
type Engine struct {
	Store  *Store
	Tree   *Tree
	Criu   *Criu
	Root   string // 被快照的目录（容器 upper / 数据根）
	Skip   []string
	Freeze FreezeFunc // 可为 nil（无任务运行时）

	taskPid *int
	mu      sync.Mutex // 串行化快照/恢复，避免并发撕裂
}

// Options 打开引擎的参数。
type Options struct {
	DataDir string // 树与存储的根
	Root    string // 被快照目录
	Skip    []string
	Encrypt bool
}

// Open 初始化引擎。首次运行生成 master key 与 genesis 节点。
func Open(opts Options) (*Engine, error) {
	storeDir := filepath.Join(opts.DataDir, "store")
	treeDir := filepath.Join(opts.DataDir, "tree")
	masterKey, err := loadOrCreateMasterKey(opts.DataDir)
	if err != nil {
		return nil, err
	}
	var key []byte
	if opts.Encrypt {
		key = masterKey
	}
	store, err := OpenStore(storeDir, key, "mixum-"+filepath.Base(opts.DataDir))
	if err != nil {
		return nil, err
	}
	tree, err := OpenTree(treeDir)
	if err != nil {
		return nil, err
	}
	e := &Engine{Store: store, Tree: tree, Criu: NewCriu(), Root: opts.Root, Skip: opts.Skip}
	// 首次启动：打 genesis 快照。
	if _, head, err := tree.Head(); err != nil || head == nil {
		if _, err := e.Snapshot(KindDisk, "genesis", true); err != nil {
			return nil, err
		}
	}
	return e, nil
}

// Snapshot 打一次快照。kind: disk|memory|hybrid。
// memory/hybrid 需要 CRIU 与 pid（任务进程树首领；pid=0 表示无任务、退化为 disk）。
func (e *Engine) Snapshot(kind, label string, auto bool) (*TreeNode, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.snapshotLocked(kind, label, auto, nil)
}

// SnapshotOnBranch 在指定分支与父节点下打快照（恢复后继续工作时使用）。
func (e *Engine) SnapshotOnBranch(kind, label string, auto bool, parent string) (*TreeNode, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.snapshotLocked(kind, label, auto, &parent)
}

func (e *Engine) snapshotLocked(kind, label string, auto bool, parent *string) (*TreeNode, error) {
	branch, head, err := e.Tree.Head()
	if err != nil {
		return nil, err
	}
	if parent != nil && *parent != "" {
		pn, err := e.Tree.Get(*parent)
		if err != nil {
			return nil, err
		}
		// 新节点跟随当前工作分支（fork 之后当前分支可能不同于父节点的原分支）。
		branch = e.Tree.CurrentBranch()
		head = pn
	} else if head != nil {
		parent = &head.ID
	}

	node := &TreeNode{
		Parent: deref(parent),
		Branch: branch,
		Label:  label,
		Kind:   kind,
		Auto:   auto,
		Kernel: kernelRelease(),
		Arch:   runtimeArch(),
	}

	doDisk := kind == KindDisk || kind == KindHybrid
	doMem := (kind == KindMemory || kind == KindHybrid) && e.Criu.Available()
	if (kind == KindMemory || kind == KindHybrid) && !e.Criu.Available() {
		return nil, fmt.Errorf("snap: 内存快照需要 CRIU，当前环境不可用")
	}

	var memStats *PackStats
	err = e.freezeAround(func() error {
		if doDisk {
			m, stats, err := PackDirectory(e.Root, e.Store, e.Skip)
			if err != nil {
				return err
			}
			node.Disk = m
			node.SizeBytes += stats.BytesLogical
			node.Chunks = len(ManifestChunks(m))
		}
		// pid=0 表示无任务运行(或驱动未注册首领进程):退化为 disk,
		// 与构造注释的文档语义一致;CRIU 不接受 --tree 0。
		if doMem && derefInt(e.taskPid) > 0 {
			imgDir := filepath.Join(os.TempDir(), "mixum-criu-"+node.ID)
			defer os.RemoveAll(imgDir)
			if err := e.Criu.Dump(derefInt(e.taskPid), imgDir, false); err != nil {
				return err
			}
			mm, ms, err := PackCriuImages(imgDir, e.Store)
			if err != nil {
				return err
			}
			node.Memory = mm
			memStats = ms
			node.SizeBytes += ms.BytesLogical
			node.Chunks += len(ManifestChunks(mm))
		}
		return nil
	})
	if err != nil {
		return nil, err
	}
	_ = memStats

	if node.Disk != nil {
		for _, h := range ManifestChunks(node.Disk) {
			e.Store.Mark(h)
		}
	}
	if node.Memory != nil {
		for _, h := range ManifestChunks(node.Memory) {
			e.Store.Mark(h)
		}
	}
	if err := e.Tree.Add(node); err != nil {
		return nil, err
	}
	return node, nil
}

// Restore 恢复到节点。strategy=inplace 把当前分支 head 移回旧节点——
// 旧节点之后如果已有别的孩子，会在恢复后继续工作时自然分叉出新支线；
// strategy=fork 显式创建新分支并切换过去。返回新分支名（fork 时）。
func (e *Engine) Restore(nodeID, strategy, label string) (branch string, node *TreeNode, err error) {
	e.mu.Lock()
	defer e.mu.Unlock()

	node, err = e.Tree.Get(nodeID)
	if err != nil {
		return "", nil, err
	}
	if node.Kind == KindMemory {
		return "", nil, fmt.Errorf("snap: 纯内存快照仅包含进程树，请配合 disk/hybrid 使用")
	}
	if node.Kernel != "" && node.Kernel != kernelRelease() {
		return "", nil, fmt.Errorf("snap: 内核不匹配（快照 %s，当前 %s），拒绝恢复", node.Kernel, kernelRelease())
	}
	if node.Arch != "" && node.Arch != runtimeArch() {
		return "", nil, fmt.Errorf("snap: 架构不匹配（快照 %s，当前 %s）", node.Arch, runtimeArch())
	}

	// 内容落地：先清 Root（保留 skip 项），再物化清单。
	if err := e.clearRoot(); err != nil {
		return "", nil, err
	}
	if _, err := Materialize(node.Disk, e.Store, e.Root); err != nil {
		return "", nil, err
	}

	switch strategy {
	case "fork":
		want := label
		if want == "" {
			want = node.Branch + "-fork"
		}
		branch, err = e.Tree.ForkBranch(nodeID, want)
		if err != nil {
			return "", nil, err
		}
		if err := e.Tree.SetCurrentBranch(branch); err != nil {
			return "", nil, err
		}
		return branch, node, nil
	default: // inplace：head 移回该节点所在分支的该节点
		if err := e.Tree.SetHead(node.Branch, nodeID); err != nil {
			return "", nil, err
		}
		if err := e.Tree.SetCurrentBranch(node.Branch); err != nil {
			return "", nil, err
		}
		return node.Branch, node, nil
	}
}

// Fork 在节点上开新分支（不改动当前工作区），返回分支名。
func (e *Engine) Fork(nodeID, want string) (string, error) {
	e.mu.Lock()
	defer e.mu.Unlock()
	return e.Tree.ForkBranch(nodeID, want)
}

// Gc 清理不再被任何节点引用的块。
func (e *Engine) Gc() (int, error) {
	reach, err := e.Tree.Reachable()
	if err != nil {
		return 0, err
	}
	e.Store.mu.Lock()
	marks := map[string]struct{}{}
	e.Store.mu.Unlock()
	nodes, err := e.Tree.All()
	if err != nil {
		return 0, err
	}
	for _, n := range nodes {
		if _, ok := reach[n.ID]; !ok {
			continue
		}
		for _, m := range []*Manifest{n.Disk, n.Memory} {
			if m == nil {
				continue
			}
			for _, h := range ManifestChunks(m) {
				marks[h] = struct{}{}
			}
		}
	}
	e.Store.mu.Lock()
	e.Store.gcMarks = marks
	e.Store.mu.Unlock()
	return e.Store.Sweep()
}

// clearRoot 清空 Root（保留 skip 前缀）。
func (e *Engine) clearRoot() error {
	entries, err := os.ReadDir(e.Root)
	if err != nil {
		return err
	}
	for _, en := range entries {
		name := en.Name()
		skip := false
		for _, s := range e.Skip {
			if name == s || strings.HasPrefix(name, strings.TrimSuffix(s, "/")+"/") {
				skip = true
				break
			}
		}
		if skip {
			continue
		}
		if err := os.RemoveAll(filepath.Join(e.Root, name)); err != nil {
			return err
		}
	}
	return nil
}

// freezeAround 在冻结回调内执行 fn。
func (e *Engine) freezeAround(fn func() error) error {
	if e.Freeze == nil {
		return fn()
	}
	return e.Freeze(fn)
}

// TaskPid 由任务引擎在打内存快照前设置。
func (e *Engine) SetTaskPid(pid int) { e.taskPid = &pid }

func loadOrCreateMasterKey(dir string) ([]byte, error) {
	path := filepath.Join(dir, "master.key")
	if b, err := os.ReadFile(path); err == nil && len(b) == 32 {
		return b, nil
	}
	key := make([]byte, 32)
	if _, err := rand.Read(key); err != nil {
		return nil, err
	}
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, err
	}
	if err := os.WriteFile(path, key, 0o600); err != nil {
		return nil, err
	}
	return key, nil
}

func deref(p *string) string {
	if p == nil {
		return ""
	}
	return *p
}

func derefInt(p *int) int {
	if p == nil {
		return 0
	}
	return *p
}

func runtimeArch() string { return runtime.GOARCH }
