//! 容器内 sudo 请求：agent 只能提交单条 argv，由 Mart/CLI 审批后 root 执行。
//!
//! 关键约束：
//! - 不提供 root shell，只执行一次精确 argv；
//! - 审批事件只发给有 Admin 能力的已连接会话；
//! - 无审批人时超时拒绝，绝不默认放行；
//! - 审计只记录命令与结果，不记录环境变量中的秘密。

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mixum_protocol::envelope::{new_id, Envelope};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex};

use crate::server::SessionStore;

const REQUEST_TTL: Duration = Duration::from_secs(120);
const EXEC_TIMEOUT: Duration = Duration::from_secs(300);
const OUTPUT_LIMIT: usize = 64 * 1024;
/// 同时待审批上限与请求频率上限，避免容器内进程刷屏或堆积请求。
const MAX_PENDING: usize = 8;
const RATE_WINDOW: Duration = Duration::from_secs(60);
const RATE_PER_TASK: usize = 6;
const RATE_GLOBAL: usize = 20;
/// 审批后固定使用的 PATH；程序解析只在这几个 root 目录里做。
const TRUSTED_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
/// 本 broker 不提供 root shell，也不允许借 wrapper 间接起 shell。
const FORBIDDEN_PROGRAMS: &[&str] = &["sh", "bash", "dash", "zsh", "ksh", "fish", "busybox", "env"];

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub approve: bool,
    #[serde(default)]
    pub reason: String,
    /// 目前只支持单次审批；保留字段用于协议前向兼容。
    #[serde(default)]
    pub scope: String,
    /// 审批人身份，由已认证的管理会话填写并写入审计。
    #[serde(default)]
    pub approver: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

struct Pending {
    request: Value,
    tx: oneshot::Sender<Decision>,
    expires_at: Instant,
}

#[derive(Default)]
struct State {
    pending: HashMap<String, Pending>,
    recent: HashMap<String, VecDeque<Instant>>,
    recent_global: VecDeque<Instant>,
}

impl State {
    /// 记录一次请求并检查频率；保留任务维度与全局两道闸。
    fn admit(&mut self, key: &str, now: Instant) -> Result<(), String> {
        if self.pending.len() >= MAX_PENDING {
            return Err("待审批的 sudo 请求过多，请稍后重试".into());
        }
        let cutoff = now.checked_sub(RATE_WINDOW).unwrap_or(now);
        self.recent_global.retain(|time| *time > cutoff);
        if self.recent_global.len() >= RATE_GLOBAL {
            return Err("sudo 请求过于频繁，请稍后重试".into());
        }
        self.recent
            .retain(|_, times| times.back().is_some_and(|time| *time > cutoff));
        let times = self.recent.entry(key.to_owned()).or_default();
        times.retain(|time| *time > cutoff);
        if times.len() >= RATE_PER_TASK {
            return Err("sudo 请求过于频繁，请稍后重试".into());
        }
        times.push_back(now);
        self.recent_global.push_back(now);
        Ok(())
    }
}

/// 审批时固化的执行计划：审批人看到的路径/哈希就是随后执行的路径/哈希。
/// 可执行文件与整条祖先目录链必须 root 所有且不可被非 root 写入,
/// 因此审批后内容不会被同 uid 的 agent 替换。
#[derive(Debug, Clone)]
struct ExecPlan {
    program: String,
    resolved: PathBuf,
    sha256: String,
    args: Vec<String>,
    cwd: PathBuf,
}

pub struct PrivilegeBroker {
    sessions: Arc<SessionStore>,
    state: Mutex<State>,
    audit_path: PathBuf,
    request_ttl: Duration,
}

impl PrivilegeBroker {
    pub fn new(sessions: Arc<SessionStore>, data_dir: impl AsRef<Path>) -> Arc<Self> {
        Self::with_ttl(sessions, data_dir, REQUEST_TTL)
    }

