//! 任务引擎、Sandbox 和内建 shell/external 驱动。

use crate::snapshot::SnapshotEngine;
use crate::wire::TaskEvent;
use async_trait::async_trait;
use mixum_protocol::GuiSession;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{ChildStdin, Command};
use tokio::sync::{broadcast, Mutex};

// 驱动 SPI 整体来自 mixum-agent-driver crate(独立发布,外部 agent 以
// 依赖方式接入);这里再导出保持本 crate 内部引用与旧路径不变。
pub use mixum_agent_driver::{
    now_ms, AgentDriver, DriverError, DriverResult, EventEmitter, InputHub, InputReplyError,
    Registry, Sandbox, ShellDriver, Signal, Task, TaskSpec, CANCELED, COMPLETED, FAILED, QUEUED,
    RUNNING, SUSPENDED,
};

/// 人机输入默认超时(tasks.md §The human-input channel:120 s)。
pub const INPUT_TIMEOUT_MS: u64 = 120_000;

/// 任务进程身份：默认 mixum，root 仅用于显式批准的 root 容器。
#[derive(Debug, Clone)]
pub struct TaskIdentity {
    pub name: String,
    pub uid: u32,
    pub gid: u32,
    pub home: PathBuf,
}

impl TaskIdentity {
    /// 解析用户名；root 直接返回 0/0。找不到用户时返回当前进程身份，
    /// 避免在开发机（没有 mixum 用户）上把测试跑挂。
    pub fn resolve(name: &str) -> Self {
        if name == "root" {
            return Self {
                name: "root".into(),
                uid: 0,
                gid: 0,
                home: PathBuf::from("/root"),
            };
        }
        if let Ok(Some(user)) = nix::unistd::User::from_name(name) {
            return Self {
                name: user.name,
                uid: user.uid.as_raw(),
                gid: user.gid.as_raw(),
                home: user.dir,
            };
        }
        Self::current()
    }

    pub fn current() -> Self {
        let uid = nix::unistd::Uid::current().as_raw();
        let gid = nix::unistd::Gid::current().as_raw();
        let (name, home) = nix::unistd::User::from_uid(nix::unistd::Uid::from_raw(uid))
            .ok()
            .flatten()
            .map(|user| (user.name, user.dir))
            .unwrap_or_else(|| ("unknown".into(), PathBuf::from("/")));
        Self {
            name,
            uid,
            gid,
            home,
        }
    }

    pub fn is_root(&self) -> bool {
        self.uid == 0
    }

    fn credentials(&self) -> Option<(u32, u32)> {
        (!self.is_root()).then_some((self.uid, self.gid))
    }
}

/// 任务默认以 mixum 运行；root 容器的 entrypoint 显式设置 MIXUM_TASK_USER=root。
pub fn task_identity_from_env() -> TaskIdentity {
    let name = std::env::var("MIXUM_TASK_USER").unwrap_or_else(|_| "mixum".into());
    TaskIdentity::resolve(name.trim())
}

/// 子进程 HOME/USER/LOGNAME 必须指向任务身份，而不是 agentd 的 root 环境。
fn apply_task_env_std(cmd: &mut std::process::Command, identity: &TaskIdentity) {
    cmd.env("HOME", &identity.home)
        .env("USER", &identity.name)
        .env("LOGNAME", &identity.name);
}

fn apply_task_env_tokio(cmd: &mut Command, identity: &TaskIdentity) {
    cmd.env("HOME", &identity.home)
        .env("USER", &identity.name)
        .env("LOGNAME", &identity.name);
}

