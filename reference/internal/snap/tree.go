package snap

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"time"

	bolt "go.etcd.io/bbolt"

	"github.com/mixumos/mixumos/internal/wire"
)

// 节点类型常量。
const (
	KindDisk   = "disk"
	KindMemory = "memory"
	KindHybrid = "hybrid"
)

var (
	bktNodes  = []byte("nodes")
	bktHeads  = []byte("heads") // branch -> head node id
	bktMeta   = []byte("meta")  // currentBranch 等
)

// TreeNode 是快照树的一个节点（比 wire 的 SnapNode 多内部字段）。
type TreeNode struct {
	ID        string            `json:"id"`
	Parent    string            `json:"parent,omitempty"`
	Branch    string            `json:"branch"`
	Label     string            `json:"label,omitempty"`
	Kind      string            `json:"kind"`
	Auto      bool              `json:"auto,omitempty"`
	CreatedAt int64             `json:"createdAt"`
	Origin    string            `json:"origin,omitempty"`
	Disk      *Manifest         `json:"disk,omitempty"`
	Memory    *Manifest         `json:"memory,omitempty"` // CRIU 镜像打包清单
	Kernel    string            `json:"kernel,omitempty"`
	Arch      string            `json:"arch,omitempty"`
	SizeBytes int64             `json:"sizeBytes"`
	Chunks    int               `json:"chunks"`
	Tags      map[string]string `json:"tags,omitempty"`
}

// Tree 是持久化的快照 DAG。分支是节点的属性；head 指针决定当前工作线。
// 恢复（inplace）把 head 移回旧节点并在其下挂新分支继续，
// 因此任何节点的多个 child 就是多条平行时间线。
type Tree struct {
	db *bolt.DB
}

// OpenTree 打开快照树数据库。
func OpenTree(dir string) (*Tree, error) {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return nil, err
	}
	db, err := bolt.Open(filepath.Join(dir, "tree.db"), 0o600, &bolt.Options{Timeout: 5 * time.Second})
	if err != nil {
		return nil, err
	}
	err = db.Update(func(tx *bolt.Tx) error {
		for _, name := range [][]byte{bktNodes, bktHeads, bktMeta} {
			if _, err := tx.CreateBucketIfNotExists(name); err != nil {
				return err
			}
		}
		return nil
	})
	if err != nil {
		db.Close()
		return nil, err
	}
	return &Tree{db: db}, nil
}

// Close 关闭数据库。
func (t *Tree) Close() error { return t.db.Close() }

// Add 插入节点。parent 为空表示根节点（genesis）。
func (t *Tree) Add(n *TreeNode) error {
	if n.ID == "" {
		n.ID = newSnapshotID()
	}
	if n.CreatedAt == 0 {
		n.CreatedAt = time.Now().UnixMilli()
	}
	if n.Branch == "" {
		n.Branch = "main"
	}
	if n.Kind == "" {
		n.Kind = KindDisk
	}
	return t.db.Update(func(tx *bolt.Tx) error {
		b := tx.Bucket(bktNodes)
		if n.Parent != "" && b.Get([]byte(n.Parent)) == nil {
			return fmt.Errorf("snap: 父节点不存在: %s", n.Parent)
		}
		raw, err := json.Marshal(n)
		if err != nil {
			return err
		}
		if err := b.Put([]byte(n.ID), raw); err != nil {
			return err
		}
		// 新节点成为其分支的 head。
		return tx.Bucket(bktHeads).Put([]byte(n.Branch), []byte(n.ID))
	})
}

// Get 读取节点。
func (t *Tree) Get(id string) (*TreeNode, error) {
	var n *TreeNode
	err := t.db.View(func(tx *bolt.Tx) error {
		raw := tx.Bucket(bktNodes).Get([]byte(id))
		if raw == nil {
			return fmt.Errorf("snap: 节点不存在: %s", id)
		}
		n = &TreeNode{}
		return json.Unmarshal(raw, n)
	})
	return n, err
}

// Children 返回某节点的所有子节点（按时间排序）。
func (t *Tree) Children(id string) ([]*TreeNode, error) {
	all, err := t.All()
	if err != nil {
		return nil, err
	}
	var out []*TreeNode
	for _, n := range all {
		if n.Parent == id {
			out = append(out, n)
		}
	}
	sort.Slice(out, func(i, j int) bool { return out[i].CreatedAt < out[j].CreatedAt })
	return out, nil
}