    pub fn with_ttl(
        sessions: Arc<SessionStore>,
        data_dir: impl AsRef<Path>,
        request_ttl: Duration,
    ) -> Arc<Self> {
        let audit_path = data_dir.as_ref().join("audit/privilege.log");
        Arc::new(Self {
            sessions,
            state: Mutex::new(State::default()),
            audit_path,
            request_ttl,
        })
    }

    /// 由容器内 `mixum-sudo` 调用：发起审批并等待；批准后以 root 执行。
    pub async fn request(
        &self,
        program: &str,
        args: &[String],
        cwd: &str,
        reason: &str,
        task_id: &str,
    ) -> Result<Outcome, String> {
        self.request_with_peer(program, args, cwd, reason, task_id, None)
            .await
    }

    async fn request_with_peer(
        &self,
        program: &str,
        args: &[String],
        cwd: &str,
        reason: &str,
        task_id: &str,
        peer_uid: Option<u32>,
    ) -> Result<Outcome, String> {
        // 先把程序解析成 root 所有的绝对路径并计算哈希；审批人看到的
        // resolved/sha256 就是随后实际执行的对象。
        let plan = prepare_execution(program, args, cwd)?;
        let rate_key = if task_id.is_empty() {
            peer_uid.map_or_else(|| "anonymous".to_owned(), |uid| format!("uid:{uid}"))
        } else {
            // taskId 由客户端自报，不能作为授权依据；这里只用于限流分桶，
            // 全局闸仍然兜底。
            format!("task:{task_id}")
        };
        let id = new_id();
        let created_at = now_ms();
        let risk = classify(&plan.resolved.to_string_lossy(), &plan.args);
        let event = json!({
            "id": id,
            "taskId": task_id,
            "peerUid": peer_uid,
            "program": plan.program,
            "resolved": plan.resolved.to_string_lossy(),
            "sha256": plan.sha256,
            "args": plan.args,
            "cwd": plan.cwd.to_string_lossy(),
            "reason": reason,
            "risk": risk,
            "createdAt": created_at,
            "expiresAt": created_at + self.request_ttl.as_millis() as i64,
        });
        let (tx, rx) = oneshot::channel();
        {
            let mut state = self.state.lock().await;
            state.admit(&rate_key, Instant::now())?;
            state.pending.insert(
                id.clone(),
                Pending {
                    request: event.clone(),
                    tx,
                    expires_at: Instant::now() + self.request_ttl,
                },
            );
        }
        self.sessions
            .broadcast_admin(Envelope::new("privilege.request", Some(event.clone())))
            .await;
        let decision = match tokio::time::timeout(self.request_ttl, rx).await {
            Ok(Ok(decision)) => decision,
            Ok(Err(_)) => {
                // decide() 已移除并审计（例如请求已过期）。
                return Err("sudo 审批请求已关闭".into());
            }
            Err(_) => {
                let removed = self.state.lock().await.pending.remove(&id).is_some();
                if removed {
                    self.audit(&id, &event, "timeout", None, None).await;
                }
                return Err("sudo 审批超时".into());
            }
        };
        if !decision.approve {
            self.audit(&id, &event, "denied", Some(&decision), None)
                .await;
            return Err(format!(
                "sudo 被拒绝{}",
                if decision.reason.is_empty() {
                    String::new()
                } else {
                    format!(": {}", decision.reason)
                }
            ));
        }
        match self.execute(&plan).await {
            Ok(outcome) => {
                self.audit(
                    &id,
                    &event,
                    "executed",
                    Some(&decision),
                    Some(&format!("exit={}", outcome.exit_code)),
                )
                .await;
                Ok(outcome)
            }
            Err(error) => {
                self.audit(&id, &event, "failed", Some(&decision), Some(&error))
                    .await;
                Err(error)
            }
        }
    }

    /// Current pending requests for Admin clients that connected after the push.
    pub async fn list(&self) -> Vec<Value> {
        self.state
            .lock()
            .await
            .pending
            .values()
            .map(|pending| pending.request.clone())
            .collect()
    }

