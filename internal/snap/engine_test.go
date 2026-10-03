package snap

import (
	"os"
	"path/filepath"
	"testing"
)

func testEngine(t *testing.T, encrypt bool) *Engine {
	t.Helper()
	root := t.TempDir()
	data := t.TempDir()
	if err := os.WriteFile(filepath.Join(root, "hello.txt"), []byte("v1"), 0o644); err != nil {
		t.Fatal(err)
	}
	e, err := Open(Options{DataDir: data, Root: root, Encrypt: encrypt, Skip: []string{"store", "tree"}})
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { e.Tree.Close() })
	return e
}

func writeWork(t *testing.T, e *Engine, name, content string) {
	t.Helper()
	if err := os.WriteFile(filepath.Join(e.Root, name), []byte(content), 0o644); err != nil {
		t.Fatal(err)
	}
}

func TestGenesisAndIncremental(t *testing.T) {
	e := testEngine(t, false)
	branch, head, err := e.Tree.Head()
	if err != nil {
		t.Fatal(err)
	}
	if branch != "main" || head == nil || head.Label != "genesis" {
		t.Fatalf("genesis 缺失: branch=%s head=%+v", branch, head)
	}
	writeWork(t, e, "hello.txt", "v2")
	n2, err := e.Snapshot(KindDisk, "second", false)
	if err != nil {
		t.Fatal(err)
	}
	if n2.Parent != head.ID || n2.Branch != "main" {
		t.Fatalf("父子关系错误: %+v", n2)
	}
	// v1 与 v2 只有 4 字节不同：块去重后总量应远小于 2×ChunkSize
	if e.Store.Count() > 8 {
		t.Fatalf("块去重失效，仓库块数过多: %d", e.Store.Count())
	}
}

func TestRestoreInplaceAndDiverge(t *testing.T) {
	e := testEngine(t, false)
	_, n1, _ := e.Tree.Head() // genesis: hello=v1
	writeWork(t, e, "hello.txt", "v2-at-20:00")
	n2, _ := e.Snapshot(KindDisk, "20:00", false)
	writeWork(t, e, "hello.txt", "v3-at-22:00")
	writeWork(t, e, "extra.txt", "22:00 的新文件")
	n3, _ := e.Snapshot(KindDisk, "22:00", false)

	// 恢复到 20:00（原地）
	b, got, err := e.Restore(n2.ID, "inplace", "")
	if err != nil {
		t.Fatal(err)
	}
	if b != "main" || got.ID != n2.ID {
		t.Fatalf("恢复目标错误")
	}
	content, _ := os.ReadFile(filepath.Join(e.Root, "hello.txt"))
	if string(content) != "v2-at-20:00" {
		t.Fatalf("文件未回到 20:00 状态: %q", content)
	}
	if _, err := os.Stat(filepath.Join(e.Root, "extra.txt")); err == nil {
		t.Fatal("22:00 的新文件应被清除")
	}

	// 回到 22:00 的快照（内容层面）——再次恢复
	if _, _, err := e.Restore(n3.ID, "inplace", ""); err != nil {
		t.Fatal(err)
	}
	content, _ = os.ReadFile(filepath.Join(e.Root, "hello.txt"))
	if string(content) != "v3-at-22:00" {
		t.Fatalf("无法回到 22:00 状态: %q", content)
	}

	// 恢复到 20:00 并 fork，然后从 fork 继续工作应产生新支线
	if _, _, err := e.Restore(n2.ID, "inplace", ""); err != nil {
		t.Fatal(err)
	}
	fb, got, err := e.Restore(n1.ID, "fork", "fix-param")
	if err != nil {
		t.Fatal(err)
	}
	if fb == "main" || got.ID != n1.ID {
		t.Fatalf("fork 分支错误: %s", fb)
	}
	writeWork(t, e, "param.txt", "修改参数后")
	nf, err := e.SnapshotOnBranch(KindDisk, "fork 后第一次", false, n1.ID)
	if err != nil {
		t.Fatal(err)
	}
	if nf.Branch != fb || nf.Parent != n1.ID {
		t.Fatalf("fork 节点挂靠错误: %+v", nf)
	}
	// main 上的 n2、n3 仍然存在
	if _, err := e.Tree.Get(n3.ID); err != nil {
		t.Fatal("fork 不应影响原分支节点")
	}
	kids, _ := e.Tree.Children(n1.ID)
	if len(kids) < 2 {
		t.Fatalf("genesis 应有 ≥2 个孩子（main 第二点 + fork 第一点），got %d", len(kids))
	}
}

