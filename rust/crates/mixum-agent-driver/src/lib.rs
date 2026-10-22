//! mixumOS 任务驱动 SPI——外部 agent 以 `impl AgentDriver` 接入容器任务引擎。
//!
//! 最小接入面：实现 [`AgentDriver`]，经 [`Registry`] 注册，mixumd 的任务
//! 引擎按 `task.submit { agent: "<name>" }` 路由到你的驱动。驱动通过
//! [`Sandbox`] 在容器内执行命令/拉起 GUI 进程，通过 [`EventEmitter`]
//! 上报日志/步骤/进度/窗口事件（对齐协议 task.events 流）。
//!
//! ```no_run
//! use mixum_agent_driver::{AgentDriver, DriverResult, EventEmitter, Registry, Sandbox, Task};
//! use std::sync::Arc;
//!
//! struct MyAgent;
//!
//! #[async_trait::async_trait]
//! impl AgentDriver for MyAgent {
//!     fn name(&self) -> &str { "my-agent" }
//!     async fn run(&self, task: &Task, sandbox: &dyn Sandbox, emit: EventEmitter) -> DriverResult {
//!         emit.step("plan").await;
//!         let code = sandbox.exec(task, "/bin/sh", &["-c".into(), task.instruction.clone()], None, Arc::new(|_| {})).await?;
//!         if code == 0 { Ok(()) } else { Err(format!("退出码 {code}").into()) }
//!     }
//! }
//!
//! let registry = Registry::new();
//! registry.register(MyAgent).unwrap();
//! // 把 Registry 交给 mixumd 的任务引擎(见 docs/agent-driver.md 两种组装方式)
//! ```

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::{broadcast, Mutex};

/// 驱动错误：装箱任意 `Error + Send + Sync`，驱动侧用 `?` 与 `Err("...".into())`。
pub type DriverError = Box<dyn Error + Send + Sync>;
pub type DriverResult = Result<(), DriverError>;

/// 任务状态机常量(与协议 task.events 的 state 字段一致)。
pub const QUEUED: &str = "queued";
pub const RUNNING: &str = "running";
pub const SUSPENDED: &str = "suspended";
pub const COMPLETED: &str = "completed";
pub const FAILED: &str = "failed";
pub const CANCELED: &str = "canceled";

/// 任务提交规格(协议 task.submit 的 payload)。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskSpec {
    pub instruction: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub runner: String,
    /// MAP task.params 是任意 JSON 对象(agent 私有参数)。
    #[serde(default)]
    pub params: serde_json::Map<String, Value>,
    #[serde(default)]
    pub snapshot_policy: Option<SnapshotPolicy>,
}

/// 任务前置快照策略(MAP tasks.md §Ops)。
/// MAP 的 `pre` 是 `"always" | "never"`;过渡期同时接受旧客户端的 bool。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPolicy {
    #[serde(default, deserialize_with = "deserialize_pre")]
    pub pre: bool,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub on_failure: String,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        // policy 一旦出现,MAP 缺省 pre=always、kind=disk、onFailure=keep。
        Self {
            pre: true,
            kind: "disk".into(),
            on_failure: "keep".into(),
        }
    }
}

fn deserialize_pre<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Pre {
        Bool(bool),
        MapMode(String),
    }
    match Pre::deserialize(deserializer)? {
        Pre::Bool(value) => Ok(value),
        Pre::MapMode(mode) => match mode.as_str() {
            "always" => Ok(true),
            "never" => Ok(false),
            other => Err(serde::de::Error::custom(format!(
                "snapshotPolicy.pre 应为 always|never(旧值 bool 亦可),收到 {other}"
            ))),
        },
    }
}

#[derive(Debug)]
struct TaskInner {
    state: String,
    error: Option<String>,
    started_at: Option<i64>,
    ended_at: Option<i64>,
    pre_snap_node: Option<String>,
    pgids: Vec<i32>,
}

/// 任务句柄：驱动读取任务上下文；引擎管理状态机与取消。
pub struct Task {
    pub id: String,
    pub instruction: String,
    pub agent: String,
    pub runner: String,
    /// MAP task.params 是任意 JSON 对象。
    pub params: serde_json::Map<String, Value>,
    pub created_at: i64,
    snapshot_policy: Option<SnapshotPolicy>,
    inner: Mutex<TaskInner>,
    cancel: tokio::sync::watch::Sender<bool>,
}

/// 任务列表/详情视图(协议 task.list/get 的 payload)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub id: String,
    pub instruction: String,
    pub agent: String,
    pub runner: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_snap_node: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub params: serde_json::Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
}