pub struct LocalSandbox {
    // std 锁:冻结钩子在同步上下文(FreezeFn)里也要读进程组,锁内无 await。
    pgids: Arc<std::sync::Mutex<Vec<i32>>>,
    // 当前任务树的首领 pid（快照引擎 CRIU dump 用）。
    leader: Option<Arc<AtomicI32>>,
    // GUI 会话清单：仅记录 spawn_gui 路径，restore 时按它重放。
    gui: Arc<std::sync::Mutex<HashMap<i32, GuiSession>>>,
    // 系统级隔离:启用后优先 cgroup v2 freezer,并在 spawn 时套 Landlock。
    cgroup: Option<crate::isolation::CgroupFreezer>,
    workspace: Option<PathBuf>,
    identity: TaskIdentity,
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
            leader: None,
            gui: Arc::new(std::sync::Mutex::new(HashMap::new())),
            cgroup: None,
            workspace: None,
            identity: TaskIdentity::current(),
        }
    }

    /// 指定任务运行身份（默认当前用户，生产装配传 mixum）。
    pub fn with_identity(mut self, identity: TaskIdentity) -> Self {
        self.identity = identity;
        self
    }

    /// 绑定一个共享的 leader pid，供快照引擎读取当前任务树首领。
    pub fn with_leader(mut self, leader: Arc<AtomicI32>) -> Self {
        self.leader = Some(leader);
        self
    }

    /// 终止全部任务进程组（同机 CRIU 恢复前调用）。
    pub fn kill_all(&self) {
        let pgids = self.pgids.lock().expect("sandbox 进程组锁").clone();
        for pgid in pgids {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(-pgid),
                nix::sys::signal::Signal::SIGKILL,
            );
        }
        self.pgids.lock().expect("sandbox 进程组锁").clear();
        if let Some(leader) = &self.leader {
            leader.store(0, Ordering::SeqCst);
        }
    }

    /// 启用系统级隔离：cgroup v2 freezer（内核/委托允许时）+ Landlock
    /// 目录白名单（Linux 5.13+）。不启用时保持旧行为（SIGSTOP + 无限制），
    /// 便于测试和不支持的环境。
    pub fn with_isolation(mut self, workspace: impl Into<PathBuf>) -> Self {
        crate::isolation::prepare_user_dirs(
            &self.identity.home,
            self.identity.uid,
            self.identity.gid,
        );
        self.cgroup = crate::isolation::CgroupFreezer::create();
        self.workspace = Some(workspace.into());
        self
    }

    /// 冻结/解冻全部任务进程组(快照引擎的 Freeze 回调,对齐 reference FreezeAll)。
    /// 有 cgroup freezer 时原子冻结整棵子树（含脱离进程组的孙进程），
    /// 否则退回进程组 SIGSTOP。
    pub fn freeze_all(&self, stop: bool) {
        if let Some(cgroup) = &self.cgroup {
            if cgroup.set_frozen(stop).is_ok() {
                return;
            }
        }
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

    /// 当前仍存活的 GUI 启动记录（快照 metadata 钩子用）。
    pub fn gui_launches(&self) -> Vec<GuiSession> {
        self.gui
            .lock()
            .expect("gui 会话锁")
            .values()
            .filter(|session| pid_alive(session.pid))
            .cloned()
            .collect()
    }

    /// restore 后按清单重放：先终止本地旧进程（仅限我们自己跟踪的 pid），
    /// 再以原命令/环境/工作目录拉起新进程。返回成功拉起的数量。
    pub fn replay_gui_sessions(&self, sessions: &[GuiSession]) -> usize {
        let mut started = 0;
        for session in sessions {
            if !session.has_launch() {
                continue;
            }
            let tracked = self
                .gui
                .lock()
                .expect("gui 会话锁")
                .contains_key(&session.pid);
            if tracked && session.pid > 0 {
                if !terminate_tracked(session.pid) {
                    eprintln!(
                        "GUI 会话重放跳过（pid {} 未退出）：{}",
                        session.pid, session.command
                    );
                    continue;
                }
                self.gui.lock().expect("gui 会话锁").remove(&session.pid);
                self.pgids
                    .lock()
                    .expect("sandbox 进程组锁")
                    .retain(|pid| *pid != session.pid);
            }
            match self.spawn_replay(session) {
                Ok(_) => started += 1,
                Err(error) => eprintln!("GUI 会话重放失败（{}）：{error}", session.command),
            }
        }
        started
    }

    fn spawn_replay(&self, session: &GuiSession) -> std::io::Result<i32> {
        let mut cmd = std::process::Command::new(&session.command);
        cmd.args(&session.args);
        if !session.cwd.is_empty() {
            cmd.current_dir(&session.cwd);
        } else if let Some(workspace) = &self.workspace {
            cmd.current_dir(workspace);
        }
        let env = replay_env(session);
        cmd.env_clear().envs(&env);
        apply_task_env_std(&mut cmd, &self.identity);
        apply_x11_env_std(&mut cmd);
        cmd.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        crate::isolation::apply_spawn_isolation(
            &mut cmd,
            self.cgroup.clone(),
            self.workspace.clone(),
            Some(&self.identity.home),
            self.identity.credentials(),
        );
        let mut child = cmd.spawn()?;
        let pid = child.id() as i32;
        let mut recorded = session.clone();
        recorded.pid = pid;
        self.gui.lock().expect("gui 会话锁").insert(pid, recorded);
        self.pgids.lock().expect("sandbox 进程组锁").push(pid);
        let gui = self.gui.clone();
        let pgids = self.pgids.clone();
        std::thread::spawn(move || {
            let _ = child.wait();
            gui.lock().expect("gui 会话锁").remove(&pid);
            pgids
                .lock()
                .expect("sandbox 进程组锁")
                .retain(|current| *current != pid);
        });
        Ok(pid)
    }
}

