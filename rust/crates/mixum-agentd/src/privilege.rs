//! 容器内 sudo 请求：agent 只能提交单条 argv，由 Mart/CLI 审批后 root 执行。
//!
//! 关键约束：
//! - 不提供 root shell，只执行一次精确 argv；
//! - 审批事件只发给有 Admin 能力的已连接会话；
//! - 无审批人时超时拒绝，绝不默认放行；
//! - 审计只记录命令与结果，不记录环境变量中的秘密。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use mixum_protocol::envelope::{new_id, Envelope};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{oneshot, Mutex};

use crate::server::SessionStore;

const REQUEST_TTL: Duration = Duration::from_secs(120);
const EXEC_TIMEOUT: Duration = Duration::from_secs(300);
const OUTPUT_LIMIT: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Decision {
    pub approve: bool,
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Outcome {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

struct Pending {
    tx: oneshot::Sender<Decision>,
}

pub struct PrivilegeBroker {
    sessions: Arc<SessionStore>,
    pending: Mutex<HashMap<String, Pending>>,
    audit_path: PathBuf,
}

impl PrivilegeBroker {
    pub fn new(sessions: Arc<SessionStore>, data_dir: impl AsRef<Path>) -> Arc<Self> {
        let audit_path = data_dir.as_ref().join("audit/privilege.log");
        Arc::new(Self {
            sessions,
            pending: Mutex::new(HashMap::new()),
            audit_path,
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
        if program.trim().is_empty() || program.contains('\0') {
            return Err("program 为空或非法".into());
        }
        if args.iter().any(|arg| arg.contains('\0')) {
            return Err("参数含 NUL".into());
        }
        let id = new_id();
        let created_at = now_ms();
        let risk = classify(program, args);
        let event = json!({
            "id": id,
            "taskId": task_id,
            "program": program,
            "args": args,
            "cwd": cwd,
            "reason": reason,
            "risk": risk,
            "createdAt": created_at,
            "expiresAt": created_at + REQUEST_TTL.as_millis() as i64,
        });
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id.clone(), Pending { tx });
        self.sessions
            .broadcast_admin(Envelope::new("privilege.request", Some(event.clone())))
            .await;
        let decision = match tokio::time::timeout(REQUEST_TTL, rx).await {
            Ok(Ok(decision)) => decision,
            _ => {
                self.pending.lock().await.remove(&id);
                self.audit(&id, &event, "timeout", None).await;
                return Err("sudo 审批超时".into());
            }
        };
        if !decision.approve {
            self.audit(&id, &event, "denied", Some(&decision.reason))
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
        let outcome = self.execute(&event).await;
        self.audit(&id, &event, "executed", Some(&decision.reason))
            .await;
        outcome
    }

    /// 由 Mart/CLI 的 `privilege.decision` 调用。
    pub async fn decide(&self, id: &str, decision: Decision) -> Result<(), String> {
        let Some(pending) = self.pending.lock().await.remove(id) else {
            return Err("请求不存在或已过期".into());
        };
        pending
            .tx
            .send(decision)
            .map_err(|_| "审批请求已关闭".to_string())
    }

    async fn execute(&self, event: &Value) -> Result<Outcome, String> {
        let program = event
            .get("program")
            .and_then(Value::as_str)
            .ok_or_else(|| "缺少 program".to_string())?;
        let args: Vec<String> = event
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
        let cwd = event
            .get("cwd")
            .and_then(Value::as_str)
            .unwrap_or("/var/lib/mixum/work");
        let mut cmd = tokio::process::Command::new(program);
        cmd.args(&args)
            .current_dir(cwd)
            .env_clear()
            .env(
                "PATH",
                "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
            )
            .env("HOME", "/root")
            .env("USER", "root")
            .env("LOGNAME", "root")
            .env("SHELL", "/bin/sh")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = cmd
            .spawn()
            .map_err(|error| format!("启动 root 命令: {error}"))?;
        let output = tokio::time::timeout(EXEC_TIMEOUT, child.wait_with_output())
            .await
            .map_err(|_| "root 命令超时".to_string())?
            .map_err(|error| format!("root 命令失败: {error}"))?;
        Ok(Outcome {
            exit_code: output.status.code().unwrap_or(-1),
            stdout: truncate(&output.stdout),
            stderr: truncate(&output.stderr),
        })
    }

    async fn audit(&self, id: &str, event: &Value, result: &str, reason: Option<&str>) {
        if let Some(parent) = self.audit_path.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let record = json!({
            "ts": now_ms(),
            "id": id,
            "result": result,
            "reason": reason.unwrap_or_default(),
            "request": event,
        });
        if let Ok(mut file) = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.audit_path)
            .await
        {
            let _ = file.write_all(format!("{record}\n").as_bytes()).await;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = tokio::fs::set_permissions(
                    &self.audit_path,
                    std::fs::Permissions::from_mode(0o600),
                )
                .await;
            }
        }
    }

    /// 监听 `/run/mixum/sudo.sock`；只接受 mixum/root uid。
    pub async fn serve_socket(
        self: Arc<Self>,
        path: &str,
        allowed_uid: u32,
        allowed_gid: u32,
    ) -> std::io::Result<()> {
        let _ = tokio::fs::remove_file(path).await;
        let listener = UnixListener::bind(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = tokio::fs::set_permissions(path, std::fs::Permissions::from_mode(0o660)).await;
            use std::os::unix::fs::chown;
            let _ = chown(std::path::Path::new(path), None, Some(allowed_gid));
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
        let _ = allowed_uid;
        #[cfg(target_os = "linux")]
        {
            if let Ok(cred) = stream.peer_cred() {
                let uid = cred.uid();
                if uid != 0 && uid != allowed_uid {
                    return Ok(());
                }
            }
        }
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
        let reply = match self.request(program, &args, cwd, reason, task_id).await {
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

fn truncate(bytes: &[u8]) -> String {
    let slice = if bytes.len() > OUTPUT_LIMIT {
        &bytes[..OUTPUT_LIMIT]
    } else {
        bytes
    };
    let mut text = String::from_utf8_lossy(slice).into_owned();
    if bytes.len() > OUTPUT_LIMIT {
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
                },
            )
            .await
            .expect("decide");
        assert!(request.await.unwrap().is_err());
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