impl Task {
    /// 协议视图(task.list/get)。
    pub async fn view(&self) -> TaskView {
        let inner = self.inner.lock().await;
        TaskView {
            id: self.id.clone(),
            instruction: self.instruction.clone(),
            agent: self.agent.clone(),
            runner: self.runner.clone(),
            state: inner.state.clone(),
            pre_snap_node: inner.pre_snap_node.clone(),
            params: self.params.clone(),
            error: inner.error.clone(),
            created_at: self.created_at,
            started_at: inner.started_at,
            ended_at: inner.ended_at,
        }
    }
    /// 由引擎构造;外部装配自定义引擎时也可直接使用。
    pub fn new(id: String, spec: TaskSpec, agent: String) -> Arc<Self> {
        let (cancel, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            id,
            instruction: spec.instruction,
            agent,
            runner: if spec.runner.is_empty() {
                "here".into()
            } else {
                spec.runner
            },
            params: spec.params,
            created_at: now_ms(),
            snapshot_policy: spec.snapshot_policy.clone(),
            inner: Mutex::new(TaskInner {
                state: QUEUED.into(),
                error: None,
                started_at: None,
                ended_at: None,
                pre_snap_node: None,
                pgids: vec![],
            }),
            cancel,
        })
    }

    pub async fn state(&self) -> String {
        self.inner.lock().await.state.clone()
    }
    pub async fn pgids(&self) -> Vec<i32> {
        self.inner.lock().await.pgids.clone()
    }
    pub fn snapshot_policy(&self) -> Option<&SnapshotPolicy> {
        self.snapshot_policy.as_ref()
    }
    pub async fn pre_snap_node(&self) -> Option<String> {
        self.inner.lock().await.pre_snap_node.clone()
    }
    /// 进程组登记(Sandbox 实现方在 spawn 后调用,供冻结/信号按组发送)。
    pub async fn push_pgid(&self, pid: i32) {
        self.inner.lock().await.pgids.push(pid);
    }
    pub async fn release_pgid(&self, pid: i32) {
        self.inner
            .lock()
            .await
            .pgids
            .retain(|current| *current != pid);
    }
    /// 状态变更(引擎内部状态机使用;自定义引擎装配时也可驱动)。
    pub async fn set_state(&self, state: &str) {
        self.inner.lock().await.state = state.into();
    }
    pub async fn set_error(&self, error: String) {
        self.inner.lock().await.error = Some(error);
    }
    pub async fn started(&self) {
        self.inner.lock().await.started_at = Some(now_ms());
    }
    pub async fn ended(&self) {
        self.inner.lock().await.ended_at = Some(now_ms());
    }
    pub async fn set_pre_snap(&self, node: String) {
        self.inner.lock().await.pre_snap_node = Some(node);
    }
    pub async fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
    pub fn cancel_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.cancel.subscribe()
    }
}

/// 沙箱抽象：驱动不直接碰进程/容器,经此执行命令与拉起 GUI 进程。
/// mixumd 内建实现按进程组管理子进程(冻结/信号对齐快照引擎)。
#[async_trait]
pub trait Sandbox: Send + Sync {
    /// 执行命令,逐行回调 stdout/stderr;返回退出码。
    async fn exec(
        &self,
        task: &Task,
        program: &str,
        args: &[String],
        dir: Option<&Path>,
        on_line: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Result<i32, DriverError>;
    /// 拉起长驻 GUI 进程(detach stdout/stderr),返回 pid。
    async fn spawn_gui(
        &self,
        task: &Task,
        program: &str,
        args: &[String],
        dir: Option<&Path>,
    ) -> Result<u32, DriverError>;
    /// 快照钩子(缺省空实现;mixumd 装配真实快照引擎)。
    async fn snapshot(&self, _task: &Task, _label: &str) -> Result<(), DriverError> {
        Ok(())
    }
    /// 向任务的进程组发信号(冻结/解冻/终止/强杀)。
    async fn signal(&self, task: &Task, signal: Signal) -> Result<(), DriverError>;
    async fn set_task_pid(&self, task: &Task, pid: i32);
    async fn release_task_pid(&self, _task: &Task, _pid: i32) {}
}

#[derive(Debug, Clone, Copy)]
pub enum Signal {
    Stop,
    Continue,
    Term,
    Kill,
}

/// 任务事件负载(协议 task.events 流的单条事件)。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEvent {
    pub task_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    pub ts: i64,
}

/// `task.reply` 的失败原因:未知/已过期 → not_found,重复答复 → conflict。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputReplyError {
    NotFound,
    Conflict,
}

