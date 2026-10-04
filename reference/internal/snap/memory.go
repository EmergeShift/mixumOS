package snap

import (
	"fmt"
	"os"
	"os/exec"
	"runtime"
)

// Criu 封装 CRIU 内存快照。内存快照把容器内任务进程树的完整状态
// （内存页、fd、信号处理器、TCP 连接等）转储为镜像文件，恢复时原地还原。
// 前置条件记录在节点元数据（内核版本、架构），恢复时校验以避免跨内核崩溃。
type Criu struct {
	bin string
}

// NewCriu 探测 CRIU 可用性。
func NewCriu() *Criu {
	for _, b := range []string{"criu", "/usr/sbin/criu", "/usr/bin/criu"} {
		if p, err := exec.LookPath(b); err == nil {
			return &Criu{bin: p}
		}
	}
	return nil
}

// Available 报告 CRIU 是否可用。
func (c *Criu) Available() bool { return c != nil }

// Dump 转储 pid 指向的进程树到 dir，返回镜像目录。
// leaveRunning 让目标进程转储后继续运行（CRIU --leave-running，用于在线快照）。
func (c *Criu) Dump(pid int, dir string, leaveRunning bool) error {
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return err
	}
	args := []string{"dump", "--tree", fmt.Sprint(pid), "--images-dir", dir, "-v4", "-o", "dump.log"}
	if leaveRunning {
		args = append(args, "--leave-running")
	}
	cmd := exec.Command(c.bin, args...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("snap: CRIU dump 失败: %w：%s", err, tail(out, 800))
	}
	return nil
}

// Restore 从镜像目录恢复进程树。恢复成功后原镜像默认被 CRIU 销毁。
func (c *Criu) Restore(dir string) error {
	args := []string{"restore", "--images-dir", dir, "-v4", "-o", "restore.log", "--shell-job"}
	cmd := exec.Command(c.bin, args...)
	out, err := cmd.CombinedOutput()
	if err != nil {
		return fmt.Errorf("snap: CRIU restore 失败: %w：%s", err, tail(out, 800))
	}
	return nil
}

// EnvFingerprint 返回内存快照的兼容性指纹：恢复时必须一致。
func EnvFingerprint() string {
	return runtime.GOARCH + "/" + kernelRelease()
}

func kernelRelease() string {
	b, err := os.ReadFile("/proc/sys/kernel/osrelease")
	if err != nil {
		return "unknown"
	}
	return string(trimSpaceBytes(b))
}

func trimSpaceBytes(b []byte) []byte {
	start, end := 0, len(b)
	for start < end && (b[start] == '\n' || b[start] == ' ' || b[start] == '\t') {
		start++
	}
	for end > start && (b[end-1] == '\n' || b[end-1] == ' ' || b[end-1] == '\t') {
		end--
	}
	return b[start:end]
}

func tail(b []byte, n int) string {
	if len(b) > n {
		b = b[len(b)-n:]
	}
	return string(b)
}

// PackCriuImages 把 CRIU 镜像目录打包进 store（作为 memory 清单）。
func PackCriuImages(imagesDir string, store *Store) (*Manifest, *PackStats, error) {
	// CRIU 镜像都是普通文件，直接复用目录打包。
	return PackDirectory(imagesDir, store, nil)
}