    /// 由 Mart/CLI 的 `privilege.decision` 调用。
    pub async fn decide(&self, id: &str, decision: Decision) -> Result<(), String> {
        if !decision.scope.is_empty() && decision.scope != "once" {
            return Err(format!("不支持的审批范围: {}", decision.scope));
        }
        let removed = self.state.lock().await.pending.remove(id);
        let Some(pending) = removed else {
            return Err("请求不存在或已过期".into());
        };
        if Instant::now() >= pending.expires_at {
            self.audit(id, &pending.request, "expired", Some(&decision), None)
                .await;
            return Err("请求已过期".into());
        }
        pending
            .tx
            .send(decision)
            .map_err(|_| "审批请求已关闭".to_string())
    }

    async fn execute(&self, plan: &ExecPlan) -> Result<Outcome, String> {
        // 审批哈希与 exec 之间仍有路径替换窗口:把已校验字节复制到
        // root-only 目录并执行副本,确保“执行对象”就是哈希覆盖的字节。
        let pin_dir = self
            .audit_path
            .parent()
            .ok_or("audit 路径无父目录")?
            .join("privilege-pins");
        fs::create_dir_all(&pin_dir).map_err(|error| format!("创建 privilege pin 目录: {error}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{chown, PermissionsExt};
            fs::set_permissions(&pin_dir, fs::Permissions::from_mode(0o700))
                .map_err(|error| format!("设置 privilege pin 目录权限: {error}"))?;
            chown(&pin_dir, Some(0), Some(0))
                .map_err(|error| format!("privilege pin 目录 chown: {error}"))?;
        }
        let pinned = pin_dir.join(format!("{}.bin", new_id()));
        fs::copy(&plan.resolved, &pinned).map_err(|error| format!("复制可执行文件: {error}"))?;
        let pinned_hash = hash_file(&pinned)?;
        if pinned_hash != plan.sha256 {
            let _ = fs::remove_file(&pinned);
            return Err("可执行文件在审批后被修改（哈希不一致）".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&pinned, fs::Permissions::from_mode(0o500))
                .map_err(|error| format!("设置 privilege pin 文件权限: {error}"))?;
        }
        let mut cmd = tokio::process::Command::new(&pinned);
        // 固化路径会解开符号链接（Alpine 的 /usr/bin/id -> /bin/busybox）；
        // 保留原始 argv[0] 才能让 busybox applet 按审批时的名字分发。
        use std::os::unix::process::CommandExt;
        cmd.as_std_mut().arg0(&plan.program);
        cmd.args(&plan.args)
            .current_dir(&plan.cwd)
            .env_clear()
            .env("PATH", TRUSTED_PATH)
            .env("HOME", "/root")
            .env("USER", "root")
            .env("LOGNAME", "root")
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        set_process_group(&mut cmd);
        let mut child = match cmd.spawn() {
            Ok(child) => child,
            Err(error) => {
                let _ = fs::remove_file(&pinned);
                return Err(format!("启动 root 命令: {error}"));
            }
        };
        // 子进程已 exec,立即解除目录项;即使后续路径被替换也不会影响本次执行。
        let _ = fs::remove_file(&pinned);
        // process_group(0) 让 pid 同时是进程组 id；超时要连同子孙一起杀。
        let pgid = child.id();
        let mut stdout = child.stdout.take().ok_or("缺少 stdout 管道")?;
        let mut stderr = child.stderr.take().ok_or("缺少 stderr 管道")?;
        let collector = tokio::spawn(async move {
            let mut out = Vec::new();
            let mut err = Vec::new();
            let (a, b) = tokio::join!(
                read_capped(&mut stdout, &mut out, OUTPUT_LIMIT),
                read_capped(&mut stderr, &mut err, OUTPUT_LIMIT),
            );
            let status = child.wait().await?;
            Ok::<_, std::io::Error>((status, out, err, a?, b?))
        });
        match tokio::time::timeout(EXEC_TIMEOUT, collector).await {
            Ok(joined) => {
                let (status, out, err, out_overflow, err_overflow) = joined
                    .map_err(|error| format!("root 命令任务失败: {error}"))?
                    .map_err(|error| format!("root 命令失败: {error}"))?;
                Ok(Outcome {
                    exit_code: status.code().unwrap_or(-1),
                    stdout: render(&out, out_overflow),
                    stderr: render(&err, err_overflow),
                })
            }
            Err(_) => {
                if let Some(pgid) = pgid {
                    kill_group(pgid);
                }
                Err("root 命令超时（已终止进程组）".into())
            }
        }
    }

    async fn audit(
        &self,
        id: &str,
        event: &Value,
        result: &str,
        decision: Option<&Decision>,
        detail: Option<&str>,
    ) {
        if let Some(parent) = self.audit_path.parent() {
            if let Err(error) = tokio::fs::create_dir_all(parent).await {
                eprintln!("privilege audit 创建目录失败: {error}");
            }
        }
        let record = json!({
            "ts": now_ms(),
            "id": id,
            "result": result,
            "approver": decision.map(|d| d.approver.as_str()).unwrap_or_default(),
            "scope": decision.map(|d| d.scope.as_str()).unwrap_or_default(),
            "reason": decision.map(|d| d.reason.as_str()).unwrap_or_default(),
            "detail": detail.unwrap_or_default(),
            "request": event,
        });
        let line = format!("{record}\n");
        let path = self.audit_path.clone();
        let written = tokio::task::spawn_blocking(move || {
            let mut file = fs::OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .open(&path)?;
            file.write_all(line.as_bytes())?;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
            Ok::<_, std::io::Error>(())
        })
        .await;
        match written {
            Ok(Ok(())) => {}
            Ok(Err(error)) => eprintln!("privilege audit 写入失败: {error}"),
            Err(error) => eprintln!("privilege audit 任务失败: {error}"),
        }
    }

    /// 监听 `/run/mixum/sudo.sock`；只接受 mixum/root uid。
    pub async fn serve_socket(
        self: Arc<Self>,
        path: &str,
        allowed_uid: u32,
        allowed_gid: u32,
    ) -> std::io::Result<()> {
        let socket_path = std::path::Path::new(path);
        if let Some(parent) = socket_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::{chown, PermissionsExt};
                // 目录必须 root 所有、组可执行但不可写;否则 mixum 用户可替换 socket。
                tokio::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o750)).await?;
                chown(parent, Some(0), Some(allowed_gid)).map_err(|error| {
                    std::io::Error::other(format!("sudo socket 目录 chown 失败: {error}"))
                })?;
            }
        }
        let _ = tokio::fs::remove_file(path).await;
        let listener = UnixListener::bind(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::{chown, PermissionsExt};
            tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660)).await?;
            chown(socket_path, None, Some(allowed_gid)).map_err(|error| {
                std::io::Error::other(format!("sudo socket chown 失败: {error}"))
            })?;
        }
        loop {
            let (stream, _) = listener.accept().await?;
            let this = self.clone();
            tokio::spawn(async move {
                let _ = this.handle_conn(stream, allowed_uid).await;
            });
        }
    }

    async fn handle_conn(&self, stream: UnixStream, allowed_uid: u32) -> std::io::Result<()> {
        #[cfg(not(target_os = "linux"))]
        let peer_uid = {
            let _ = allowed_uid;
            None
        };
        // peer_cred 失败时拒绝连接：不能确认对端身份就不进入审批。
        #[cfg(target_os = "linux")]
        let peer_uid = {
            let cred = match stream.peer_cred() {
                Ok(cred) => cred,
                Err(error) => {
                    eprintln!("privilege: 读取对端凭据失败，拒绝连接: {error}");
                    return Ok(());
                }
            };
            let uid = cred.uid();
            if uid != 0 && uid != allowed_uid {
                eprintln!("privilege: 拒绝 uid={uid} 的连接（允许 uid={allowed_uid} 或 root）");
                return Ok(());
            }
            Some(uid)
        };
        let (reader, mut writer) = stream.into_split();
        let mut lines = BufReader::new(reader).lines();
        let Some(line) = lines.next_line().await? else {
            return Ok(());
        };
        let request: Value = serde_json::from_str(&line).unwrap_or_default();
        let program = request.get("program").and_then(Value::as_str).unwrap_or("");
        let args: Vec<String> = request
            .get("args")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(ToOwned::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let cwd = request
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("/var/lib/mixum/work");
        let reason = request.get("reason").and_then(Value::as_str).unwrap_or("");
        let task_id = request.get("taskId").and_then(Value::as_str).unwrap_or("");
        let reply = match self
            .request_with_peer(program, &args, cwd, reason, task_id, peer_uid)
            .await
        {
            Ok(outcome) => {
                json!({"ok": true, "exitCode": outcome.exit_code, "stdout": outcome.stdout, "stderr": outcome.stderr})
            }
            Err(error) => json!({"ok": false, "error": error}),
        };
        writer.write_all(format!("{reply}\n").as_bytes()).await?;
        writer.shutdown().await?;
        Ok(())
    }
}

