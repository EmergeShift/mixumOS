package task

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
)

func marshalData(v any) (json.RawMessage, error) {
	if v == nil {
		return nil, nil
	}
	if raw, ok := v.(json.RawMessage); ok {
		return raw, nil
	}
	b, err := json.Marshal(v)
	if err != nil {
		return nil, err
	}
	return b, nil
}

// ---------- shell 驱动 ----------

// ShellDriver 把指令当作 shell 命令执行（params["cmd"] 优先）。
// 仅供确定性任务与 e2e 测试；真实 agent 走 external 驱动。
type ShellDriver struct{}

func (ShellDriver) Name() string { return "shell" }

func (ShellDriver) Run(ctx context.Context, t *Task, sb Sandbox, emit func(Event)) error {
	emit(Event{Type: "step", Step: "plan", Data: "shell 驱动：把指令作为命令执行"})
	cmd := t.Params["cmd"]
	if cmd == "" {
		cmd = t.Instruction
	}
	emit(Event{Type: "step", Step: "exec", Data: cmd})
	code, err := sb.Exec(ctx, "/bin/bash", []string{"-lc", cmd}, nil, "", func(line []byte) {
		emit(Event{Type: "log", Data: string(line)})
	})
	emit(Event{Type: "progress", Data: map[string]any{"exitCode": code}})
	if err != nil {
		return err
	}
	if code != 0 {
		return fmt.Errorf("shell 驱动：命令退出码 %d", code)
	}
	return nil
}

// ---------- external 驱动（外部 agent 接入点） ----------

// ExternalDriver 拉起一个外部 agent 进程，用行分隔 JSON 与其双向通信。
// 这是给「我们自己的 agent」预留的主接口：agent 桥协议见 docs/agent-bridge.md。
type ExternalDriver struct {
	// Command 是 agent 可执行文件；为空时从 params["agentCmd"] 取。
	Command string
}

func (ExternalDriver) Name() string { return "external" }

type agentMsg struct {
	Type     string            `json:"type"`
	Line     string            `json:"line,omitempty"`
	Pct      int               `json:"pct,omitempty"`
	Step     string            `json:"step,omitempty"`
	Argv     []string          `json:"argv,omitempty"`
	Error    string            `json:"error,omitempty"`
	Code     int               `json:"code,omitempty"`
	Params   map[string]string `json:"params,omitempty"`
}

func (d ExternalDriver) Run(ctx context.Context, t *Task, sb Sandbox, emit func(Event)) error {
	bin := d.Command
	if bin == "" {
		bin = t.Params["agentCmd"]
	}
	if bin == "" {
		return fmt.Errorf("external 驱动：未指定 agent 可执行文件（params.agentCmd）")
	}
	if _, err := os.Stat(bin); err != nil {
		return fmt.Errorf("external 驱动：%w", err)
	}
	emit(Event{Type: "step", Step: "spawn", Data: filepath.Base(bin)})

	cmd := exec.CommandContext(ctx, bin)
	cmd.Env = append(os.Environ(),
		"MIXUM_INSTRUCTION="+t.Instruction,
		"MIXUM_TASK_ID="+t.ID,
		"MIXUM_AGENT_BRIDGE=1",
	)
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return err
	}
	cmd.Stderr = cmd.Stdout // stderr 并入同一解析流
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("external 驱动：启动失败: %w", err)
	}
	if cmd.Process != nil {
		sb.SetTaskPid(cmd.Process.Pid)
	}

	hello, _ := json.Marshal(agentMsg{Type: "hello", Params: t.Params, Step: t.Instruction})
	fmt.Fprintln(stdin, string(hello))

	var (
		wg        sync.WaitGroup
		runErr    error
		finalErr  error
		writeMu   sync.Mutex
	)
	send := func(m agentMsg) {
		writeMu.Lock()
		defer writeMu.Unlock()
		b, _ := json.Marshal(m)
		fmt.Fprintln(stdin, string(b))
	}

	wg.Add(1)
	go func() {
		defer wg.Done()
		sc := bufio.NewScanner(stdout)
		sc.Buffer(make([]byte, 0, 64*1024), 8*1024*1024)
		for sc.Scan() {
			var m agentMsg
			if err := json.Unmarshal(sc.Bytes(), &m); err != nil {
				emit(Event{Type: "log", Data: sc.Text()})
				continue
			}
			switch m.Type {
			case "log":
				emit(Event{Type: "log", Data: m.Line})
			case "progress":
				emit(Event{Type: "progress", Data: map[string]any{"pct": m.Pct, "step": m.Step}})
			case "exec":
				code, err := sb.Exec(ctx, m.Argv[0], m.Argv[1:], nil, "", func(line []byte) {
					emit(Event{Type: "log", Data: string(line)})
				})
				resp := agentMsg{Type: "exec.done", Code: code}
				if err != nil {
					resp.Error = err.Error()
				}
				send(resp)
			case "done":
				// 正常结束
			case "fail":
				finalErr = fmt.Errorf("agent: %s", m.Error)
			}
		}
	}()

	wg.Wait()
	if err := cmd.Wait(); err != nil {
		runErr = err
	}
	if finalErr != nil {
		return finalErr
	}
	if ctx.Err() != nil {
		return ctx.Err()
	}
	if runErr != nil {
		return fmt.Errorf("external 驱动：agent 进程异常退出: %w", runErr)
	}
	emit(Event{Type: "progress", Data: map[string]any{"pct": 100}})
	return nil
}

// ---------- browser 驱动（最小可用：打开页面并保持窗口） ----------

// BrowserDriver 用容器内的 chromium 打开指令中的第一个 URL，
// 窗口经 mixcomp 流到主机，供用户实时观看与接管。自动化操作由后续
// agent 通过 exec/exec.done 或 CDP 扩展实现。
type BrowserDriver struct {
	BinPath string // 默认 /usr/bin/chromium
}

func (BrowserDriver) Name() string { return "browser" }

func (d BrowserDriver) Run(ctx context.Context, t *Task, sb Sandbox, emit func(Event)) error {
	url := t.Params["url"]
	if url == "" {
		url = firstURL(t.Instruction)
	}
	if url == "" {
		return fmt.Errorf("browser 驱动：指令与 params.url 中均未找到 URL")
	}
	bin := d.BinPath
	if bin == "" {
		bin = "/usr/bin/chromium"
	}
	emit(Event{Type: "step", Step: "launch", Data: url})
	args := []string{
		"--ozone-platform=wayland",
		"--wayland-display=wayland-1",
		"--window-size=1280,800",
		"--no-first-run", "--no-default-browser-check",
		"--disable-features=LensOverlay,Translate",
		url,
	}
	pid, err := sb.Spawn(ctx, bin, args, []string{"ELECTRON_OZONE_PLATFORM_HINT=wayland"}, "")
	if err != nil {
		return fmt.Errorf("browser 驱动：启动 chromium 失败: %w", err)
	}
	emit(Event{Type: "window", Data: map[string]any{"pid": pid, "url": url}})
	// 保持到任务被取消/完成信号。
	<-ctx.Done()
	if ctx.Err() == context.Canceled {
		return nil
	}
	return nil
}

func firstURL(s string) string {
	for _, f := range strings.Fields(s) {
		if strings.HasPrefix(f, "http://") || strings.HasPrefix(f, "https://") {
			return strings.Trim(f, `",'。;）)`)
		}
	}
	return ""
}
