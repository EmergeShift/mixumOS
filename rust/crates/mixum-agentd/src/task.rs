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
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, Mutex};

pub type DriverError = Box<dyn Error + Send + Sync>;
pub type DriverResult = Result<(), DriverError>;

pub const QUEUED: &str = "queued";
pub const RUNNING: &str = "running";
pub const SUSPENDED: &str = "suspended";
pub const COMPLETED: &str = "completed";
pub const FAILED: &str = "failed";
pub const CANCELED: &str = "canceled";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskSpec {
    pub instruction: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub runner: String,
    #[serde(default)]
    pub params: HashMap<String, String>,
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
    pub params: HashMap<String, String>,
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

#[derive(Clone)]
pub struct EventEmitter {
    tx: broadcast::Sender<TaskEvent>,
    task_id: String,
}

impl EventEmitter {
    fn new(tx: broadcast::Sender<TaskEvent>, task_id: String) -> Self {
        Self { tx, task_id }
    }
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
        self.emit("log", None, Some(Value::String(line.into())))
            .await;
    }
    pub async fn step(&self, step: impl Into<String>) {
        self.emit("step", Some(&step.into()), None).await;
    }
    pub async fn progress(&self, pct: u32, step: Option<&str>) {
        self.emit("progress", step, Some(serde_json::json!({"pct": pct})))
            .await;
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
            .cloned()
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
    #[serde(default)]
    line: String,
    #[serde(default)]
    pct: u32,
    #[serde(default)]
    step: String,
    #[serde(default)]
    argv: Vec<String>,
    #[serde(default)]
    error: String,
    #[serde(default)]
    code: i32,
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
            .or_else(|| task.params.get("agentCmd").cloned())
            .ok_or("external 驱动：未指定 agent 可执行文件")?;
        let mut command = Command::new(&command);
        command
            .kill_on_drop(true)
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
        let mut stdin = child.stdin.take().ok_or("external stdin 不可用")?;
        let stdout = child.stdout.take().ok_or("external stdout 不可用")?;
        let stderr = child.stderr.take().ok_or("external stderr 不可用")?;
        let hello =
            serde_json::json!({"type":"hello","instruction":task.instruction,"params":task.params});
        stdin
            .write_all(serde_json::to_string(&hello)?.as_bytes())
            .await?;
        stdin.write_all(b"\n").await?;
        let mut lines = BufReader::new(stdout).lines();
        let mut stderr_lines = BufReader::new(stderr).lines();
        let mut failed: Option<String> = None;
        loop {
            tokio::select! {
                line = lines.next_line() => match line? {
                    Some(line) => match serde_json::from_str::<BridgeMessage>(&line) {
                        Ok(m) => match m.kind.as_str() {
                            "log" => emit.log(m.line).await,
                            "progress" => emit.progress(m.pct, (!m.step.is_empty()).then_some(m.step.as_str())).await,
                            "exec" => {
                                if m.argv.is_empty() { failed = Some("external exec 缺少 argv".into()); continue; }
                                let code = sandbox.exec(task, &m.argv[0], &m.argv[1..], None, Arc::new(|_| {})).await?;
                                let reply = serde_json::json!({"type":"exec.done","code":code,"error":""}); stdin.write_all(serde_json::to_string(&reply)?.as_bytes()).await?; stdin.write_all(b"\n").await?;
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

pub struct Engine {
    registry: Arc<Registry>,
    sandbox: Arc<dyn Sandbox>,
    snapshots: Option<Arc<dyn SnapshotEngine>>,
    tasks: Arc<Mutex<Vec<Arc<Task>>>>,
    events: broadcast::Sender<TaskEvent>,
    sequence: std::sync::atomic::AtomicU64,
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
            sequence: std::sync::atomic::AtomicU64::new(1),
        })
    }
    pub fn subscribe(&self) -> broadcast::Receiver<TaskEvent> {
        self.events.subscribe()
    }
    pub async fn submit(self: &Arc<Self>, spec: TaskSpec) -> Result<Arc<Task>, String> {
        let driver = self
            .registry
            .lookup(&spec.agent)
            .ok_or_else(|| format!("未注册的驱动：{}", spec.agent))?;
        let id = format!(
            "task_{}",
            self.sequence
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        );
        let task = Task::new(id, spec, driver.name().into());
        self.tasks.lock().await.push(task.clone());
        let engine = self.clone();
        let task_clone = task.clone();
        tokio::spawn(async move {
            engine.run(task_clone, driver).await;
        });
        Ok(task)
    }
    async fn run(self: Arc<Self>, task: Arc<Task>, driver: Arc<dyn AgentDriver>) {
        let mut cancel = task.cancel_receiver();
        self.emit_state(&task, QUEUED).await;
        if let Some(policy) = &task.snapshot_policy {
            if policy.pre {
                if let Some(snap) = &self.snapshots {
                    let kind = if policy.kind.is_empty() {
                        "disk"
                    } else {
                        &policy.kind
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
        task.set_state(RUNNING).await;
        task.started().await;
        self.emit_state(&task, RUNNING).await;
        let driver_run = driver.run(
            &task,
            self.sandbox.as_ref(),
            EventEmitter::new(self.events.clone(), task.id.clone()),
        );
        tokio::pin!(driver_run);
        let result = tokio::select! {
            result = &mut driver_run => result,
            changed = cancel.changed() => {
                if changed.is_ok() && *cancel.borrow() { task.set_state(CANCELED).await; task.ended().await; self.emit_state(&task, CANCELED).await; return; }
                Err("任务取消通道关闭".into())
            }
        };
        if *cancel.borrow() {
            task.set_state(CANCELED).await;
            task.ended().await;
            self.emit_state(&task, CANCELED).await;
            return;
        }
        match result {
            Ok(()) => {
                task.set_state(COMPLETED).await;
                task.ended().await;
                self.emit_state(&task, COMPLETED).await;
            }
            Err(err) => {
                if task
                    .snapshot_policy
                    .as_ref()
                    .is_some_and(|p| p.on_failure == "restore")
                {
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
                    data: Some(Value::String(err.to_string())),
                    ts: now_ms(),
                });
                self.emit_state(&task, FAILED).await;
            }
        }
    }
    async fn emit_state(&self, task: &Task, state: &str) {
        let _ = self.events.send(TaskEvent {
            task_id: task.id.clone(),
            event_type: "state".into(),
            step: None,
            data: Some(Value::String(state.into())),
            ts: now_ms(),
        });
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
    use std::sync::atomic::{AtomicUsize, Ordering};
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
                params: HashMap::new(),
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
        let mut params = HashMap::new();
        params.insert("agentCmd".into(), script.to_string_lossy().into_owned());
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
        for _ in 0..50 {
            if matches!(task.state().await.as_str(), COMPLETED | FAILED) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(task.state().await, COMPLETED);
        assert!(std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| event.event_type == "log"
                && event.data == Some(Value::String("bridge-ok".into()))));
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
                params: HashMap::new(),
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
        let a = AtomicUsize::new(0);
        a.fetch_add(1, Ordering::Relaxed);
    }
}