fn classify(program: &str, args: &[String]) -> &'static str {
    let name = Path::new(program)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(program);
    match name {
        "npm" | "pnpm" | "yarn" | "pip" | "pip3" | "cargo" | "pacman" | "apt" | "apt-get" => {
            "package"
        }
        "rm" | "dd" | "mkfs" | "shred" | "fdisk" | "parted" => "destructive",
        "systemctl" | "mount" | "umount" | "chown" | "chmod" | "useradd" | "usermod" => "system",
        "curl" | "wget" | "ssh" | "scp" | "rsync" => "network",
        "sh" | "bash" => "shell",
        _ => {
            let _ = args;
            "unknown"
        }
    }
}

fn prepare_execution(program: &str, args: &[String], cwd: &str) -> Result<ExecPlan, String> {
    if program.trim().is_empty() || program.contains('\0') {
        return Err("program 为空或非法".into());
    }
    if args.iter().any(|arg| arg.contains('\0')) {
        return Err("参数含 NUL".into());
    }
    let name = Path::new(program)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or(program);
    if FORBIDDEN_PROGRAMS.contains(&name) {
        return Err(format!("不允许通过 sudo 启动 {name}（不提供 root shell）"));
    }
    let resolved = resolve_program(program)?;
    let sha256 = hash_file(&resolved)?;
    let cwd = PathBuf::from(cwd);
    if !cwd.is_absolute() {
        return Err("cwd 必须是绝对路径".into());
    }
    let cwd = fs::canonicalize(&cwd).map_err(|error| format!("cwd 不可用: {error}"))?;
    if !cwd.is_dir() {
        return Err("cwd 不是目录".into());
    }
    Ok(ExecPlan {
        program: program.to_owned(),
        resolved,
        sha256,
        args: args.to_vec(),
        cwd,
    })
}