struct InputEntry {
    /// None = 已答复/已超时;保留占位以便区分 conflict 与 not_found。
    sender: Option<tokio::sync::oneshot::Sender<String>>,
}

/// 人机输入通道:bridge `input.request` 与 MAP `task.reply` 之间的会合点。
/// 任务终态时由引擎清理,避免占位无限增长。
#[derive(Default)]
pub struct InputHub {
    pending: Mutex<HashMap<(String, String), InputEntry>>,
}

impl InputHub {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一次 input.request,返回等待回答的接收端。
    pub async fn open(
        &self,
        task_id: &str,
        reply_id: &str,
    ) -> tokio::sync::oneshot::Receiver<String> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.pending.lock().await.insert(
            (task_id.to_owned(), reply_id.to_owned()),
            InputEntry {
                sender: Some(sender),
            },
        );
        receiver
    }

    /// 交付一次回答;重复回答同一 reply_id → conflict。
    pub async fn answer(
        &self,
        task_id: &str,
        reply_id: &str,
        value: String,
    ) -> Result<(), InputReplyError> {
        let mut pending = self.pending.lock().await;
        match pending.get_mut(&(task_id.to_owned(), reply_id.to_owned())) {
            None => Err(InputReplyError::NotFound),
            Some(entry) => match entry.sender.take() {
                Some(sender) => {
                    let _ = sender.send(value);
                    Ok(())
                }
                None => Err(InputReplyError::Conflict),
            },
        }
    }

    /// 超时:不再接受回答(保留占位 → 迟到的回答报 conflict)。
    pub async fn expire(&self, task_id: &str, reply_id: &str) {
        if let Some(entry) = self
            .pending
            .lock()
            .await
            .get_mut(&(task_id.to_owned(), reply_id.to_owned()))
        {
            entry.sender = None;
        }
    }

    pub async fn clear_task(&self, task_id: &str) {
        self.pending
            .lock()
            .await
            .retain(|(task, _), _| task != task_id);
    }
}

/// 驱动向引擎上报事件的出口:log/step/progress/window/input。
#[derive(Clone)]
pub struct EventEmitter {
    tx: broadcast::Sender<TaskEvent>,
    task_id: String,
    inputs: Arc<InputHub>,
}

impl EventEmitter {
    pub fn new(tx: broadcast::Sender<TaskEvent>, task_id: String, inputs: Arc<InputHub>) -> Self {
        Self {
            tx,
            task_id,
            inputs,
        }
    }
    pub fn task_id(&self) -> &str {
        &self.task_id
    }
    pub fn inputs(&self) -> Arc<InputHub> {
        self.inputs.clone()
    }
    /// 事件 data 统一为对象(MAP task.event.data: object)。
    pub async fn emit(&self, event_type: &str, step: Option<&str>, data: Option<Value>) {
        let _ = self.tx.send(TaskEvent {
            task_id: self.task_id.clone(),
            event_type: event_type.into(),
            step: step.map(str::to_owned),
            data,
            ts: now_ms(),
        });
    }
    pub async fn log(&self, line: impl Into<String>) {
        let line = line.into();
        self.emit("log", None, Some(serde_json::json!({ "line": line })))
            .await;
    }
    pub async fn step(&self, step: impl Into<String>) {
        let step = step.into();
        self.emit(
            "step",
            Some(&step),
            Some(serde_json::json!({ "name": step })),
        )
        .await;
    }
    pub async fn progress(&self, pct: u32, step: Option<&str>) {
        let pct = pct.min(100);
        let data = match step {
            Some(step) => serde_json::json!({ "pct": pct, "step": step }),
            None => serde_json::json!({ "pct": pct }),
        };
        self.emit("progress", step, Some(data)).await;
    }
    /// 人机输入请求:客户端看到 task.event { type: "input", data: { replyId, … } }。
    pub async fn input_request(
        &self,
        reply_id: &str,
        prompt: &str,
        choices: Option<Vec<String>>,
        sensitive: bool,
    ) {
        let mut data = serde_json::json!({ "replyId": reply_id, "prompt": prompt });
        if let Some(choices) = choices {
            data["choices"] = serde_json::json!(choices);
        }
        if sensitive {
            data["sensitive"] = serde_json::json!(true);
        }
        self.emit("input", None, Some(data)).await;
    }
    /// 窗口事件:GUI 驱动拉起窗口后上报(引擎接到显示 broker 建窗口流)。
    pub async fn window_opened(&self, data: Value) {
        self.emit("window", None, Some(data)).await;
    }
}

