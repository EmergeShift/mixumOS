package snap

import (
	"runtime"
	"syscall"
)

// 文件系统魔数（statfs f_type）。
const (
	magicBtrfs   = 0x9123683E
	magicOverlay = 0x794C7630 // OVERLAYFS_SUPER_MAGIC
	magicTmpfs   = 0x01021994
	magicExt4    = 0xEF53
	magicXfs     = 0x58465342
)

// BackendName 探测 Root 所在文件系统，报告快照后端：
// btrfs（原生 COW，秒级）、overlayfs、其他（通用目录后端，走块打包）。
func (e *Engine) BackendName() string {
	if runtime.GOOS != "linux" {
		return "directory"
	}
	var st syscall.Statfs_t
	if err := syscall.Statfs(e.Root, &st); err != nil {
		return "directory"
	}
	switch uint32(st.Type) {
	case magicBtrfs:
		return "btrfs"
	case magicOverlay:
		return "overlayfs"
	case magicExt4, magicXfs, magicTmpfs:
		return "directory-reflink"
	}
	return "directory"
}