/// 只接受命令名（在固定 PATH 中查找）或绝对路径；相对路径随任务 cwd
/// 变化，等于让 agent 决定执行哪个文件。
fn resolve_program(program: &str) -> Result<PathBuf, String> {
    let candidate = if Path::new(program).is_absolute() {
        PathBuf::from(program)
    } else if program.contains('/') {
        return Err("程序路径必须是绝对路径".into());
    } else {
        TRUSTED_PATH
            .split(':')
            .map(|dir| Path::new(dir).join(program))
            .find(|path| path.is_file())
            .ok_or_else(|| format!("找不到命令: {program}"))?
    };
    let resolved =
        fs::canonicalize(&candidate).map_err(|error| format!("解析可执行文件失败: {error}"))?;
    ensure_trusted(&resolved)?;
    Ok(resolved)
}

/// 文件本身及整条祖先目录链都必须是 root 所有、对 group/other 不可写。
fn ensure_trusted(path: &Path) -> Result<(), String> {
    for current in path.ancestors() {
        let metadata = fs::metadata(current)
            .map_err(|error| format!("读取 {} 失败: {error}", current.display()))?;
        if current == path && !metadata.is_file() {
            return Err("可执行文件不是普通文件".into());
        }
        if metadata.uid() != 0 {
            return Err(format!("{} 不属于 root", current.display()));
        }
        if metadata.mode() & 0o022 != 0 {
            return Err(format!("{} 可被非 root 写入", current.display()));
        }
    }
    Ok(())
}

