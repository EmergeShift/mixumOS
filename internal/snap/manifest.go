package snap

import (
	"fmt"
	"os"
	"path/filepath"
	"sort"
	"syscall"
)

// FileEntry 描述清单中的一个文件。
type FileEntry struct {
	Path   string   `json:"path"`   // 相对根的路径
	Mode   uint32   `json:"mode"`   // 权限位与类型（go 的 FileMode 位）
	Size   int64    `json:"size"`
	Symlink string  `json:"symlink,omitempty"` // 符号链接目标（符号链接不占块）
	Chunks []string `json:"chunks"`            // 按序的块哈希
	Dev    uint64   `json:"dev,omitempty"`     // 设备文件（容器内基本不用，保留字段）
	Ino    uint64   `json:"ino,omitempty"`
}

// FileNode 描述一个目录。
type DirEntry struct {
	Path string `json:"path"`
	Mode uint32 `json:"mode"`
}

// Manifest 是一次磁盘快照的文件清单：目录结构 + 文件到块的映射。
type Manifest struct {
	Version int         `json:"version"`
	Files   []FileEntry `json:"files"`
	Dirs    []DirEntry  `json:"dirs"`
}

// PackStats 记录打包统计。
type PackStats struct {
	Files       int
	Chunks      int
	BytesLogical int64 // 明文总量
}

// PackDirectory 把 root 目录打包进 store，返回清单。
// skip 用于排除路径（相对 root，前缀匹配），例如块仓库自身。
func PackDirectory(root string, store *Store, skip []string) (*Manifest, *PackStats, error) {
	m := &Manifest{Version: 1}
	st := &PackStats{}
	skips := append([]string{}, skip...)

	err := filepath.WalkDir(root, func(p string, d os.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(root, p)
		if err != nil {
			return err
		}
		if rel == "." {
			return nil
		}
		rel = filepath.ToSlash(rel)
		for _, s := range skips {
			if rel == s || hasPrefixPath(rel, s) {
				if d.IsDir() {
					return filepath.SkipDir
				}
				return nil
			}
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		mode := uint32(info.Mode().Perm())
		switch {
		case d.IsDir():
			m.Dirs = append(m.Dirs, DirEntry{Path: rel, Mode: mode})
		case info.Mode()&os.ModeSymlink != 0:
			target, err := os.Readlink(p)
			if err != nil {
				return err
			}
			m.Files = append(m.Files, FileEntry{
				Path: rel, Mode: uint32(info.Mode()), Symlink: target,
			})
			st.Files++
		case info.Mode().IsRegular():
			fe, n, nc, err := packFile(p, store)
			if err != nil {
				return err
			}
			fe.Path = rel
			fe.Mode = uint32(info.Mode())
			fe.Size = n
			if st1, ok := info.Sys().(*syscall.Stat_t); ok {
				fe.Ino = st1.Ino
			}
			m.Files = append(m.Files, *fe)
			st.Files++
			st.Chunks += nc
			st.BytesLogical += n
		default:
			// 其他类型（socket/fifo/device）记录类型位但不打包内容。
			m.Files = append(m.Files, FileEntry{Path: rel, Mode: uint32(info.Mode())})
		}
		return nil
	})
	if err != nil {
		return nil, nil, err
	}
	sortManifest(m)
	return m, st, nil
}

func packFile(path string, store *Store) (*FileEntry, int64, int, error) {
	f, err := os.Open(path)
	if err != nil {
		return nil, 0, 0, err
	}
	defer f.Close()
	fe := &FileEntry{}
	var total int64
	nChunks := 0
	err = splitChunks(f, func(b []byte) error {
		h, err := store.Put(b)
		if err != nil {
			return err
		}
		fe.Chunks = append(fe.Chunks, h)
		total += int64(len(b))
		nChunks++
		return nil
	})
	if err != nil {
		return nil, 0, 0, err
	}
	return fe, total, nChunks, nil
}

// Materialize 把清单落地到 dst（要求 dst 为空或仅含将被覆盖的内容）。
// 返回落地的块数。采用「写临时 + rename」保证文件原子性。
func Materialize(m *Manifest, store *Store, dst string) (int, error) {
	// 先建目录（dirs 可能乱序，逐个 MkdirAll 兜底）
	for _, d := range m.Dirs {
		if err := os.MkdirAll(filepath.Join(dst, filepath.FromSlash(d.Path)), os.FileMode(d.Mode)&0o777); err != nil {
			return 0, err
		}
	}
	// 清掉 dst 下不属于清单的旧文件由调用方负责（Restore 会先清 upper）。
	count := 0
	for _, f := range m.Files {
		target := filepath.Join(dst, filepath.FromSlash(f.Path))
		mode := os.FileMode(f.Mode)
		switch {
		case mode&os.ModeSymlink != 0:
			_ = os.Remove(target)
			if err := os.Symlink(f.Symlink, target); err != nil {
				return count, err
			}
		case mode.IsRegular():
			if err := materializeFile(f, store, target); err != nil {
				return count, fmt.Errorf("snap: 落地 %s 失败: %w", f.Path, err)
			}
			count += len(f.Chunks)
		case mode&os.ModeDir != 0:
			if err := os.MkdirAll(target, mode&os.ModePerm); err != nil {
				return count, err
			}
		default:
			// 跳过非常规文件
		}
	}
	return count, nil
}

func materializeFile(f FileEntry, store *Store, target string) error {
	if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
		return err
	}
		tmp := target + ".mixum-restore"
		out, err := os.OpenFile(tmp, os.O_CREATE|os.O_TRUNC|os.O_WRONLY, os.FileMode(f.Mode)&0o777)
	if err != nil {
		return err
	}
	defer os.Remove(tmp)
	w := func(h string) error {
		b, err := store.Get(h)
		if err != nil {
			return err
		}
		_, err = out.Write(b)
		return err
	}
	for _, h := range f.Chunks {
		if err := w(h); err != nil {
			out.Close()
			return err
		}
	}
	if err := out.Close(); err != nil {
		return err
	}
	if err := os.Chmod(tmp, os.FileMode(f.Mode)&0o777); err != nil {
		return err
	}
	return os.Rename(tmp, target)
}