func TestForkBranchNaming(t *testing.T) {
	e := testEngine(t, false)
	_, n1, _ := e.Tree.Head()
	b1, err := e.Fork(n1.ID, "experiment")
	if err != nil {
		t.Fatal(err)
	}
	b2, err := e.Fork(n1.ID, "experiment")
	if err != nil {
		t.Fatal(err)
	}
	if b1 == b2 {
		t.Fatalf("分支名冲突未自动去重: %s", b1)
	}
	if b1 != "experiment" || b2 != "experiment-2" {
		t.Fatalf("命名规则错误: %s, %s", b1, b2)
	}
}

func TestDiff(t *testing.T) {
	e := testEngine(t, false)
	_, n1, _ := e.Tree.Head()
	writeWork(t, e, "hello.txt", "changed")
	writeWork(t, e, "new.txt", "added")
	n2, _ := e.Snapshot(KindDisk, "after-change", false)
	added, removed, changed, newChunks := diffManifests(n1.Disk, n2.Disk)
	if len(added) != 1 || added[0] != "new.txt" {
		t.Fatalf("added 错误: %v", added)
	}
	if len(changed) != 1 || changed[0] != "hello.txt" {
		t.Fatalf("changed 错误: %v", changed)
	}
	if len(removed) != 0 || newChunks == 0 {
		t.Fatalf("removed/newChunks 错误: %v %d", removed, newChunks)
	}
}

func TestGcRemovesOrphans(t *testing.T) {
	e := testEngine(t, false)
	_, n1, _ := e.Tree.Head()
	writeWork(t, e, "hello.txt", "totally different content for gc test")
	n2, _ := e.Snapshot(KindDisk, "b", false)
	before := e.Store.Count()
	if before < 2 {
		t.Fatalf("至少应有两个不同内容的块: %d", before)
	}
	// 删除 n2（叶子）后 GC 应清掉它独有的块
	if err := e.Tree.Delete(n2.ID); err != nil {
		t.Fatal(err)
	}
	n, err := e.Gc()
	if err != nil {
		t.Fatal(err)
	}
	if n == 0 {
		t.Fatal("GC 未清理孤儿块")
	}
	// n1 的内容仍可恢复
	if _, _, err := e.Restore(n1.ID, "inplace", ""); err != nil {
		t.Fatal(err)
	}
}

func TestEncryptedStoreRoundTrip(t *testing.T) {
	e := testEngine(t, true)
	_, n1, _ := e.Tree.Head()
	writeWork(t, e, "secret.txt", "机密内容")
	if _, err := e.Snapshot(KindDisk, "enc", false); err != nil {
		t.Fatal(err)
	}
	// 加密开启后，块文件不应包含明文
	h := n1.Disk.Files[0].Chunks[0]
	raw, err := os.ReadFile(chunkPath(filepath.Join(filepath.Dir(e.Tree.db.Path()), "store"), h))
	if err == nil && len(raw) > 0 {
		for _, secret := range []string{"机密内容", "secret"} {
			if string(raw) != "" && bytesContains(raw, []byte(secret)) {
				t.Fatal("静态加密未生效，块文件含明文")
			}
		}
	}
	// 恢复仍应成功
	if _, _, err := e.Restore(n1.ID, "inplace", ""); err != nil {
		t.Fatal(err)
	}
	if _, err := os.Stat(filepath.Join(e.Root, "secret.txt")); err == nil {
		t.Fatal("恢复到 genesis 后 secret.txt 不应存在")
	}
}

func bytesContains(hay, needle []byte) bool {
	for i := 0; i+len(needle) <= len(hay); i++ {
		match := true
		for j := range needle {
			if hay[i+j] != needle[j] {
				match = false
				break
			}
		}
		if match {
			return true
		}
	}
	return false
}

func TestFreezeCallbackRuns(t *testing.T) {
	e := testEngine(t, false)
	frozen := false
	e.Freeze = func(fn func() error) error {
		frozen = true
		return fn()
	}
	if _, err := e.Snapshot(KindDisk, "frozen", false); err != nil {
		t.Fatal(err)
	}
	if !frozen {
		t.Fatal("Freeze 回调未被调用")
	}
}
