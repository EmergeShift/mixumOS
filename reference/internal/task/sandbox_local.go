package task

import (
	"bufio"
	"context"
	"fmt"
	"io"
	"os"
	"os/exec"
	"sync"
	"syscall"
)

// LocalSandbox 直接在本机（容器内）执行进程。mixumd 本来就跑在容器里，
// 所以这里就是真实的执行环境。
type LocalSandbox struct {
	setPid func(int) // 注册任务首领进程（内存快照目标）

	mu    sync.Mutex
	pgids map[int]struct{} // 全部活跃任务进程组
}

// NewLocalSandbox 构造；setPid 由引擎传入（快照引擎 SetTaskPid）。
func NewLocalSandbox(setPid func(int)) *LocalSandbox {
	return &LocalSandbox{pgids: map[int]struct{}{}, setPid: setPid}
}

func (s *LocalSandbox) Exec(ctx context.Context, name string, args []string, env []string, dir string, onOut func(line []byte)) (int, error) {
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if len(env) > 0 {
		cmd.Env = append(os.Environ(), env...)
	}
	if dir != "" {
		cmd.Dir = dir
	}
	s.trackPgid(cmd, true)

	pipe, err := cmd.StdoutPipe()
	if err != nil {
		return -1, err
	}
	cmd.Stderr = cmd.Stdout
	if err := cmd.Start(); err != nil {
		s.trackPgid(cmd, false)
		return -1, err
	}
	// 注册首领进程:任务运行中的 hybrid 快照才知道 dump 谁(短命令结束时
	// 引擎按 pid=0 退化为 disk)。
	s.SetTaskPid(cmd.Process.Pid)
	// setpgid 后进程组 id 等于子进程 pid
	wg := sync.WaitGroup{}
	wg.Add(1)
	go func() {
		defer wg.Done()
		sc := bufio.NewScanner(pipe)
		sc.Buffer(make([]byte, 0, 64*1024), 4*1024*1024)
		for sc.Scan() {
			if onOut != nil {
				onOut(append([]byte(nil), sc.Bytes()...))
			}
		}
	}()
	err = cmd.Wait()
	wg.Wait()
	s.trackPgid(cmd, false)
	if err != nil {
		if ee, ok := err.(*exec.ExitError); ok {
			return ee.ExitCode(), nil // 正常的非零退出
		}
		return -1, err
	}
	return 0, nil
}

func (s *LocalSandbox) Spawn(ctx context.Context, name string, args []string, env []string, dir string) (int, error) {
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	if len(env) > 0 {
		cmd.Env = append(os.Environ(), env...)
	}
	if dir != "" {
		cmd.Dir = dir
	}
	devnull, err := os.OpenFile(os.DevNull, os.O_RDWR, 0)
	if err == nil {
		cmd.Stdin = devnull
		cmd.Stdout = io.MultiWriter(devnull)
		cmd.Stderr = devnull
		defer devnull.Close()
	}
	if err := cmd.Start(); err != nil {
		return -1, err
	}
	s.trackPgid(cmd, true)
	if s.setPid != nil {
		s.setPid(cmd.Process.Pid)
	}
	go func() {
		<-ctx.Done()
		s.trackPgid(cmd, false)
	}()
	return cmd.Process.Pid, nil
}

func (s *LocalSandbox) SetTaskPid(pid int) {
	if s.setPid != nil {
		s.setPid(pid)
	}
}

// Signal 向全部活跃任务进程组发信号（快照冻结/任务取消共用）。
func (s *LocalSandbox) Signal(sig string) error {
	num, ok := map[string]syscall.Signal{
		"TERM": syscall.SIGTERM, "KILL": syscall.SIGKILL,
		"STOP": syscall.SIGSTOP, "CONT": syscall.SIGCONT,
	}[sig]
	if !ok {
		return fmt.Errorf("task: 未知信号 %q", sig)
	}
	s.mu.Lock()
	pgids := make([]int, 0, len(s.pgids))
	for p := range s.pgids {
		pgids = append(pgids, p)
	}
	s.mu.Unlock()
	var firstErr error
	for _, p := range pgids {
		if err := syscall.Kill(-p, num); err != nil && firstErr == nil {
			firstErr = err
		}
	}
	return firstErr
}

// FreezeAll 冻结全部任务进程组，执行 fn 后解冻——快照引擎的 FreezeFunc。
func (s *LocalSandbox) FreezeAll(fn func() error) error {
	s.mu.Lock()
	pgids := make([]int, 0, len(s.pgids))
	for p := range s.pgids {
		pgids = append(pgids, p)
	}
	s.mu.Unlock()
	for _, p := range pgids {
		_ = syscall.Kill(-p, syscall.SIGSTOP)
	}
	defer func() {
		for _, p := range pgids {
			_ = syscall.Kill(-p, syscall.SIGCONT)
		}
	}()
	return fn()
}

func (s *LocalSandbox) trackPgid(cmd *exec.Cmd, add bool) {
	if cmd.Process == nil {
		return
	}
	pgid := int(cmd.Process.Pid)
	s.mu.Lock()
	defer s.mu.Unlock()
	if add {
		s.pgids[pgid] = struct{}{}
	} else {
		delete(s.pgids, pgid)
	}
}
