> English translation of the Chinese document. Original: [agent-bridge.zh-CN.md](agent-bridge.zh-CN.md). Technical review pending.

# Agent integration guide (agent-bridge)

> mixumOS's task runner is open: our own agents embed as a crate (first-class),
> while any external agent uses the external bridge protocol (line-delimited JSON).
> This page specifies both.

## Option A: crate embedding (recommended; our own agents)

```rust
// in your agent repository
use mixum_agentd::task::{AgentDriver, Task, Sandbox, EventEmitter};

struct MyAgent { /* model handle, tool set, config */ }

#[async_trait]
impl AgentDriver for MyAgent {
    fn name(&self) -> &str { "my-agent" }

    async fn run(&self, task: &Task, sb: &dyn Sandbox, emit: EventEmitter) -> Result<()> {
        emit.step("plan").await;
        let plan = self.plan(&task.instruction).await?;

        for step in plan {
            emit.progress(&step.desc).await;
            // Sandbox execution: managed process group, streamed output, freezable/takeover-able
            let code = sb.exec("bash", ["-lc", &step.cmd]).await?;
            if code != 0 { return Err(anyhow!("step failed: {step.desc}")); }

            // Open a GUI: start a Wayland app in the container; the window is streamed to the user
            if let Some(url) = &step.open_url {
                sb.spawn_gui("chromium", ["--ozone-platform=wayland", url]).await?;
                emit.window_opened(url).await;   // the console attaches the stream automatically
            }

            // Snapshot: explicit save point at a key node (visible in the tree)
            sb.snapshot(format!("after-{}", step.name)).await?;
        }
        Ok(())
    }
}

// Register at container startup
registry.register(MyAgent::new(cfg))?;
```

Contract points:

- **Sandbox** is the only door into the world: exec/spawn_gui/snapshot/signal. The
  agent must not call process APIs directly, or snapshot freeze, takeover and stdio
  streaming all break.
- **Events** are the only outward voice: log/step/progress/window all go through
  emit; the runner fans them out to every subscribed connection.
- A driver may be a stateless singleton; task isolation comes from the runner-side
  task object.

## Option B: external bridge (third-party agents, any language)

Configure `agents: [{name: "xxx", command: "/path/to/agent"}]`; the runner starts it
as a child process and speaks line-delimited JSON over stdin/stdout:

### runner → agent

```json
{"type":"hello","instruction":"打开 b 站搜 mixumOS","params":{},"env":{"arch":"aarch64","kernel":"6.18.35","workspace":"/workspace"}}
{"type":"exec.done","id":"e1","code":0}
{"type":"input.response","id":"q1","value":"yes"}
{"type":"suspend"}
{"type":"resume"}
```

### agent → runner

```json
{"type":"log","line":"正在打开浏览器"}
{"type":"progress","pct":30,"step":"browse"}
{"type":"exec","id":"e1","argv":["bash","-lc","echo hi"],"timeoutMs":60000}
{"type":"input.request","id":"q1","prompt":"继续吗?","choices":["yes","no"],"timeoutMs":120000}
{"type":"done"}
{"type":"fail","error":"页面超时"}
```

- `exec`: the agent asks the runner to execute a command; the runner runs it in the
  sandbox and replies `exec.done`. This is the only legal channel for an external
  agent to operate the container (it must go through the sandbox, same reason as
  above). `timeoutMs` defaults to 60 s; on timeout the process group is killed and
  the reply is `code: null, error: "timeout"`.
- `input.request`: the human-input channel. The runner broadcasts
  `task.event {type:"input"}` to observers; after the client's `task.reply` the
  runner returns `input.response {id,value}`. The same id is answered once; on
  timeout the reply is `input.response {id,value:"",error:"timeout"}`.
- Non-JSON stdout lines are treated as logs and cannot break the protocol.
- The task succeeds when the agent exits 0 without sending `fail` (`fail` wins over
  the exit code).
- Lifecycle: runner cancellation = close stdin + SIGTERM the process group.
- Environment variables (canonical): `MAP_INSTRUCTION` `MAP_TASK_ID` `MAP_AGENT_NAME`
  `MAP_BRIDGE=1`; during the transition the old names `MIXUM_INSTRUCTION`
  `MIXUM_TASK_ID` `MIXUM_AGENT_BRIDGE=1` are also set, so adapters can keep reading them.

## Display and takeover cooperation

- When a task runs a GUI app, the runner puts the window in `agent` mode: the user
  can see it, but injection is refused. The agent operates through the Sandbox input
  API (embedded mode) or `exec` (xdotool-like tools; a native mixcomp input API is
  stage 4).
- After the user runs `display.takeover claim`, the window becomes `user`; the
  agent's later injections are buffered or refused by the runner (a driver may
  implement Suspender and pause on the suspend event).
- When the task ends (completed/canceled), all windows return to `free`.

## Go reference implementation (removed)

The external bridge behavior used to be specified in the Go reference
implementation's `drivers.go` (`ExternalDriver`); it was removed with that
implementation. The Rust `mixum-agentd` implementation and its tests now carry the
behavior, and the embedded interface is the trait in the `mixum-agentd` crate.
