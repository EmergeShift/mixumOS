> English translation of the Chinese document. Original: [agent-driver.zh-CN.md](agent-driver.zh-CN.md). Technical review pending.

# Task-Driven SPI Integration Guide (mixum-agent-driver)

The shortest story for the generalized wrapper: **your agent integrates with the
mixumOS container task engine as a dependency**, and without changing a single
line of mixumd code, your agent runs on the foundation of "container + window
stream + snapshot rollback".

- crate: `rust/crates/mixum-agent-driver` (published independently, zero internal
  dependencies; the only dependencies are async-trait/serde/tokio)
- Runnable example: `cargo run -p mixum-agent-driver --example hello_driver`
- Full SPI: the `AgentDriver` trait, `Task`/`TaskSpec`/`TaskView`, the `Sandbox`
  trait, `EventEmitter`, `Registry`, `ShellDriver`

## Integration approach 1: in-process (implement the trait, compile with the daemon)

```rust
use mixum_agent_driver::{AgentDriver, DriverResult, EventEmitter, Registry, Sandbox, Task};
use std::sync::Arc;

struct MyAgent { /* your configuration */ }

#[async_trait::async_trait]
impl AgentDriver for MyAgent {
    fn name(&self) -> &str { "my-agent" }

    async fn run(&self, task: &Task, sandbox: &dyn Sandbox, emit: EventEmitter) -> DriverResult {
        emit.step("plan").await;                       // step event → task.events
        let code = sandbox
            .exec(task, "/bin/sh",
                  &["-c".into(), task.instruction.clone()],
                  None, Arc::new(|line| { /* optional per-line callback */ }))
            .await?;                                   // execute inside the container
        emit.progress(100, Some("plan")).await;
        if code == 0 { Ok(()) } else { Err(format!("exit code {code}").into()) }
    }
}

// Assembly (see main.rs in mixum-agentd): register with the Registry, then hand it to the task engine
let mut registry = Registry::new();
registry.register(MyAgent)?;
```

After that, any client's `task.submit { instruction, agent: "my-agent" }` is
routed to your driver; window events (`emit.window_opened`) automatically create
a window stream through the display broker.

## Integration approach 2: subprocess bridge (no code changes)

The agent becomes its own process and reads/writes stdio following the bridge
protocol in `docs/agent-bridge.md` (hello/log/progress/exec/fail/done); it is
launched by the built-in `external` / named external driver via the `agents`
configuration. This suits cases where the agent already has an independent
release artifact.

## What the driver gets

| Capability | Entry point | Description |
|------|------|------|
| Task context | `task.instruction/params/agent` | Payload of `task.submit` |
| Container execution | `sandbox.exec` | Per-line stdout/stderr callback, returns the exit code |
| GUI process | `sandbox.spawn_gui` | Long-running process; the window stream is captured by mixcomp |
| Process group management | `sandbox.set_task_pid/signal` | Freeze/terminate, aligned with the snapshot engine |
| Event reporting | `emit.log/step/progress/window_opened` | task.events stream |
| Snapshot hook | `sandbox.snapshot` | Empty no-op by default; mixumd wires in the real engine |

The state machine (queued/running/completed/failed/canceled), cancellation, and
failure rollback (`snapshot_policy.on_failure=restore`) belong to the task
engine; the driver is only responsible for `run`.

## Runtime boundaries

- The driver runs inside the mixumd process (Linux container environment); the
  `Sandbox` implementation determines the isolation level.
- The SPI crate itself is platform-independent; the built-in `LocalSandbox`
  (process group / nix) lives in mixum-agentd.
- Versioning policy: the SPI evolves in 0.x; breaking changes are announced with
  minor releases.

## One-step integration: `mixum-ctl agent`

`mixum-ctl` ships install recipes for four common CLI agents (claude-code / codex
/ gemini-cli / opencode). Installation runs as a shell task inside the
container; nothing needs to be installed on the host:

```bash
mixum-ctl agent list                 # catalog + whether it is already installed in the container
mixum-ctl agent install claude-code  # install npm if missing → official package install → verify executable
mixum-ctl agent template codex       # print a prompt that can be copied into any agent
```

- By default it runs as the **task user** (usually `mixum`, not root): npm
  packages are installed to `$HOME/.local`, and `$HOME/.local/bin` and
  `$HOME/.opencode/bin` are added to PATH; system directories are never written;
- If the container lacks `npm`/`curl` and the current user is not root, the
  installation fails with an explicit error and asks the administrator to
  pre-install them; it no longer attempts `pacman -Sy` / `apt-get install`;
- opencode uses the official remote install script
  (`curl -fsSL https://opencode.ai/install | bash`), which executes an upstream
  script inside the container — this is an act of trusting the upstream release;
- The prompt template is written to `~/.mixum/agents/<name>/PROMPT.md`;
- `--dry-run` only prints the script that would be executed; `--cmd '<command>'`
  can replace the install command (private mirror or testing), and the prompt is
  still written;
- Installation failure is returned with a non-zero exit code, and in-container
  logs are echoed in real time (log of `task.event`).