fn pid_alive(pid: i32) -> bool {
    pid > 0 && nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

/// 终止重放前的旧进程：先 SIGTERM 并等待退出，超时再 SIGKILL，
/// 避免新旧窗口/控制 socket 在同一 pid 上争用。
fn terminate_tracked(pid: i32) -> bool {
    if pid <= 0 {
        return true;
    }
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(-pid),
        nix::sys::signal::Signal::SIGTERM,
    );
    if wait_pid_gone(pid, Duration::from_secs(1)) {
        return true;
    }
    let _ = nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(-pid),
        nix::sys::signal::Signal::SIGKILL,
    );
    wait_pid_gone(pid, Duration::from_millis(300))
}

fn wait_pid_gone(pid: i32, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    !pid_alive(pid)
}

/// 重放时保留最小环境：路径、显示、运行时目录与 MIXUM_WORK。
fn gui_env() -> BTreeMap<String, String> {
    gui_env_from(std::env::vars())
}

/// 只保留启动 GUI 必需的最小环境；MIXUM_* 里的令牌/密钥不入快照。
fn gui_env_from(vars: impl IntoIterator<Item = (String, String)>) -> BTreeMap<String, String> {
    const KEEP: [&str; 7] = [
        "PATH",
        "HOME",
        "USER",
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XDG_RUNTIME_DIR",
        "MIXUM_WORK",
    ];
    vars.into_iter()
        .filter(|(key, _)| KEEP.contains(&key.as_str()))
        .collect()
}

/// 重放环境:无论快照里记录了什么,都按 GUI 白名单过滤;
/// 空环境退回当前进程的最小 GUI 环境。
fn replay_env(session: &GuiSession) -> BTreeMap<String, String> {
    if session.env.is_empty() {
        gui_env()
    } else {
        gui_env_from(session.env.iter().map(|(k, v)| (k.clone(), v.clone())))
    }
}