fn hash_file(path: &Path) -> Result<String, String> {
    let mut file = fs::File::open(path).map_err(|error| format!("打开可执行文件失败: {error}"))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("读取可执行文件失败: {error}"))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn set_process_group(command: &mut tokio::process::Command) {
    use std::os::unix::process::CommandExt;
    command.as_std_mut().process_group(0);
}

fn kill_group(pgid: u32) {
    use nix::sys::signal::{killpg, Signal};
    use nix::unistd::Pid;
    let _ = killpg(Pid::from_raw(pgid as i32), Signal::SIGKILL);
}

/// 持续排空管道但只保留前 `limit` 字节，避免子进程因管道写满而卡死；
/// 超限部分丢弃并在结果里标记截断。
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<bool> {
    let mut chunk = [0u8; 16 * 1024];
    let mut overflow = false;
    loop {
        let read = reader.read(&mut chunk).await?;
        if read == 0 {
            return Ok(overflow);
        }
        let space = limit.saturating_sub(buffer.len());
        let take = read.min(space);
        buffer.extend_from_slice(&chunk[..take]);
        if take < read {
            overflow = true;
        }
    }
}

fn render(bytes: &[u8], overflow: bool) -> String {
    let mut text = String::from_utf8_lossy(bytes).into_owned();
    if overflow {
        text.push_str("\n…(截断)");
    }
    text
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_commands() {
        assert_eq!(classify("npm", &["i".into(), "-g".into()]), "package");
        assert_eq!(classify("rm", &["-rf".into()]), "destructive");
        assert_eq!(classify("curl", &[]), "network");
    }

    #[test]
    fn pins_executable_to_root_owned_absolute_path() {
        let plan = prepare_execution("id", &["-u".into()], "/tmp").expect("id should resolve");
        assert!(plan.resolved.is_absolute());
        assert_eq!(plan.program, "id");
        assert_eq!(plan.sha256.len(), 64);
        assert_eq!(plan.cwd, PathBuf::from("/tmp"));
    }

    #[test]
    fn rejects_shells_relative_paths_and_untrusted_files() {
        assert!(prepare_execution("sh", &["-c".into(), "id".into()], "/tmp").is_err());
        assert!(prepare_execution("env", &["id".into()], "/tmp").is_err());
        assert!(prepare_execution("./id", &[], "/tmp").is_err());
        assert!(prepare_execution("id", &[], "relative/dir").is_err());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("writable");
        std::fs::write(&path, b"#!/bin/sh\n").unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(prepare_execution(path.to_str().unwrap(), &[], "/tmp").is_err());
    }
}

#[cfg(test)]
mod approval_tests {
    use super::*;
    use mixum_protocol::ops::Cap;
    use mixum_protocol::token::Claims;
    use tokio::sync::mpsc;

    fn claims() -> Claims {
        Claims {
            sub: "test-admin".into(),
            caps: vec![Cap::Admin],
            iat: 0,
            exp: i64::MAX / 2,
            jti: String::new(),
            task_id: String::new(),
            windows: vec![],
        }
    }