// ManifestChunks 去重列出清单引用的全部块哈希（按字典序，便于 have/need 对账）。
func ManifestChunks(m *Manifest) []string {
	set := map[string]struct{}{}
	for _, f := range m.Files {
		for _, h := range f.Chunks {
			set[h] = struct{}{}
		}
	}
	out := make([]string, 0, len(set))
	for h := range set {
		out = append(out, h)
	}
	sort.Strings(out)
	return out
}

// ManifestSize 汇总逻辑字节数。
func ManifestSize(m *Manifest) int64 {
	var n int64
	for _, f := range m.Files {
		n += f.Size
	}
	return n
}

func sortManifest(m *Manifest) {
	sort.Slice(m.Files, func(i, j int) bool { return m.Files[i].Path < m.Files[j].Path })
	sort.Slice(m.Dirs, func(i, j int) bool { return m.Dirs[i].Path < m.Dirs[j].Path })
}

func hasPrefixPath(rel, prefix string) bool {
	return len(rel) > len(prefix) && rel[:len(prefix)] == prefix && rel[len(prefix)] == '/'
}

// diffManifests 计算两个清单的文件差异。
func diffManifests(from, to *Manifest) (added, removed, changed []string, newChunks int) {
	fromMap := map[string]FileEntry{}
	for _, f := range from.Files {
		fromMap[f.Path] = f
	}
	toMap := map[string]FileEntry{}
	for _, f := range to.Files {
		toMap[f.Path] = f
	}
	chunkSet := map[string]struct{}{}
	for _, f := range to.Files {
		old, ok := fromMap[f.Path]
		if !ok {
			added = append(added, f.Path)
		} else if !sameChunks(old.Chunks, f.Chunks) {
			changed = append(changed, f.Path)
		}
		if !ok || !sameChunks(old.Chunks, f.Chunks) {
			for _, h := range f.Chunks {
				chunkSet[h] = struct{}{}
			}
		}
	}
	for _, f := range from.Files {
		if _, ok := toMap[f.Path]; !ok {
			removed = append(removed, f.Path)
		}
	}
	return added, removed, changed, len(chunkSet)
}

func sameChunks(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}
	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}
	return true
}