/// 任务驱动 SPI:实现并注册后,mixumd 按 `task.submit { agent: name }` 路由。
#[async_trait]
pub trait AgentDriver: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, task: &Task, sandbox: &dyn Sandbox, emit: EventEmitter) -> DriverResult;
}

/// 驱动注册表:重名拒绝;空 agent 名回退 "shell"。
#[derive(Default)]
pub struct Registry {
    drivers: std::sync::RwLock<HashMap<String, Arc<dyn AgentDriver>>>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn register<D: AgentDriver + 'static>(&self, driver: D) -> Result<(), String> {
        let name = driver.name().to_owned();
        let mut map = self.drivers.write().expect("registry lock");
        if map.contains_key(&name) {
            return Err(format!("驱动重名：{name}"));
        }
        map.insert(name, Arc::new(driver));
        Ok(())
    }
    pub fn lookup(&self, name: &str) -> Option<Arc<dyn AgentDriver>> {
        let map = self.drivers.read().expect("registry lock");
        if name.is_empty() {
            map.get("shell").cloned()
        } else {
            map.get(name).cloned()
        }
    }
    pub fn names(&self) -> Vec<String> {
        self.drivers
            .read()
            .expect("registry lock")
            .keys()
            .cloned()
            .collect()
    }
}

/// 内建 shell 驱动:`params.cmd`(缺省用 instruction)交 /bin/bash -lc 执行。
pub struct ShellDriver;
#[async_trait]
impl AgentDriver for ShellDriver {
    fn name(&self) -> &str {
        "shell"
    }
    async fn run(&self, task: &Task, sandbox: &dyn Sandbox, emit: EventEmitter) -> DriverResult {
        emit.step("plan").await;
        let command = task
            .params
            .get("cmd")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| task.instruction.clone());
        emit.step("exec").await;
        let line_emit = emit.clone();
        let on_line = Arc::new(move |line: String| {
            let emit = line_emit.clone();
            tokio::spawn(async move {
                emit.log(line).await;
            });
        });
        let code = sandbox
            .exec(task, "/bin/bash", &["-lc".into(), command], None, on_line)
            .await?;
        emit.progress(100, Some("exec")).await;
        if code == 0 {
            Ok(())
        } else {
            Err(format!("shell 驱动：命令退出码 {code}").into())
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeSandbox;

    #[async_trait]
    impl Sandbox for FakeSandbox {
        async fn exec(
            &self,
            _task: &Task,
            program: &str,
            args: &[String],
            _dir: Option<&Path>,
            on_line: Arc<dyn Fn(String) + Send + Sync>,
        ) -> Result<i32, DriverError> {
            assert_eq!(program, "/bin/bash");
            on_line(format!("ran: {}", args.join(" ")));
            Ok(0)
        }
        async fn spawn_gui(
            &self,
            _task: &Task,
            _program: &str,
            _args: &[String],
            _dir: Option<&Path>,
        ) -> Result<u32, DriverError> {
            Ok(4242)
        }
        async fn signal(&self, _task: &Task, _signal: Signal) -> Result<(), DriverError> {
            Ok(())
        }
        async fn set_task_pid(&self, task: &Task, pid: i32) {
            task.push_pgid(pid).await;
        }
    }

    #[tokio::test]
    async fn registry_routes_by_name_and_shell_driver_runs() {
        let reg = Registry::new();
        reg.register(ShellDriver).unwrap();
        assert!(reg.register(ShellDriver).is_err());
        assert_eq!(reg.names(), vec!["shell".to_owned()]);
        assert!(reg.lookup("").is_some()); // 空 agent 回退 shell

        let driver = reg.lookup("shell").unwrap();
        let task = Task::new(
            "t1".into(),
            TaskSpec {
                instruction: "echo hi".into(),
                agent: "shell".into(),
                ..Default::default()
            },
            "shell".into(),
        );
        let (tx, mut rx) = broadcast::channel(16);
        let result = driver
            .run(
                &task,
                &FakeSandbox,
                EventEmitter::new(tx, task.id.clone(), Arc::new(InputHub::new())),
            )
            .await;
        assert!(result.is_ok());
        // ShellDriver 的 log 经 tokio::spawn 异步上报,让一拍再排水
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let mut saw_log = false;
        while let Ok(event) = rx.try_recv() {
            if event.event_type == "log" {
                saw_log = true;
            }
        }
        assert!(saw_log);
        assert_eq!(task.state().await, QUEUED); // 状态机归引擎,驱动不动状态
    }
}