// All 返回全部节点。
func (t *Tree) All() ([]*TreeNode, error) {
	var out []*TreeNode
	err := t.db.View(func(tx *bolt.Tx) error {
		return tx.Bucket(bktNodes).ForEach(func(_, v []byte) error {
			n := &TreeNode{}
			if err := json.Unmarshal(v, n); err != nil {
				return err
			}
			out = append(out, n)
			return nil
		})
	})
	return out, err
}

// Head 返回当前分支与其 head 节点。
func (t *Tree) Head() (branch string, node *TreeNode, err error) {
	err = t.db.View(func(tx *bolt.Tx) error {
		if b := tx.Bucket(bktMeta).Get([]byte("currentBranch")); b != nil {
			branch = string(b)
		} else {
			branch = "main"
		}
		if h := tx.Bucket(bktHeads).Get([]byte(branch)); h != nil {
			raw := tx.Bucket(bktNodes).Get(h)
			if raw == nil {
				return fmt.Errorf("snap: head 指向不存在的节点")
			}
			node = &TreeNode{}
			return json.Unmarshal(raw, node)
		}
		return nil
	})
	return branch, node, err
}

// SetHead 显式移动某分支的 head。
func (t *Tree) SetHead(branch, nodeID string) error {
	return t.db.Update(func(tx *bolt.Tx) error {
		if tx.Bucket(bktNodes).Get([]byte(nodeID)) == nil {
			return fmt.Errorf("snap: 节点不存在: %s", nodeID)
		}
		return tx.Bucket(bktHeads).Put([]byte(branch), []byte(nodeID))
	})
}

// SetCurrentBranch 设置当前工作分支。
func (t *Tree) SetCurrentBranch(branch string) error {
	return t.db.Update(func(tx *bolt.Tx) error {
		return tx.Bucket(bktMeta).Put([]byte("currentBranch"), []byte(branch))
	})
}

// CurrentBranch 读取当前工作分支。
func (t *Tree) CurrentBranch() string {
	var b string
	t.db.View(func(tx *bolt.Tx) error {
		if v := tx.Bucket(bktMeta).Get([]byte("currentBranch")); v != nil {
			b = string(v)
		} else {
			b = "main"
		}
		return nil
	})
	return b
}

// Branches 列出全部分支名及其 head。
func (t *Tree) Branches() (map[string]string, error) {
	out := map[string]string{}
	err := t.db.View(func(tx *bolt.Tx) error {
		return tx.Bucket(bktHeads).ForEach(func(k, v []byte) error {
			out[string(k)] = string(v)
			return nil
		})
	})
	return out, err
}

// ForkBranch 在节点上创建新分支（head 指向该节点），返回分支名。
// 名字冲突时自动追加序号。
func (t *Tree) ForkBranch(nodeID, want string) (string, error) {
	if _, err := t.Get(nodeID); err != nil {
		return "", err
	}
	existing, err := t.Branches()
	if err != nil {
		return "", err
	}
	name := want
	for i := 2; ; i++ {
		if _, taken := existing[name]; !taken {
			break
		}
		name = fmt.Sprintf("%s-%d", want, i)
	}
	if err := t.SetHead(name, nodeID); err != nil {
		return "", err
	}
	return name, nil
}

// Delete 移除节点（要求无子节点），并返回是否清空了某分支 head。
func (t *Tree) Delete(id string) error {
	kids, err := t.Children(id)
	if err != nil {
		return err
	}
	if len(kids) > 0 {
		return fmt.Errorf("snap: 节点 %s 有 %d 个子节点，不能删除", id, len(kids))
	}
	n, err := t.Get(id)
	if err != nil {
		return err
	}
	return t.db.Update(func(tx *bolt.Tx) error {
		if err := tx.Bucket(bktNodes).Delete([]byte(id)); err != nil {
			return err
		}
		if h := tx.Bucket(bktHeads).Get([]byte(n.Branch)); string(h) == id {
			return tx.Bucket(bktHeads).Delete([]byte(n.Branch))
		}
		return nil
	})
}

// Reachable 返回从 root 可达的全部节点 id（GC 用）。
func (t *Tree) Reachable() (map[string]struct{}, error) {
	all, err := t.All()
	if err != nil {
		return nil, err
	}
	byID := map[string]*TreeNode{}
	for _, n := range all {
		byID[n.ID] = n
	}
	// 从所有无子节点的叶子反向标记（head/分支都只是指针，快照内容以节点为准）。
	// 更简单的定义：删除只允许删叶子（见 Delete），GC 范围 = 全部现存节点。
	reach := map[string]struct{}{}
	for id := range byID {
		reach[id] = struct{}{}
	}
	return reach, nil
}

func newSnapshotID() string { return "snap_" + wire.NewID() }
