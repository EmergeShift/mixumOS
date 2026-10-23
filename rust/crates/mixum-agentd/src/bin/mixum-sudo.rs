//! `sudo` 客户端：只把 argv 交给 agentd 审批，不自行提权。
//!
//! 安装为 `/usr/local/bin/sudo`，运行身份是 mixum；agentd root supervisor
//! 负责审批后的 root 执行。拒绝/超时返回 126。

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::process::exit;

fn main() {
    let mut argv = std::env::args().skip(1);
    let mut program = match argv.next() {
        Some(value) => value,
        None => {
            eprintln!("sudo: 缺少命令");
            exit(2);
        }
    };
    if program == "--" {
        program = match argv.next() {
            Some(value) => value,
            None => {
                eprintln!("sudo: 缺少命令");
                exit(2);
            }
        };
    }
    if program.starts_with('-') {
        eprintln!("sudo: mixum sudo 只接受单条命令，不支持 sudo 选项: {program}");
        exit(2);
    }
    let args: Vec<String> = argv.collect();
    let cwd = std::env::current_dir()
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "/var/lib/mixum/work".into());
    let reason = std::env::var("MIXUM_SUDO_REASON").unwrap_or_default();
    let task_id = std::env::var("MIXUM_TASK_ID").unwrap_or_default();
    let request = serde_json::json!({
        "program": program,
        "args": args,
        "cwd": cwd,
        "reason": reason,
        "taskId": task_id,
    });
    let path = std::env::var("MIXUM_SUDO_SOCKET").unwrap_or_else(|_| "/run/mixum/sudo.sock".into());
    let mut stream = match UnixStream::connect(&path) {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("sudo: 连接审批服务失败({path}): {error}");
            exit(126);
        }
    };
    if let Err(error) = writeln!(stream, "{request}") {
        eprintln!("sudo: 发送审批请求失败: {error}");
        exit(126);
    }
    let mut reply = String::new();
    if let Err(error) = stream.read_to_string(&mut reply) {
        eprintln!("sudo: 读取审批结果失败: {error}");
        exit(126);
    }
    let value: serde_json::Value = match serde_json::from_str(reply.trim()) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("sudo: 审批结果无效: {error}");
            exit(126);
        }
    };
    if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
        let message = value
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("审批失败");
        eprintln!("sudo: {message}");
        exit(126);
    }
    if let Some(stdout) = value.get("stdout").and_then(serde_json::Value::as_str) {
        print!("{stdout}");
    }
    if let Some(stderr) = value.get("stderr").and_then(serde_json::Value::as_str) {
        eprint!("{stderr}");
    }
    exit(
        value
            .get("exitCode")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(1) as i32,
    );
}
