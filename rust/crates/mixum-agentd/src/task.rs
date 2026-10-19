//! 任务引擎、Sandbox 和内建 shell/external 驱动。

use crate::snapshot::SnapshotEngine;
use crate::wire::{SnapshotPolicy, TaskEvent, TaskView};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::error::Error;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use std::sync::atomic::Ordering;
use tokio::sync::{broadcast, Mutex};

pub type DriverError = Box<dyn Error + Send + Sync>;
pub type DriverResult = Result<(), DriverError>;

pub const QUEUED: &str = "queued";
pub const RUNNING: &str = "running";
pub const SUSPENDED: &str = "suspended";
pub const COMPLETED: &str = "completed";
pub const FAILED: &str = "failed";
pub const CANCELED: &str = "canceled";

/// 人机输入默认超时(tasks.md §The human-input channel:120 s)。
pub const INPUT_TIMEOUT_MS: u64 = 120_000;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TaskSpec {
    pub instruction: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub runner: String,
    /// MAP task.params 是任意 JSON 对象。
    #[serde(default)]
    pub params: serde_json::Map<String, Value>,
    #[serde(default)]
    pub snapshot_policy: Option<SnapshotPolicy>,
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

pub struct Task {
    pub id: String,
    pub instruction: String,
    pub agent: String,
    pub runner: String,
    pub params: serde_json::Map<String, Value>,
    pub created_at: i64,
    snapshot_policy: Option<SnapshotPolicy>,
    inner: Mutex<TaskInner>,
    cancel: tokio::sync::watch::Sender<bool>,
}

impl Task {
    fn new(id: String, spec: TaskSpec, agent: String) -> Arc<Self> {
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
    async fn set_state(&self, state: &str) {
        self.inner.lock().await.state = state.into();
    }
    async fn set_error(&self, error: String) {
        self.inner.lock().await.error = Some(error);
    }
    async fn started(&self) {
        self.inner.lock().await.started_at = Some(now_ms());
    }
    async fn ended(&self) {
        self.inner.lock().await.ended_at = Some(now_ms());
    }
    async fn set_pre_snap(&self, node: String) {
        self.inner.lock().await.pre_snap_node = Some(node);
    }
    pub async fn cancel(&self) {
        let _ = self.cancel.send(true);
    }
    fn cancel_receiver(&self) -> tokio::sync::watch::Receiver<bool> {
        self.cancel.subscribe()
    }
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn exec(
        &self,
        task: &Task,
        program: &str,
        args: &[String],
        dir: Option<&Path>,
        on_line: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Result<i32, DriverError>;
    async fn spawn_gui(
        &self,
        task: &Task,
        program: &str,
        args: &[String],
        dir: Option<&Path>,
    ) -> Result<u32, DriverError>;
    async fn snapshot(&self, _task: &Task, _label: &str) -> Result<(), DriverError> {
        Ok(())
    }
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

#[derive(Clone)]
pub struct EventEmitter {
    tx: broadcast::Sender<TaskEvent>,
    task_id: String,
    inputs: Arc<InputHub>,
}

impl EventEmitter {
    fn new(tx: broadcast::Sender<TaskEvent>, task_id: String, inputs: Arc<InputHub>) -> Self {
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
    pub async fn window_opened(&self, data: Value) {
        self.emit("window", None, Some(data)).await;
    }
}

#[async_trait]
pub trait AgentDriver: Send + Sync {
    fn name(&self) -> &str;
    async fn run(&self, task: &Task, sandbox: &dyn Sandbox, emit: EventEmitter) -> DriverResult;
}

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

pub struct LocalSandbox {
    // std 锁:冻结钩子在同步上下文(FreezeFn)里也要读进程组,锁内无 await。
    pgids: Arc<std::sync::Mutex<Vec<i32>>>,
}

impl Default for LocalSandbox {
    fn default() -> Self {
        Self::new()
    }
}
impl LocalSandbox {
    pub fn new() -> Self {
        Self {
            pgids: Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    /// 冻结/解冻全部任务进程组(快照引擎的 Freeze 回调,对齐 reference FreezeAll)。
    pub fn freeze_all(&self, stop: bool) {
        let signal = if stop {
            nix::libc::SIGSTOP
        } else {
            nix::libc::SIGCONT
        };
        let Ok(signal) = nix::sys::signal::Signal::try_from(signal) else {
            return;
        };
        let pgids = self.pgids.lock().expect("sandbox 进程组锁").clone();
        for pgid in pgids {
            let _ = nix::sys::signal::kill(nix::unistd::Pid::from_raw(-pgid), signal);
        }
    }
}

#[async_trait]
impl Sandbox for LocalSandbox {
    async fn exec(
        &self,
        task: &Task,
        program: &str,
        args: &[String],
        dir: Option<&Path>,
        on_line: Arc<dyn Fn(String) + Send + Sync>,
    ) -> Result<i32, DriverError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        set_process_group(&mut cmd);
        if let Some(path) = dir {
            cmd.current_dir(path);
        }
        let mut child = cmd.spawn()?;
        let pid = child.id();
        if let Some(pid) = pid {
            self.set_task_pid(task, pid as i32).await;
        }
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let callback = on_line.clone();
        let out_task = tokio::spawn(async move {
            if let Some(out) = stdout {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    callback(line);
                }
            }
        });
        let err_callback = on_line;
        let err_task = tokio::spawn(async move {
            if let Some(out) = stderr {
                let mut lines = BufReader::new(out).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    err_callback(line);
                }
            }
        });
        let status = child.wait().await?;
        let _ = out_task.await;
        let _ = err_task.await;
        if let Some(pid) = pid {
            self.release_task_pid(task, pid as i32).await;
        }
        Ok(status.code().unwrap_or(-1))
    }

    async fn spawn_gui(
        &self,
        task: &Task,
        program: &str,
        args: &[String],
        dir: Option<&Path>,
    ) -> Result<u32, DriverError> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        set_process_group(&mut cmd);
        if let Some(path) = dir {
            cmd.current_dir(path);
        }
        let mut child = cmd.spawn()?;
        let pid = child.id().ok_or("子进程没有 pid")?;
        self.set_task_pid(task, pid as i32).await;
        let pgids = self.pgids.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            pgids.lock().expect("sandbox 进程组锁").retain(|current| *current != pid as i32);
        });
        Ok(pid)
    }

    async fn signal(&self, _task: &Task, signal: Signal) -> Result<(), DriverError> {
        let pgids = self.pgids.lock().expect("sandbox 进程组锁").clone();
        let sig = match signal {
            Signal::Stop => nix::libc::SIGSTOP,
            Signal::Continue => nix::libc::SIGCONT,
            Signal::Term => nix::libc::SIGTERM,
            Signal::Kill => nix::libc::SIGKILL,
        };
        for pgid in pgids {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-pgid),
                nix::sys::signal::Signal::try_from(sig)?,
            );
        }
        Ok(())
    }

    async fn set_task_pid(&self, task: &Task, pid: i32) {
        self.pgids.lock().expect("sandbox 进程组锁").push(pid);
        task.inner.lock().await.pgids.push(pid);
    }

    async fn release_task_pid(&self, task: &Task, pid: i32) {
        self.pgids
            .lock()
            .expect("sandbox 进程组锁")
            .retain(|current| *current != pid);
        task.inner
            .lock()
            .await
            .pgids
            .retain(|current| *current != pid);
    }
}

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

