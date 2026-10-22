# 任务驱动 SPI 接入指南（mixum-agent-driver）

通用化封装的最小故事：**你们的 agent 以依赖方式接入 mixumOS 的容器任务引擎**，
不改 mixumd 一行代码即可让自己的 agent 跑在"容器 + 窗口流 + 快照回退"的底座上。

- crate：`rust/crates/mixum-agent-driver`（独立发布,零内部依赖,依赖仅
  async-trait/serde/tokio）
- 可运行示例：`cargo run -p mixum-agent-driver --example hello_driver`
- SPI 全集：`AgentDriver` trait、`Task`/`TaskSpec`/`TaskView`、`Sandbox`
  trait、`EventEmitter`、`Registry`、`ShellDriver`

## 接入方式一：in-process（实现 trait,随 daemon 编译）

```rust
use mixum_agent_driver::{AgentDriver, DriverResult, EventEmitter, Registry, Sandbox, Task};
use std::sync::Arc;

struct MyAgent { /* 你的配置 */ }

#[async_trait::async_trait]
impl AgentDriver for MyAgent {
    fn name(&self) -> &str { "my-agent" }

    async fn run(&self, task: &Task, sandbox: &dyn Sandbox, emit: EventEmitter) -> DriverResult {
        emit.step("plan").await;                       // 步骤事件 → task.events
        let code = sandbox
            .exec(task, "/bin/sh",
                  &["-c".into(), task.instruction.clone()],
                  None, Arc::new(|line| { /* 可选行回调 */ }))
            .await?;                                   // 容器内执行
        emit.progress(100, Some("plan")).await;
        if code == 0 { Ok(()) } else { Err(format!("退出码 {code}").into()) }
    }
}

// 装配(见 mixum-agentd 的 main.rs):Registry 注册后交给任务引擎
let mut registry = Registry::new();
registry.register(MyAgent)?;
```

之后任何客户端 `task.submit { instruction, agent: "my-agent" }` 都会路由到
你的驱动；窗口事件（`emit.window_opened`）经显示 broker 自动建窗口流。

## 接入方式二：子进程桥接（不改代码）

agent 独立成进程，按 `docs/agent-bridge.md` 的桥协议读写 stdio
（hello/log/progress/exec/fail/done），由内建 `external`/命名 external
驱动经 `agents` 配置拉起。适合 agent 已有独立发布物的场景。

## 驱动能拿到什么

| 能力 | 入口 | 说明 |
|------|------|------|
| 任务上下文 | `task.instruction/params/agent` | `task.submit` 的载荷 |
| 容器执行 | `sandbox.exec` | 逐行 stdout/stderr 回调,返回退出码 |
| GUI 进程 | `sandbox.spawn_gui` | 长驻进程,窗口流由 mixcomp 捕获 |
| 进程组管理 | `sandbox.set_task_pid/signal` | 冻结/终止对齐快照引擎 |
| 事件上报 | `emit.log/step/progress/window_opened` | task.events 流 |
| 快照钩子 | `sandbox.snapshot` | 缺省空实现,mixumd 装配真实引擎 |

状态机（queued/running/completed/failed/canceled）与取消、失败回退
（`snapshot_policy.on_failure=restore`）归任务引擎，驱动只管 `run`。

## 运行边界

- 驱动跑在 mixumd 进程内（Linux 容器环境）；`Sandbox` 实现决定隔离级别。
- SPI crate 自身平台无关；内建 `LocalSandbox`（进程组/nix）在 mixum-agentd。
- 版本策略：SPI 以 0.x 演进，破坏性变更随 minor 版本公告。

## 一键接入：`mixum-ctl agent`

`mixum-ctl` 内置四个常见 CLI agent 的安装配方（claude-code / codex /
gemini-cli / opencode），安装通过容器内 shell 任务执行，不需要在宿主
装任何东西：

```bash
mixum-ctl agent list                 # 目录 + 容器内是否已安装
mixum-ctl agent install claude-code  # 补 npm → 官方包安装 → 验证可执行
mixum-ctl agent template codex       # 打印可复制到任意 agent 的提示词
```

- 安装脚本按 `pacman`/`apk`/`apt-get` 自动补依赖，再把提示词模板写到
  `~/.mixum/agents/<name>/PROMPT.md`；
- `--dry-run` 只打印将执行的脚本；`--cmd '<命令>'` 可替换安装命令
  （私有镜像源或测试），提示词仍会写入；
- 安装失败以非零退出码返回，容器内日志实时回显（`task.event` 的 log）。
