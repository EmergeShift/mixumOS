// mixumd 是 mixumOS 的容器内核心守护进程：
// 任务引擎（agent runner）、快照引擎（git 式树）、窗口流中枢（对接 mixcomp）、
// 以及对主机/云端/移动端的 WSS 服务。
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"log/slog"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"github.com/mixumos/mixumos/internal/auth"
	"github.com/mixumos/mixumos/internal/display"
	"github.com/mixumos/mixumos/internal/server"
	"github.com/mixumos/mixumos/internal/snap"
	"github.com/mixumos/mixumos/internal/task"
	"github.com/mixumos/mixumos/internal/wire"
)

type config struct {
	Listen   string `json:"listen"`
	DataDir  string `json:"dataDir"`
	WorkRoot string `json:"workRoot"`
	Hostname string `json:"hostname"`
	Encrypt  bool   `json:"encryptSnapshots"`
	TokenTTL int    `json:"tokenTtlSec"`

	// agent 注册：external 驱动的可执行文件列表
	Agents []struct {
		Name    string `json:"name"`
		Command string `json:"command"`
	} `json:"agents"`
}

func defaultConfig() config {
	return config{
		Listen:   "0.0.0.0:7601",
		DataDir:  "/var/lib/mixum",
		WorkRoot: "/var/lib/mixum/work",
		Hostname: "",
		Encrypt:  true,
		TokenTTL: 86400,
	}
}

func main() {
	cfgPath := flag.String("config", "/etc/mixum/mixumd.json", "配置文件路径")
	flag.Parse()
	log := slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo}))

	cfg := defaultConfig()
	b, readErr := os.ReadFile(*cfgPath)
	if readErr == nil && len(b) > 0 {
		if err := json.Unmarshal(b, &cfg); err != nil {
			log.Error("配置文件解析失败", "err", err)
			os.Exit(1)
		}
	} else if readErr != nil && *cfgPath != "/dev/null" {
		log.Warn("配置文件不存在，使用默认配置", "path", *cfgPath)
	}
	if cfg.Hostname == "" {
		cfg.Hostname, _ = os.Hostname()
	}

	if err := run(cfg, log); err != nil {
		log.Error("mixumd 启动失败", "err", err)
		os.Exit(1)
	}
}

func run(cfg config, log *slog.Logger) error {
	// 1. 身份：令牌签名密钥 + TLS 证书（自签，指纹供主机端 TOFU 固定）。
	hosts := []string{"localhost", "127.0.0.1", "::1", cfg.Hostname}
	identity, err := auth.LoadOrCreate(cfg.DataDir, hosts)
	if err != nil {
		return fmt.Errorf("身份初始化失败: %w", err)
	}
	log.Info("身份就绪", "fingerprint", identity.Fingerprint)

	// 2. 快照引擎（git 式树 + 内容寻址块存储 + CRIU）。
	if err := os.MkdirAll(cfg.WorkRoot, 0o755); err != nil {
		return err
	}
	snapEngine, err := snap.Open(snap.Options{
		DataDir: filepath.Join(cfg.DataDir, "snapshots"),
		Root:    cfg.WorkRoot,
		Encrypt: cfg.Encrypt,
		Skip:    []string{".mixum"},
	})
	if err != nil {
		return fmt.Errorf("快照引擎初始化失败: %w", err)
	}
	log.Info("快照引擎就绪", "backend", snapEngine.BackendName(),
		"criu", snapEngine.Criu.Available())

	// 3. 任务引擎与驱动注册。
	sandbox := task.NewLocalSandbox(snapEngine.SetTaskPid)
	persist, err := task.OpenPersist(filepath.Join(cfg.DataDir, "tasks"))
	if err != nil {
		return err
	}
	reg := task.NewRegistry()
	_ = reg.Register(task.ShellDriver{})
	_ = reg.Register(task.ExternalDriver{})
	_ = reg.Register(task.BrowserDriver{})
	for _, a := range cfg.Agents {
		if err := reg.Register(task.ExternalDriver{Command: a.Command}); err != nil {
			log.Warn("自定义 agent 注册失败", "name", a.Name, "err", err)
			continue
		}
		log.Info("已注册 agent", "name", a.Name, "command", a.Command)
	}
	tasks := task.NewEngine(reg, sandbox, persist)

	// 4. 窗口流中枢（等待 mixcomp 连入）。
	broker := display.NewBroker("/run/mixum/display.sock")
	if err := broker.Start(); err != nil {
		log.Warn("display 套接字启动失败（窗口流不可用）", "err", err)
	}

	// 5. 快照冻结回调：冻结全部任务进程组，快照点即冻结点（无感）。
	snapEngine.Freeze = sandbox.FreezeAll

	// 6. WSS 服务。
	srv := server.New(server.Config{
		Listen:   cfg.Listen,
		DataDir:  cfg.DataDir,
		WorkRoot: cfg.WorkRoot,
		Hostname: cfg.Hostname,
		TokenTTL: cfg.TokenTTL,
	}, log, identity, tasks, broker, snapEngine)
	if err := srv.Start(); err != nil {
		return err
	}

	// 7. bootstrap 管理令牌（首次生成，后续复用）。
	adminTokenPath := filepath.Join(cfg.DataDir, "admin-token")
	if _, err := os.Stat(adminTokenPath); err != nil {
		tok, err := srv.MintToken("bootstrap-admin", []wire.Cap{
			wire.CapAdmin,
		}, 10*365*24*time.Hour)
		if err != nil {
			return err
		}
		if err := os.WriteFile(adminTokenPath, []byte(tok), 0o600); err != nil {
			return err
		}
		log.Info("已生成 bootstrap 管理令牌", "path", adminTokenPath)
	}

	log.Info("mixumd 已启动",
		"listen", cfg.Listen,
		"fingerprint", identity.Fingerprint,
		"drivers", reg.Names(),
		"compositor", broker.Online())

	sig := make(chan os.Signal, 1)
	signal.Notify(sig, syscall.SIGINT, syscall.SIGTERM)
	<-sig
	log.Info("正在关闭")
	srv.Stop()
	persist.Close()
	return snapEngine.Tree.Close()
}