    #[tokio::test]
    async fn approval_executes_and_denial_blocks() {
        let sessions = Arc::new(SessionStore::new(16));
        let (tx, mut rx) = mpsc::channel(16);
        sessions.create("test-conn", claims(), tx).await;
        let dir = tempfile::tempdir().unwrap();
        let broker = PrivilegeBroker::new(sessions.clone(), dir.path());

        let request = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .request("id", &["-u".into()], "/tmp", "test approval", "")
                    .await
            })
        };
        let message = rx.recv().await.expect("privilege event");
        let text = message.to_text().expect("text event");
        let event: Value = serde_json::from_str(text).expect("event json");
        let id = event["payload"]["id"]
            .as_str()
            .expect("request id")
            .to_owned();
        broker
            .decide(
                &id,
                Decision {
                    approve: true,
                    reason: "test".into(),
                    scope: "once".into(),
                    approver: "test-admin".into(),
                },
            )
            .await
            .expect("decide");
        let outcome = request.await.unwrap().expect("approved command");
        assert_eq!(outcome.exit_code, 0);
        assert_eq!(
            outcome.stdout.trim(),
            nix::unistd::Uid::current().as_raw().to_string()
        );

        let request = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .request("id", &["-u".into()], "/tmp", "denied", "")
                    .await
            })
        };
        let message = rx.recv().await.expect("privilege event");
        let text = message.to_text().expect("text event");
        let event: Value = serde_json::from_str(text).expect("event json");
        let id = event["payload"]["id"]
            .as_str()
            .expect("request id")
            .to_owned();
        broker
            .decide(
                &id,
                Decision {
                    approve: false,
                    reason: "no".into(),
                    scope: "once".into(),
                    approver: "test-admin".into(),
                },
            )
            .await
            .expect("decide");
        assert!(request.await.unwrap().is_err());
    }
}

#[cfg(test)]
mod hardening_tests {
    use super::*;
    use mixum_protocol::ops::Cap;
    use mixum_protocol::token::Claims;
    use tokio::sync::mpsc;
    use tokio_tungstenite::tungstenite::Message;

    fn admin_claims() -> Claims {
        Claims {
            sub: "test-admin".into(),
            caps: vec![Cap::Admin],
            iat: 0,
            exp: i64::MAX / 2,
            jti: String::new(),
            task_id: String::new(),
            windows: vec![],
        }
    }

    fn decision(approver: &str, scope: &str) -> Decision {
        Decision {
            approve: true,
            reason: "test".into(),
            scope: scope.into(),
            approver: approver.into(),
        }
    }

    async fn next_request(rx: &mut mpsc::Receiver<Message>) -> String {
        let message = rx.recv().await.expect("privilege event");
        let event: Value =
            serde_json::from_str(message.to_text().expect("text event")).expect("event json");
        event["payload"]["id"]
            .as_str()
            .expect("request id")
            .to_owned()
    }

