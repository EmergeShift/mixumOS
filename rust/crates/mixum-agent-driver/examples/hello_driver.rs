//! 通用化封装最小故事：外部 agent 以依赖方式接入 mixumOS 任务引擎。
//!
//! 本示例不依赖 mixum-agentd——只依赖本 SPI crate，实现一个自定义驱动
//! 和一个最小沙箱，组装出可跑的任务回路。生产接入两条路：
//! 1. in-process：像本例这样实现 AgentDriver，把 Registry 交给 mixumd
//!    的任务引擎（fork 后在你的 main.rs 装配，或提 PR 注册内建驱动）；
//! 2. 子进程桥接：不改代码，agent 按 mixumos docs/agent-bridge.md 的
//!    桥协议独立成进程，由内建 external 驱动拉起。
//!
//! 运行：cargo run -p mixum-agent-driver --example hello_driver

use async_trait::async_trait;
use mixum_agent_driver::{
    AgentDriver, EventEmitter, InputHub, Registry, Sandbox, Signal, Task, TaskEvent, TaskSpec,
    COMPLETED, RUNNING,
};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::broadcast;

/// 你的 agent 驱动:实现 run() 即完成接入。
struct GreetingAgent;

#[async_trait]
impl AgentDriver for GreetingAgent {
    fn name(&self) -> &str {
        "greeting"
    }

    async fn run(
        &self,
        task: &Task,
        sandbox: &dyn Sandbox,
        emit: EventEmitter,
    ) -> mixum_agent_driver::DriverResult {
        emit.step("greet").await;
        let name = task
            .params
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("world")
            .to_owned();
        let code = sandbox
            .exec(
                task,
                "/bin/sh",
                &["-c".into(), format!("echo hello, {name}")],
                None,
                Arc::new(|_| {}),
            )
            .await?;
        emit.progress(100, Some("greet")).await;
        if code == 0 {
            Ok(())
        } else {
            Err(format!("greeting 退出码 {code}").into())
        }
    }
}

/// 最小沙箱:演示 SPI 的另一侧(真实实现见 mixumd 的 LocalSandbox)。
struct EchoSandbox;

#[async_trait]
impl Sandbox for EchoSandbox {
    async fn exec(
        &self,
        _task: &Task,
        program: &str,
        args: &[String],
        _dir: Option<&Path>,
        on_line: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Result<i32, mixum_agent_driver::DriverError> {
        // 真实实现会起进程;这里直接把命令回显为输出。
        on_line(format!("$ {program} {}", args.join(" ")));
        Ok(0)
    }
    async fn spawn_gui(
        &self,
        _task: &Task,
        _program: &str,
        _args: &[String],
        _dir: Option<&Path>,
    ) -> Result<u32, mixum_agent_driver::DriverError> {
        Ok(1)
    }
    async fn signal(
        &self,
        _task: &Task,
        _signal: Signal,
    ) -> Result<(), mixum_agent_driver::DriverError> {
        Ok(())
    }
    async fn set_task_pid(&self, task: &Task, pid: i32) {
        task.push_pgid(pid).await;
    }
}

/// 极简引擎回路:submit → 标 RUNNING → 驱动跑 → 标 COMPLETED(生产版
/// 状态机/快照/取消在 mixumd 的 Engine)。
async fn run_once(
    driver: Arc<dyn AgentDriver>,
    sandbox: Arc<dyn Sandbox>,
    spec: TaskSpec,
    tx: broadcast::Sender<TaskEvent>,
) {
    let task = Task::new("demo-1".into(), spec, driver.name().into());
    let _ = tx.send(TaskEvent {
        task_id: task.id.clone(),
        event_type: "state".into(),
        step: None,
        data: Some(serde_json::json!({ "state": RUNNING })),
        ts: mixum_agent_driver::now_ms(),
    });
    let result = driver
        .run(
            &task,
            sandbox.as_ref(),
            EventEmitter::new(tx.clone(), task.id.clone(), Arc::new(InputHub::new())),
        )
        .await;
    let _ = tx.send(TaskEvent {
        task_id: task.id.clone(),
        event_type: "state".into(),
        step: None,
        data: Some(serde_json::json!({
            "state": if result.is_ok() { COMPLETED } else { "failed" }
        })),
        ts: mixum_agent_driver::now_ms(),
    });
}

#[tokio::main]
async fn main() {
    let registry = Registry::new();
    registry.register(GreetingAgent).unwrap();
    let driver = registry.lookup("greeting").unwrap();

    let (tx, mut rx) = broadcast::channel(64);
    run_once(
        driver,
        Arc::new(EchoSandbox),
        TaskSpec {
            instruction: "问好".into(),
            agent: "greeting".into(),
            params: serde_json::Map::from_iter([("name".to_owned(), "mixumOS".into())]),
            ..Default::default()
        },
        tx,
    )
    .await;

    while let Ok(event) = rx.recv().await {
        println!(
            "[{}] {}",
            event.event_type,
            event.data.clone().unwrap_or_default()
        );
        if event.event_type == "state"
            && matches!(
                event.data.as_ref().and_then(serde_json::Value::as_str),
                Some("completed") | Some("failed")
            )
        {
            break;
        }
    }
}