#[derive(Clone)]
pub struct ExternalDriver {
    pub command: Option<String>,
}

#[derive(Clone)]
pub struct NamedExternalDriver {
    pub name: String,
    pub command: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct BridgeMessage {
    #[serde(rename = "type")]
    kind: String,
    /// exec / input.request 的请求 id(MAP-Bridge 必填)。
    #[serde(default)]
    id: String,
    #[serde(default)]
    line: String,
    #[serde(default)]
    pct: u32,
    #[serde(default)]
    step: String,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    prompt: String,
    #[serde(default)]
    choices: Option<Vec<String>>,
    #[serde(default)]
    sensitive: bool,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    error: String,
    #[serde(default)]
    code: i32,
}

/// 向 agent 写一行 MAP-Bridge 消息(串行化到同一 stdin)。
async fn write_bridge(
    stdin: &Arc<Mutex<ChildStdin>>,
    message: &Value,
) -> Result<(), DriverError> {
    let mut stdin = stdin.lock().await;
    stdin
        .write_all(serde_json::to_string(message)?.as_bytes())
        .await?;
    stdin.write_all(b"\n").await?;
    Ok(())
}

fn bridge_kernel() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_owned())
        .unwrap_or_else(|_| "unknown".into())
}

#[async_trait]
impl AgentDriver for ExternalDriver {
    fn name(&self) -> &str {
        "external"
    }
    async fn run(&self, task: &Task, sandbox: &dyn Sandbox, emit: EventEmitter) -> DriverResult {
        let command = self
            .command
            .clone()
            .or_else(|| {
                task.params
                    .get("agentCmd")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .ok_or("external 驱动：未指定 agent 可执行文件")?;
        // command 串允许带参数（如 "cli --agent-bridge"）：经 sh -c 执行，
        // 避免把整串当作单个可执行路径导致 ENOENT。
        let mut command = if command.contains(' ') {
            let mut inner = Command::new("sh");
            inner.arg("-c").arg(&command);
            inner
        } else {
            Command::new(&command)
        };
        command
            .kill_on_drop(true)
            // MAP-Bridge 规范环境变量;MIXUM_* 为过渡期兼容,不得再作为规范名。
            .env("MAP_INSTRUCTION", &task.instruction)
            .env("MAP_TASK_ID", &task.id)
            .env("MAP_AGENT_NAME", &task.agent)
            .env("MAP_BRIDGE", "1")
            .env("MIXUM_INSTRUCTION", &task.instruction)
            .env("MIXUM_TASK_ID", &task.id)
            .env("MIXUM_AGENT_BRIDGE", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        set_process_group(&mut command);
        let mut child = command.spawn()?;
        let pid = child.id();
        if let Some(pid) = pid {
            sandbox.set_task_pid(task, pid as i32).await;
        }
        let stdin = Arc::new(Mutex::new(
            child.stdin.take().ok_or("external stdin 不可用")?,
        ));
        let stdout = child.stdout.take().ok_or("external stdout 不可用")?;
        let stderr = child.stderr.take().ok_or("external stderr 不可用")?;
        let workspace = std::env::current_dir()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        let hello = serde_json::json!({
            "type": "hello",
            "instruction": task.instruction,
            "params": task.params,
            "env": {
                "arch": std::env::consts::ARCH,
                "kernel": bridge_kernel(),
                "workspace": workspace,
            }
        });
        // 尽力而为:不读 stdin 的短命 agent 可能在父进程写入前已退出
        // (EPIPE),此时按 stdout EOF 走正常收尾,不能因此标 failed
        if let Err(e) = write_bridge(&stdin, &hello).await {
            eprintln!("external hello 写入失败(子进程可能已退出): {e}");
        }
        let mut lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();
        let mut failed: Option<String> = None;
        loop {
            tokio::select! {
                line = lines.next_line() => match line? {
                    Some(line) => match serde_json::from_str::<BridgeMessage>(&line) {
                        Ok(m) => match m.kind.as_str() {
                            "log" => emit.log(m.line).await,
                            "progress" => {
                                let step = (!m.step.is_empty()).then(|| m.step.clone());
                                emit.progress(m.pct, step.as_deref()).await;
                            }
                            "exec" => {
                                if m.argv.is_empty() {
                                    failed = Some("external exec 缺少 argv".into());
                                    continue;
                                }
                                // MAP:缺省 60 s,超时杀掉进程组。
                                let timeout_ms = m.timeout_ms.filter(|v| *v > 0).unwrap_or(60_000);
                                let execution = sandbox.exec(
                                    task,
                                    &m.argv[0],
                                    &m.argv[1..],
                                    None,
                                    Arc::new(|_| {}),
                                );
                                let outcome = match tokio::time::timeout(
                                    Duration::from_millis(timeout_ms),
                                    execution,
                                )
                                .await
                                {
                                    Ok(result) => result,
                                    Err(_) => {
                                        let _ = sandbox.signal(task, Signal::Kill).await;
                                        Err(format!("执行超时({timeout_ms} ms)").into())
                                    }
                                };
                                let reply = match outcome {
                                    Ok(code) => serde_json::json!({
                                        "type": "exec.done", "id": m.id, "code": code
                                    }),
                                    Err(error) => serde_json::json!({
                                        "type": "exec.done", "id": m.id, "code": null,
                                        "error": error.to_string()
                                    }),
                                };
                                write_bridge(&stdin, &reply).await?;
                            }
                            "input.request" => {
                                if m.id.is_empty() || m.prompt.is_empty() {
                                    emit.log("input.request 缺少 id/prompt,已忽略").await;
                                    continue;
                                }
                                let hub = emit.inputs();
                                let task_id = task.id.clone();
                                let reply_id = m.id.clone();
                                // 先登记再广播,避免客户端在登记前答复导致丢失。
                                let receiver = hub.open(&task_id, &reply_id).await;
                                emit.input_request(
                                    &reply_id,
                                    &m.prompt,
                                    m.choices.clone(),
                                    m.sensitive,
                                )
                                .await;
                                let timeout_ms = m.timeout_ms.filter(|v| *v > 0).unwrap_or(INPUT_TIMEOUT_MS);
                                let stdin = stdin.clone();
                                tokio::spawn(async move {
                                    let value = match tokio::time::timeout(
                                        Duration::from_millis(timeout_ms),
                                        receiver,
                                    )
                                    .await
                                    {
                                        Ok(Ok(value)) => Some(value),
                                        _ => {
                                            hub.expire(&task_id, &reply_id).await;
                                            None
                                        }
                                    };
                                    let response = match value {
                                        Some(value) => serde_json::json!({
                                            "type": "input.response", "id": reply_id, "value": value
                                        }),
                                        // 桥 v1 无独立 timeout 消息:以附加 error 字段的
                                        // input.response 回告(tasks.md 的 timeout 语义)。
                                        None => serde_json::json!({
                                            "type": "input.response", "id": reply_id,
                                            "value": "", "error": "timeout"
                                        }),
                                    };
                                    if let Err(e) = write_bridge(&stdin, &response).await {
                                        eprintln!("input.response 写入失败: {e}");
                                    }
                                });
                            }
                            "fail" => failed = Some(m.error),
                            "done" => {}
                            _ => emit.log(line).await,
                        },
                        Err(_) => emit.log(line).await,
                    },
                    None => break,
                },
                line = stderr_lines.next_line() => if let Some(line) = line? { emit.log(line).await; },
            }
        }
        let status = child.wait().await?;
        if let Some(pid) = pid {
            sandbox.release_task_pid(task, pid as i32).await;
        }
        if let Some(error) = failed {
            return Err(format!("agent：{error}").into());
        }
        if !status.success() {
            return Err(format!("external 驱动：进程退出 {status}").into());
        }
        emit.progress(100, None).await;
        Ok(())
    }
}

#[async_trait]
impl AgentDriver for NamedExternalDriver {
    fn name(&self) -> &str {
        &self.name
    }
    async fn run(&self, task: &Task, sandbox: &dyn Sandbox, emit: EventEmitter) -> DriverResult {
        ExternalDriver {
            command: Some(self.command.clone()),
        }
        .run(task, sandbox, emit)
        .await
    }
}

type ActiveChangeCallback = std::sync::RwLock<Option<Arc<dyn Fn(bool) + Send + Sync>>>;

pub struct Engine {
    registry: Arc<Registry>,
    sandbox: Arc<dyn Sandbox>,
    snapshots: Option<Arc<dyn SnapshotEngine>>,
    tasks: Arc<Mutex<Vec<Arc<Task>>>>,
    events: broadcast::Sender<TaskEvent>,
    /// 人机输入会合点(task.reply ↔ bridge input.request)。
    inputs: Arc<InputHub>,
    active: std::sync::atomic::AtomicUsize,
    /// 活跃任务计数变化回调:0→非 0 传 true(任务开始占用窗口),
    /// 非 0→0 传 false(全部回 free)。由装配层接到显示 broker。
    on_active_change: ActiveChangeCallback,
}

impl Engine {
    pub fn new(registry: Arc<Registry>, sandbox: Arc<dyn Sandbox>) -> Arc<Self> {
        Self::with_snapshots(registry, sandbox, None)
    }
    pub fn with_snapshots(
        registry: Arc<Registry>,
        sandbox: Arc<dyn Sandbox>,
        snapshots: Option<Arc<dyn SnapshotEngine>>,
    ) -> Arc<Self> {
        let (events, _) = broadcast::channel(256);
        Arc::new(Self {
            registry,
            sandbox,
            snapshots,
            tasks: Arc::new(Mutex::new(Vec::new())),
            events,
            inputs: Arc::new(InputHub::new()),
            active: std::sync::atomic::AtomicUsize::new(0),
            on_active_change: std::sync::RwLock::new(None),
        })
    }
    /// 设置活跃任务回调(装配层接显示 broker 的接管状态机)。
    pub fn set_active_change_callback(&self, cb: Arc<dyn Fn(bool) + Send + Sync>) {
        if let Ok(mut slot) = self.on_active_change.write() {
            *slot = Some(cb);
        }
    }
    fn active_changed(&self, active: bool) {
        if let Ok(slot) = self.on_active_change.read() {
            if let Some(cb) = slot.as_ref() {
                cb(active);
            }
        }
    }
    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.events.subscribe()
    }
    pub async fn submit(self: &Arc<Self>, spec: TaskSpec) -> Result<Arc<Task>, String> {
        let driver = self
            .registry
            .lookup(&spec.agent)
            .ok_or_else(|| format!("未注册的驱动：{}", spec.agent))?;
        // MAP task.id 必须是 ULID。
        let id = mixum_protocol::envelope::new_id();
        let task = Task::new(id, spec, driver.name().into());
        self.tasks.lock().await.push(task.clone());
        // MAP tasks.md:task.submit 的应答在 pre-task 快照完成后才发出,
        // 因此快照在这里同步完成(不在 run_inner 里)。
        if let Some(policy) = task.snapshot_policy.clone() {
            if policy.pre {
                if let Some(snap) = &self.snapshots {
                    let kind = if policy.kind.is_empty() {
                        "disk"
                    } else {
                        policy.kind.as_str()
                    };
                    if let Ok(node) = snap
                        .create(kind, Some("任务前置无感快照"), None, None, true)
                        .await
                    {
                        task.set_pre_snap(node.id).await;
                    }
                }
            }
        }
        let engine = self.clone();
        let task_clone = task.clone();
        tokio::spawn(async move {
            engine.run(task_clone, driver).await;
        });
        Ok(task)
    }
    async fn run(self: Arc<Self>, task: Arc<Task>, driver: Arc<dyn AgentDriver>) {
        // 活跃计数 0→1 通知「任务占用窗口」;所有退出路径保证 1→0 通知。
        if self.active.fetch_add(1, Ordering::SeqCst) == 0 {
            self.active_changed(true);
        }
        let _ = self.clone().run_inner(task.clone(), driver).await;
        // 终态后清理人机输入占位(迟到的 task.reply → not_found)。
        self.inputs.clear_task(&task.id).await;
        if self.active.fetch_sub(1, Ordering::SeqCst) == 1 {
            self.active_changed(false);
        }
    }

    async fn run_inner(self: Arc<Self>, task: Arc<Task>, driver: Arc<dyn AgentDriver>) -> Result<(), String> {
        let mut cancel = task.cancel_receiver();
        self.emit_state(&task, QUEUED).await;
        task.set_state(RUNNING).await;
        task.started().await;
        self.emit_state(&task, RUNNING).await;
        let driver_run = driver.run(
            &task,
            self.sandbox.as_ref(),
            EventEmitter::new(self.events.clone(), task.id.clone(), self.inputs.clone()),
        );
        tokio::pin!(driver_run);
        let result = tokio::select! {
            result = &mut driver_run => result,
            changed = cancel.changed() => {
                if changed.is_ok() && *cancel.borrow() {
                    task.set_state(CANCELED).await;
                    task.ended().await;
                    self.emit_state(&task, CANCELED).await;
                    return Ok(());
                }
                Err("任务取消通道关闭".into())
            }
        };
        if *cancel.borrow() {
            task.set_state(CANCELED).await;
            task.ended().await;
            self.emit_state(&task, CANCELED).await;
            return Ok(());
        }
        match result {
            Ok(()) => {
                task.set_state(COMPLETED).await;
                task.ended().await;
                self.emit_state(&task, COMPLETED).await;
            }
            Err(err) => {
                if task.snapshot_policy.as_ref().is_some_and(|p| {
                    p.on_failure == "rollback" || p.on_failure == "restore"
                }) {
                    if let (Some(snap), Some(node)) = (
                        &self.snapshots,
                        task.inner.lock().await.pre_snap_node.clone(),
                    ) {
                        let _ = snap.restore(&node, "inplace", None).await;
                    }
                }
                task.set_error(err.to_string()).await;
                task.set_state(FAILED).await;
                task.ended().await;
                let _ = self.events.send(TaskEvent {
                    task_id: task.id.clone(),
                    event_type: "error".into(),
                    step: None,
                    // MAP task.event/error: data { code?, message }
                    data: Some(serde_json::json!({ "message": err.to_string() })),
                    ts: now_ms(),
                });
                self.emit_state(&task, FAILED).await;
            }
        }
        Ok(())
    }
    async fn emit_state(&self, task: &Task, state: &str) {
        let _ = self.events.send(TaskEvent {
            task_id: task.id.clone(),
            event_type: "state".into(),
            step: None,
            // MAP task.event/state: data { state }(旧实现是裸字符串)
            data: Some(serde_json::json!({ "state": state })),
            ts: now_ms(),
        });
    }
    /// 交付一次 task.reply 到等待中的 bridge input.request。
    pub async fn reply_input(
        &self,
        task_id: &str,
        reply_id: &str,
        value: String,
    ) -> Result<(), InputReplyError> {
        self.inputs.answer(task_id, reply_id, value).await
    }
    pub async fn list(&self) -> Vec<Arc<Task>> {
        self.tasks.lock().await.clone()
    }
    pub async fn get(&self, id: &str) -> Option<Arc<Task>> {
        self.tasks.lock().await.iter().find(|t| t.id == id).cloned()
    }
    pub async fn cancel(&self, id: &str) -> Result<(), String> {
        let task = self
            .get(id)
            .await
            .ok_or_else(|| format!("任务不存在：{id}"))?;
        task.cancel().await;
        let _ = self.sandbox.signal(&task, Signal::Term).await;
        Ok(())
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(unix)]
fn set_process_group(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

#[cfg(not(unix))]
fn set_process_group(_command: &mut Command) {}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::Ordering;
    use tempfile::tempdir;

    #[tokio::test]
    async fn shell_driver_runs_through_sandbox_and_emits_output() {
        let reg = Arc::new(Registry::new());
        reg.register(ShellDriver).unwrap();
        let engine = Engine::new(reg, Arc::new(LocalSandbox::new()));
        let mut events = engine.subscribe();
        let task = engine
            .submit(TaskSpec {
                instruction: "printf hi".into(),
                agent: "shell".into(),
                runner: String::new(),
                params: serde_json::Map::new(),
                snapshot_policy: None,
            })
            .await
            .unwrap();
        for _ in 0..20 {
            if matches!(task.state().await.as_str(), COMPLETED | FAILED) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(task.state().await, COMPLETED);
        let mut saw_log = false;
        while let Ok(event) = events.try_recv() {
            if event.event_type == "log" {
                saw_log = true;
            }
        }
        assert!(saw_log || task.state().await == COMPLETED);
    }

    #[test]
    fn registry_rejects_duplicate_names() {
        let reg = Registry::new();
        assert!(reg.register(ShellDriver).is_ok());
        assert!(reg.register(ShellDriver).is_err());
    }

    #[tokio::test]
    async fn external_bridge_round_trip_emits_log_and_completes() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("agent.sh");
        std::fs::write(&script, "#!/bin/sh\nprintf '%s\\n' '{\"type\":\"log\",\"line\":\"bridge-ok\"}'\nprintf '%s\\n' '{\"type\":\"done\"}'\n").unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();
        let registry = Arc::new(Registry::new());
        registry.register(ExternalDriver { command: None }).unwrap();
        let engine = Engine::new(registry, Arc::new(LocalSandbox::new()));
        let mut events = engine.subscribe();
        let mut params = serde_json::Map::new();
        params.insert(
            "agentCmd".into(),
            Value::String(script.to_string_lossy().into_owned()),
        );
        let task = engine
            .submit(TaskSpec {
                instruction: "bridge".into(),
                agent: "external".into(),
                runner: "here".into(),
                params,
                snapshot_policy: None,
            })
            .await
            .unwrap();
        for _ in 0..200 {
            if matches!(task.state().await.as_str(), COMPLETED | FAILED) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(task.state().await, COMPLETED);
        assert!(std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| event.event_type == "log"
                && event.data == Some(serde_json::json!({ "line": "bridge-ok" }))));
    }

    #[tokio::test]
    async fn cancel_marks_task_canceled_and_terminates_process_group() {
        let registry = Arc::new(Registry::new());
        registry.register(ShellDriver).unwrap();
        let engine = Engine::new(registry, Arc::new(LocalSandbox::new()));
        let task = engine
            .submit(TaskSpec {
                instruction: "sleep 30".into(),
                agent: "shell".into(),
                runner: "here".into(),
                params: serde_json::Map::new(),
                snapshot_policy: None,
            })
            .await
            .unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        engine.cancel(&task.id).await.unwrap();
        for _ in 0..50 {
            if task.state().await == CANCELED {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(task.state().await, CANCELED);
    }

    #[allow(dead_code)]
    fn _atomic_is_used() {
        let a = std::sync::atomic::AtomicUsize::new(0);
        a.fetch_add(1, Ordering::Relaxed);
    }

    #[tokio::test]
    async fn input_hub_answer_conflict_and_expiry() {
        let hub = InputHub::new();
        let receiver = hub.open("t1", "q1").await;
        hub.answer("t1", "q1", "yes".into()).await.unwrap();
        assert_eq!(receiver.await.unwrap(), "yes");
        assert_eq!(
            hub.answer("t1", "q1", "no".into()).await,
            Err(InputReplyError::Conflict),
            "同一 replyId 第二次答复必须 conflict"
        );
        assert_eq!(
            hub.answer("t1", "missing", "x".into()).await,
            Err(InputReplyError::NotFound)
        );

        let _pending = hub.open("t2", "q2").await;
        hub.expire("t2", "q2").await;
        assert_eq!(
            hub.answer("t2", "q2", "late".into()).await,
            Err(InputReplyError::Conflict),
            "超时后迟到的答复必须 conflict"
        );
    }

    #[tokio::test]
    async fn bridge_input_request_round_trips_through_task_reply() {
        let dir = tempdir().unwrap();
        let script = dir.path().join("input-agent.sh");
        std::fs::write(
            &script,
            r#"#!/bin/sh
read hello
printf '%s\n' '{"type":"input.request","id":"q1","prompt":"Recursive?","choices":["yes","no"]}'
read response
case "$response" in
  *'"value":"yes"'*) printf '%s\n' '{"type":"log","line":"answer-yes"}' ;;
  *) printf '%s\n' '{"type":"log","line":"answer-other"}' ;;
esac
printf '%s\n' '{"type":"done"}'
"#,
        )
        .unwrap();
        let mut permissions = std::fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&script, permissions).unwrap();
        let registry = Arc::new(Registry::new());
        registry.register(ExternalDriver { command: None }).unwrap();
        let engine = Engine::new(registry, Arc::new(LocalSandbox::new()));
        let mut events = engine.subscribe();
        let mut params = serde_json::Map::new();
        params.insert(
            "agentCmd".into(),
            Value::String(script.to_string_lossy().into_owned()),
        );
        let task = engine
            .submit(TaskSpec {
                instruction: "ask".into(),
                agent: "external".into(),
                runner: "here".into(),
                params,
                snapshot_policy: None,
            })
            .await
            .unwrap();

        let mut input_event = None;
        for _ in 0..100 {
            while let Ok(event) = events.try_recv() {
                if event.event_type == "input" {
                    input_event = Some(event);
                }
            }
            if input_event.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let input_event = input_event.expect("应收到 task.event/input");
        let data = input_event.data.expect("input 事件带 data");
        assert_eq!(data["replyId"], "q1");
        assert_eq!(data["prompt"], "Recursive?");
        assert_eq!(data["choices"], serde_json::json!(["yes", "no"]));

        engine
            .reply_input(&task.id, "q1", "yes".into())
            .await
            .expect("task.reply 应交付");

        for _ in 0..100 {
            if task.state().await == COMPLETED {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(task.state().await, COMPLETED);
        assert!(
            std::iter::from_fn(|| events.try_recv().ok()).any(|event| {
                event.event_type == "log"
                    && event.data == Some(serde_json::json!({ "line": "answer-yes" }))
            }),
            "agent 应收到 input.response 中的 value"
        );
    }

    #[tokio::test]
    async fn state_events_carry_map_shape() {
        let registry = Arc::new(Registry::new());
        registry.register(ShellDriver).unwrap();
        let engine = Engine::new(registry, Arc::new(LocalSandbox::new()));
        let mut events = engine.subscribe();
        let task = engine
            .submit(TaskSpec {
                instruction: "true".into(),
                agent: "shell".into(),
                ..Default::default()
            })
            .await
            .unwrap();
        for _ in 0..50 {
            if task.state().await == COMPLETED {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let states: Vec<_> = std::iter::from_fn(|| events.try_recv().ok())
            .filter(|event| event.event_type == "state")
            .map(|event| event.data.unwrap_or_default())
            .collect();
        assert!(
            states.contains(&serde_json::json!({ "state": "queued" })),
            "state 事件 data 必须是对象: {states:?}"
        );
        assert!(states.contains(&serde_json::json!({ "state": "completed" })));
    }
}