    #[tokio::test]
    async fn caps_pending_requests() {
        let sessions = Arc::new(SessionStore::new(16));
        let (tx, _rx) = mpsc::channel(64);
        sessions.create("cap", admin_claims(), tx).await;
        let dir = tempfile::tempdir().unwrap();
        let broker = PrivilegeBroker::new(sessions, dir.path());

        let mut handles = Vec::new();
        for index in 0..MAX_PENDING {
            let broker = broker.clone();
            handles.push(tokio::spawn(async move {
                broker
                    .request(
                        "id",
                        &["-u".into()],
                        "/tmp",
                        "cap",
                        &format!("task-{index}"),
                    )
                    .await
            }));
        }
        for _ in 0..200 {
            if broker.list().await.len() == MAX_PENDING {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(broker.list().await.len(), MAX_PENDING);
        let error = broker
            .request("id", &["-u".into()], "/tmp", "cap", "task-extra")
            .await
            .unwrap_err();
        assert!(error.contains("待审批"), "unexpected error: {error}");
        for handle in handles {
            handle.abort();
        }
    }

    #[tokio::test]
    async fn unknown_scope_does_not_consume_request() {
        let sessions = Arc::new(SessionStore::new(16));
        let (tx, mut rx) = mpsc::channel(16);
        sessions.create("scope", admin_claims(), tx).await;
        let dir = tempfile::tempdir().unwrap();
        let broker = PrivilegeBroker::new(sessions, dir.path());
        let request = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .request("id", &["-u".into()], "/tmp", "scope", "")
                    .await
            })
        };
        let id = next_request(&mut rx).await;
        let error = broker
            .decide(&id, decision("test-admin", "always"))
            .await
            .unwrap_err();
        assert!(error.contains("审批范围"), "unexpected error: {error}");
        assert_eq!(broker.list().await.len(), 1);
        broker
            .decide(&id, decision("test-admin", "once"))
            .await
            .unwrap();
        assert!(request.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn audit_records_approver_and_result() {
        let sessions = Arc::new(SessionStore::new(16));
        let (tx, mut rx) = mpsc::channel(16);
        sessions.create("audit", admin_claims(), tx).await;
        let dir = tempfile::tempdir().unwrap();
        let broker = PrivilegeBroker::new(sessions, dir.path());
        let request = {
            let broker = broker.clone();
            tokio::spawn(async move {
                broker
                    .request("id", &["-u".into()], "/tmp", "audit", "")
                    .await
            })
        };
        let id = next_request(&mut rx).await;
        broker
            .decide(&id, decision("alice-admin", "once"))
            .await
            .unwrap();
        assert!(request.await.unwrap().is_ok());

        let log = std::fs::read_to_string(dir.path().join("audit/privilege.log")).unwrap();
        let record: Value = serde_json::from_str(log.lines().last().unwrap()).unwrap();
        assert_eq!(record["result"], "executed");
        assert_eq!(record["approver"], "alice-admin");
        assert_eq!(record["request"]["taskId"], "");
        assert_eq!(record["request"]["sha256"].as_str().map(str::len), Some(64));
    }
}

#[cfg(test)]
mod socket_tests {
    use super::*;
    use mixum_protocol::ops::Cap;
    use mixum_protocol::token::Claims;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::mpsc;

    #[tokio::test]
    async fn socket_round_trip_requires_decision() {
        let sessions = Arc::new(SessionStore::new(16));
        let (tx, mut rx) = mpsc::channel(16);
        sessions
            .create(
                "socket-conn",
                Claims {
                    sub: "test-admin".into(),
                    caps: vec![Cap::Admin],
                    iat: 0,
                    exp: i64::MAX / 2,
                    jti: String::new(),
                    task_id: String::new(),
                    windows: vec![],
                },
                tx,
            )
            .await;
        let dir = tempfile::tempdir().unwrap();
        let broker = PrivilegeBroker::new(sessions, dir.path());
        let path = dir.path().join("sudo.sock");
        let path_string = path.to_string_lossy().into_owned();
        let server = {
            let broker = broker.clone();
            tokio::spawn(async move {
                let _ = broker
                    .serve_socket(
                        &path_string,
                        nix::unistd::Uid::current().as_raw(),
                        nix::unistd::Gid::current().as_raw(),
                    )
                    .await;
            })
        };
        tokio::time::sleep(Duration::from_millis(50)).await;
        let mut stream = tokio::net::UnixStream::connect(&path).await.unwrap();
        stream
            .write_all(
                br#"{"program":"id","args":["-u"],"cwd":"/tmp","reason":"socket test"}
"#,
            )
            .await
            .unwrap();
        let message = rx.recv().await.expect("request event");
        let event: Value = serde_json::from_str(message.to_text().unwrap()).expect("event json");
        let id = event["payload"]["id"].as_str().unwrap().to_owned();
        broker
            .decide(
                &id,
                Decision {
                    approve: true,
                    reason: "socket".into(),
                    scope: "once".into(),
                    approver: "test-admin".into(),
                },
            )
            .await
            .unwrap();
        let mut reply = String::new();
        stream.read_to_string(&mut reply).await.unwrap();
        let value: Value = serde_json::from_str(reply.trim()).unwrap();
        assert_eq!(value["ok"], true);
        assert_eq!(
            value["stdout"].as_str().unwrap().trim(),
            nix::unistd::Uid::current().as_raw().to_string()
        );
        server.abort();
    }
}