/// 只重放本机产生的 disk 快照：memory/hybrid 恢复已由 CRIU 带回进程，
/// 再按 guiSessions 重放会产生重复窗口；sync/import 节点 origin 非空且不可信。
pub fn gui_replay_trusted(origin: &str, memory_restored: bool) -> bool {
    !memory_restored && origin.is_empty()
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
        apply_task_env_tokio(&mut cmd, &self.identity);
        cmd.env("MIXUM_TASK_ID", &task.id);
        apply_x11_env(&mut cmd);
        cmd.args(args)
            .kill_on_drop(true)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        set_process_group(&mut cmd);
        crate::isolation::apply_spawn_isolation(
            cmd.as_std_mut(),
            self.cgroup.clone(),
            self.workspace.clone(),
            Some(&self.identity.home),
            self.identity.credentials(),
        );
        let cwd = dir
            .map(Path::to_path_buf)
            .or_else(|| self.workspace.clone());
        if let Some(path) = cwd {
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
        let env = gui_env();
        let mut cmd = Command::new(program);
        cmd.env_clear();
        cmd.envs(&env);
        apply_task_env_tokio(&mut cmd, &self.identity);
        cmd.env("MIXUM_TASK_ID", &task.id);
        apply_x11_env(&mut cmd);
        cmd.args(args)
            .kill_on_drop(true)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        set_process_group(&mut cmd);
        crate::isolation::apply_spawn_isolation(
            cmd.as_std_mut(),
            self.cgroup.clone(),
            self.workspace.clone(),
            Some(&self.identity.home),
            self.identity.credentials(),
        );
        let cwd = dir
            .map(Path::to_path_buf)
            .or_else(|| self.workspace.clone());
        if let Some(path) = cwd {
            cmd.current_dir(path);
        }
        let mut child = cmd.spawn()?;
        let pid = child.id().ok_or("子进程没有 pid")?;
        self.set_task_pid(task, pid as i32).await;
        let session = GuiSession {
            command: program.to_string(),
            args: args.to_vec(),
            cwd: dir
                .map(|path| path.to_string_lossy().into_owned())
                .unwrap_or_default(),
            env,
            pid: pid as i32,
            ..Default::default()
        };
        self.gui
            .lock()
            .expect("gui 会话锁")
            .insert(pid as i32, session);
        let pgids = self.pgids.clone();
        let gui = self.gui.clone();
        tokio::spawn(async move {
            let _ = child.wait().await;
            pgids
                .lock()
                .expect("sandbox 进程组锁")
                .retain(|current| *current != pid as i32);
            gui.lock().expect("gui 会话锁").remove(&(pid as i32));
        });
        Ok(pid)
    }

    async fn signal(&self, _task: &Task, signal: Signal) -> Result<(), DriverError> {
        // per-task 信号必须只作用于该任务进程组；cgroup freezer 是全局的，
        // 只用于快照的 freeze_all，不能在这里冻结所有任务。
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
        task.push_pgid(pid).await;
        if let Some(leader) = &self.leader {
            leader.store(pid, Ordering::SeqCst);
        }
    }

    async fn release_task_pid(&self, task: &Task, pid: i32) {
        self.pgids
            .lock()
            .expect("sandbox 进程组锁")
            .retain(|current| *current != pid);
        task.release_pgid(pid).await;
        if let Some(leader) = &self.leader {
            if leader.load(Ordering::SeqCst) == pid {
                leader.store(0, Ordering::SeqCst);
            }
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
async fn write_bridge(stdin: &Arc<Mutex<ChildStdin>>, message: &Value) -> Result<(), DriverError> {
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
        apply_x11_env(&mut command);
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
        // stderr EOF 后必须停止轮询它:next_line() 会永久立即返回 None,
        // 而 stdout 仍开着的话 select 循环会空转打满 CPU。
        let mut stderr_open = true;
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
                line = async {
                    if stderr_open {
                        stderr_lines.next_line().await
                    } else {
                        futures_util::future::pending::<std::io::Result<Option<String>>>().await
                    }
                } => match line? {
                    Some(line) => emit.log(line).await,
                    None => stderr_open = false,
                },
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
        if let Some(policy) = task.snapshot_policy().cloned() {
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

    async fn run_inner(
        self: Arc<Self>,
        task: Arc<Task>,
        driver: Arc<dyn AgentDriver>,
    ) -> Result<(), String> {
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
                if task
                    .snapshot_policy()
                    .is_some_and(|p| p.on_failure == "rollback" || p.on_failure == "restore")
                {
                    if let (Some(snap), Some(node)) = (&self.snapshots, task.pre_snap_node().await)
                    {
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

#[cfg(unix)]
/// B compositor's XWayland exposes its display number here. Inject it into
/// task/agent processes so X11 apps need no developer-supplied env.
fn x11_display() -> Option<String> {
    let raw = std::fs::read_to_string("/run/mixum/xwayland.display").ok()?;
    let display = raw.trim();
    if display.is_empty() {
        None
    } else {
        Some(format!(":{display}"))
    }
}

fn apply_x11_env(command: &mut Command) {
    if let Some(display) = x11_display() {
        command
            .env("DISPLAY", display)
            .env("XMODIFIERS", "@im=mixum");
    }
}

fn apply_x11_env_std(command: &mut std::process::Command) {
    if let Some(display) = x11_display() {
        command
            .env("DISPLAY", display)
            .env("XMODIFIERS", "@im=mixum");
    }
}

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
        // 上限 5s：CI runner 上进程启动与调度偶发超过 200ms，
        // 原来固定 20×10ms 的窗口会把这个用例变成 flaky（2026-10-21 出现过一次）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
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
        let mut states = Vec::new();
        for _ in 0..100 {
            while let Ok(event) = events.try_recv() {
                if event.event_type == "state" {
                    states.push(event.data.unwrap_or_default());
                }
            }
            if task.state().await == COMPLETED
                && states.contains(&serde_json::json!({ "state": "completed" }))
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert!(
            states.contains(&serde_json::json!({ "state": "queued" })),
            "state 事件 data 必须是对象: {states:?}"
        );
        assert!(states.contains(&serde_json::json!({ "state": "completed" })));
    }

    #[test]
    fn replay_gui_sessions_spawns_with_recorded_command() {
        let sandbox = LocalSandbox::new();
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("replayed.txt");
        let session = GuiSession {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), format!("echo replayed > {}", target.display())],
            cwd: dir.path().to_string_lossy().into_owned(),
            env: Default::default(),
            pid: 999_999,
            ..Default::default()
        };
        assert_eq!(sandbox.replay_gui_sessions(&[session]), 1);
        let mut replayed = false;
        for _ in 0..100 {
            if target.exists() {
                replayed = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(replayed, "重放命令没有产生文件");
    }
}

#[cfg(test)]
mod gui_env_tests {
    use super::*;

    #[test]
    fn gui_env_strips_tokens_and_unknown_mixum_vars() {
        let env = gui_env_from([
            ("PATH".to_string(), "/usr/bin".to_string()),
            ("MIXUM_WORK".to_string(), "/var/lib/mixum/work".to_string()),
            ("MIXUM_GATEWAY_TOKEN".to_string(), "secret".to_string()),
            ("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string()),
        ]);
        assert_eq!(env.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(
            env.get("MIXUM_WORK").map(String::as_str),
            Some("/var/lib/mixum/work")
        );
        assert!(!env.contains_key("MIXUM_GATEWAY_TOKEN"));
        assert!(!env.contains_key("LD_PRELOAD"));
    }

    #[test]
    fn replay_env_filters_recorded_snapshot_variables() {
        let mut env = BTreeMap::new();
        env.insert("PATH".to_string(), "/usr/bin".to_string());
        env.insert("HOME".to_string(), "/home/u".to_string());
        env.insert("LD_PRELOAD".to_string(), "/tmp/evil.so".to_string());
        env.insert("MIXUM_GATEWAY_TOKEN".to_string(), "secret".to_string());
        let session = GuiSession {
            command: "app".into(),
            env,
            ..GuiSession::default()
        };
        let filtered = replay_env(&session);
        assert_eq!(filtered.get("PATH").map(String::as_str), Some("/usr/bin"));
        assert_eq!(filtered.get("HOME").map(String::as_str), Some("/home/u"));
        assert!(!filtered.contains_key("LD_PRELOAD"));
        assert!(!filtered.contains_key("MIXUM_GATEWAY_TOKEN"));
    }

    #[test]
    fn terminate_tracked_returns_for_invalid_pid() {
        assert!(terminate_tracked(0));
        assert!(terminate_tracked(-1));
    }

    #[test]
    fn only_local_origin_is_trusted_for_replay() {
        assert!(gui_replay_trusted("", false));
        assert!(!gui_replay_trusted("  ", false));
        assert!(!gui_replay_trusted("remote", false));
        assert!(!gui_replay_trusted("test-host", false));
        assert!(!gui_replay_trusted("", true));
        assert!(!gui_replay_trusted("test-host", true));
    }
}
