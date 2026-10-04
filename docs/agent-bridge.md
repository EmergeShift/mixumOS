# Agent 接入指南(agent-bridge)

> mixumOS 的任务执行器是开放的:我们自己的 agent 以 crate 嵌入(一等公民),
> 任何外部 agent 走 external 桥协议(行分隔 JSON)。本文档是两种方式的规范。

## 方式 A:crate 嵌入(推荐,我们自己的 agent)

```rust
// 你们的 agent 仓库
use mixum_agentd::task::{AgentDriver, Task, Sandbox, EventEmitter};

struct MyAgent { /* 模型句柄、工具集、配置 */ }

#[async_trait]
impl AgentDriver for MyAgent {
    fn name(&self) -> &str { "my-agent" }

    async fn run(&self, task: &Task, sb: &dyn Sandbox, emit: EventEmitter) -> Result<()> {
        emit.step("plan").await;
        let plan = self.plan(&task.instruction).await?;

        for step in plan {
            emit.progress(&step.desc).await;
            // 沙箱执行:进程组托管、输出流式上抛、可被冻结/接管
            let code = sb.exec("bash", ["-lc", &step.cmd]).await?;
            if code != 0 { return Err(anyhow!("step failed: {step.desc}")); }

            // 打开 GUI:容器内起 wayland 应用,窗口自动流给用户
            if let Some(url) = &step.open_url {
                sb.spawn_gui("chromium", ["--ozone-platform=wayland", url]).await?;
                emit.window_opened(url).await;   // 控制台自动附流
            }

            // 快照:关键节点打显式保存点(树里可见)
            sb.snapshot(format!("after-{}", step.name)).await?;
        }
        Ok(())
    }
}

// 注册(容器启动时)
registry.register(MyAgent::new(cfg))?;
```

契约要点:

- **Sandbox** 是唯一的世界入口:exec/spawn_gui/snapshot/signal。
  agent 不直接碰进程 API,否则快照冻结、接管、stdio 流全部失效;
- **事件**是唯一对外的嘴:log/step/progress/window 全走 emit,
  runner 负责扇出到所有订阅连接;
- driver 可以是无状态单例;任务隔离靠 runner 侧的任务对象。

## 方式 B:external 桥(第三方 agent,任何语言)

配置 `agents: [{name: "xxx", command: "/path/to/agent"}]`,
runner 以子进程拉起,stdin/stdout 行分隔 JSON:

### runner → agent

```json
{"type":"hello","instruction":"打开 b 站搜 mixumOS","params":{}}
{"type":"exec.done","code":0,"error":""}
```

### agent → runner

```json
{"type":"log","line":"正在打开浏览器"}
{"type":"progress","pct":30,"step":"browse"}
{"type":"exec","argv":["bash","-lc","echo hi"]}
{"type":"done"}
{"type":"fail","error":"页面超时"}
```

- `exec`:agent 请求执行命令,runner 在沙箱执行后回 `exec.done`;
  这是外部 agent 操作容器的唯一合法通道(必须过沙箱,理由同上);
- stdout 里非 JSON 行按日志处理,不会压垮协议;
- agent 进程退出码 0 且未发 `fail` 即任务成功;
- 生命周期:runner 取消任务 = 关 stdin + SIGTERM 进程组;
- 环境变量:`MIXUM_INSTRUCTION` `MIXUM_TASK_ID` `MIXUM_AGENT_BRIDGE=1`。

## 显示与接管的协作

- 任务跑 GUI 应用后,runner 把窗口置为 `agent` 模式:用户看得到,
  但注入被拒绝;agent 通过 Sandbox 的输入 API(嵌入方式)或
  `exec`(xdotool 类工具,阶段 4 提供 mixcomp 原生输入 API)操作;
- 用户 `display.takeover claim` 后窗口转 `user`,agent 的后续注入
  由 runner 缓存或拒绝(driver 可选实现 Suspender,收到暂停时挂起);
- 任务结束(completed/canceled),窗口全部回 `free`。

## Go 参考实现对照

external 桥协议的行为规范在 `reference/internal/task/drivers.go`
(ExternalDriver),测试见 `reference/internal/task/`。
Rust 版嵌入接口以 `mixum-agentd` crate 的 trait 为准(阶段 2 定稿,
本文件同步更新签名)。
