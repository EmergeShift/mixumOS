mod agent;
mod cmd;
mod power;
mod transport;
mod view;

use clap::Parser;
use cmd::{AgentSubcommand, Cli, Command, FileSubcommand, SnapSubcommand, TaskSubcommand};
use mixum_protocol::frames::{marshal_file, unmarshal_file};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use transport::{known_hosts_path, Client, ConnectionOptions, Error};

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("mixum-ctl: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Error> {
    let cli = Cli::parse();
    if let Command::Connect(args) = &cli.command {
        let mut config = power::load()?;
        config.remote = Some(args.addr.clone());
        config.token_path = args.token_path.clone().or(config.token_path);
        config.fingerprint = args.fingerprint.clone().or(config.fingerprint);
        power::save(&config)?;
        if let Some(token) = args.token.as_deref().or(cli.token.as_deref()) {
            let options = connection_options(
                &cli,
                Some(&args.addr),
                Some(token),
                args.fingerprint.as_deref(),
            )?;
            let client = Client::connect(options).await?;
            println!("已连接 {}，证书指纹 {}", args.addr, client.fingerprint);
        } else {
            println!("已保存设备配置 {}", args.addr);
        }
        return Ok(());
    }
    if let Command::Power(args) = &cli.command {
        if args.install || args.remove || args.print {
            return power::systemd_manage(args.install, args.remove, args.print);
        }
        let configured_remote = power::load()?.remote;
        let options = connection_options(&cli, None, None, None)?;
        let watch_deadline = if args.deadline_secs == 0 {
            power::WATCH_DEADLINE
        } else {
            std::time::Duration::from_secs(args.deadline_secs)
        };
        let run_deadline = std::time::Duration::from_secs(args.deadline_secs);
        if args.watch {
            return power::watch_sleep(
                options,
                args.remote.clone().or(configured_remote),
                args.remote_token.clone(),
                watch_deadline,
            )
            .await;
        }
        if !args.simulate_sleep {
            println!("{}", power::PLATFORM_NOTES);
            return Ok(());
        }
        return power::simulate_sleep(
            options,
            args.remote.clone().or(configured_remote),
            args.remote_token.clone(),
            run_deadline,
        )
        .await;
    }
    let client = Client::connect(connection_options(&cli, None, None, None)?).await?;
    match cli.command {
        Command::Task(task) => task_command(&client, task.command).await?,
        Command::Snap(snap) => {
            snap_command(&client, snap.command, Some(client.token().to_owned())).await?
        }
        Command::Display(display) => match display.command {
            cmd::DisplaySubcommand::List => {
                let reply = client.call("display.list", serde_json::json!({})).await?;
                print_json(reply.payload);
            }
            cmd::DisplaySubcommand::Record {
                window,
                out,
                frames,
            } => {
                let n = view::record(&client, window, PathBuf::from(out).as_path(), frames).await?;
                println!("已保存 {n} 帧");
            }
        },
        Command::File(file) => file_command(&client, file.command).await?,
        Command::Serve(args) => view::serve(client, &args.bind).await?,
        Command::Agent(args) => agent_command(&client, args.command).await?,
        Command::Connect(_) | Command::Power(_) => unreachable!(),
    }
    Ok(())
}

async fn file_command(client: &Client, command: FileSubcommand) -> Result<(), Error> {
    match command {
        FileSubcommand::Put { path, name, json } => file_put(client, &path, name, json).await,
        FileSubcommand::Get { path, out, json } => {
            file_get(client, &path, out.as_deref(), json).await
        }
    }
}

async fn file_put(
    client: &Client,
    path: &str,
    name: Option<String>,
    json_out: bool,
) -> Result<(), Error> {
    let local = Path::new(path);
    let name = name
        .or_else(|| local.file_name().map(|name| name.to_string_lossy().into_owned()))
        .ok_or_else(|| Error::Config("本地路径没有文件名".into()))?;
    let metadata = tokio::fs::metadata(local).await?;
    if !metadata.is_file() {
        return Err(Error::Config("不是普通文件".into()));
    }
    let reply = client
        .call(
            "file.put",
            json!({"name": name, "size": metadata.len()}),
        )
        .await?;
    let started = reply.payload.unwrap_or_default();
    let transfer = started["transfer"]
        .as_u64()
        .ok_or_else(|| Error::Protocol("file.put 未返回 transfer".into()))?
        as u32;
    let remote = started["path"]
        .as_str()
        .ok_or_else(|| Error::Protocol("file.put 未返回 path".into()))?
        .to_owned();
    let mut file = tokio::fs::File::open(local).await?;
    let mut buffer = vec![0u8; 256 * 1024];
    let mut offset = 0u64;
    let mut sha = Sha256::new();
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        sha.update(&buffer[..read]);
        client
            .send_binary(marshal_file(transfer, offset, &buffer[..read]))
            .await?;
        offset += read as u64;
    }
    let digest = sha.finalize();
    let hash = digest.iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    let committed = client
        .call(
            "file.put.commit",
            json!({"transfer": transfer, "sha256": hash}),
        )
        .await?;
    let payload = committed.payload.unwrap_or_default();
    if json_out {
        print_json(Some(payload));
    } else {
        println!("{remote}");
    }
    Ok(())
}

async fn file_get(
    client: &Client,
    remote: &str,
    out: Option<&str>,
    json_out: bool,
) -> Result<(), Error> {
    let mut frames = client.subscribe_frames();
    let reply = client
        .call("file.get", json!({"path": remote}))
        .await?;
    let started = reply.payload.unwrap_or_default();
    let transfer = started["transfer"]
        .as_u64()
        .ok_or_else(|| Error::Protocol("file.get 未返回 transfer".into()))?
        as u32;
    let size = started["size"]
        .as_u64()
        .ok_or_else(|| Error::Protocol("file.get 未返回 size".into()))?;
    let remote_name = started["name"].as_str().unwrap_or("file");
    let local = out
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(remote_name));
    let mut file = tokio::fs::File::create(&local).await?;
    let mut received = 0u64;
    let deadline = Instant::now() + Duration::from_secs(300);
    while received < size {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(Error::Timeout(format!("file.get {remote}")));
        }
        let frame = tokio::time::timeout(remaining, frames.recv())
            .await
            .map_err(|_| Error::Timeout(format!("file.get {remote}")))?
            .map_err(|error| Error::Protocol(format!("文件帧: {error}")))?;
        let Ok((id, offset, data)) = unmarshal_file(&frame.0) else {
            continue;
        };
        if id != transfer {
            continue;
        }
        if offset != received {
            return Err(Error::Protocol(format!(
                "文件块偏移错误: got {offset} want {received}"
            )));
        }
        file.write_all(data).await?;
        received += data.len() as u64;
    }
    file.flush().await?;
    if json_out {
        print_json(Some(json!({
            "path": local.to_string_lossy(),
            "remote": remote,
            "size": received,
        })));
    } else {
        println!("{}", local.to_string_lossy());
    }
    Ok(())
}

fn connection_options(
    cli: &Cli,
    addr: Option<&String>,
    token: Option<&str>,
    fingerprint: Option<&str>,
) -> Result<ConnectionOptions, Error> {
    let config = power::load()?;
    let addr = addr
        .cloned()
        .or_else(|| cli.addr.clone())
        .or_else(|| config.remote.clone())
        .ok_or_else(|| Error::Config("请提供 --addr 或先执行 connect".into()))?;
    let token_path = config
        .token_path
        .or_else(|| std::env::var("MIXUM_TOKEN_PATH").ok());
    let token = token
        .map(ToOwned::to_owned)
        .or(cli.token.clone())
        .or_else(|| {
            token_path
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok().map(|s| s.trim().to_owned()))
        })
        .ok_or_else(|| Error::Config("请提供 --token 或配置 token_path".into()))?;
    let pinned = transport::known_pin(&known_hosts_path(), &addr);
    let fingerprint = resolve_fingerprint(
        fingerprint
            .map(ToOwned::to_owned)
            .or(cli.fingerprint.clone()),
        pinned,
        config.fingerprint,
        config.remote.as_deref(),
        &addr,
    );
    Ok(ConnectionOptions {
        addr,
        token,
        fingerprint,
        known_hosts: known_hosts_path(),
        device: cli.device.clone(),
    })
}

/// 指纹解析顺序:显式 --fingerprint、known-hosts 按地址记录、config 中已配对
/// 设备的指纹。config 指纹只对 config.remote 生效,避免多节点场景下
/// 一个地址的配对信息污染另一个地址的校验与首次学习。
fn resolve_fingerprint(
    explicit: Option<String>,
    pinned: Option<String>,
    configured: Option<String>,
    config_remote: Option<&str>,
    addr: &str,
) -> Option<String> {
    explicit.or(pinned).or_else(|| {
        if config_remote == Some(addr) {
            configured
        } else {
            None
        }
    })
}

async fn agent_command(client: &Client, command: AgentSubcommand) -> Result<(), Error> {
    match command {
        AgentSubcommand::List { json } => {
            let outcome = agent::run_shell(client, &agent::probe_script(), false).await?;
            if outcome.state != "completed" {
                return Err(Error::Remote {
                    code: outcome.state,
                    message: outcome.error.unwrap_or_else(|| "探测失败".into()),
                });
            }
            let installed = |name: &str| {
                outcome
                    .logs
                    .iter()
                    .any(|line| line == &format!("installed {name}"))
            };
            if json {
                let items: Vec<_> = agent::AGENTS
                    .iter()
                    .map(|spec| {
                        json!({
                            "name": spec.name,
                            "summary": spec.summary,
                            "installed": installed(spec.name)
                        })
                    })
                    .collect();
                print_json(Some(json!(items)));
            } else {
                for spec in agent::AGENTS {
                    println!(
                        "{:<12} {:<6} {}",
                        spec.name,
                        if installed(spec.name) { "已安装" } else { "未安装" },
                        spec.summary
                    );
                }
            }
        }
        AgentSubcommand::Install {
            name,
            dry_run,
            cmd,
            json,
        } => {
            let spec = agent::find(&name).ok_or_else(|| {
                Error::Config(format!(
                    "未知 agent: {name}（可用: {}）",
                    agent::names().join(", ")
                ))
            })?;
            let script = agent::install_script_with(spec, cmd.as_deref());
            if dry_run {
                print!("{script}");
                return Ok(());
            }
            let outcome = agent::run_shell(client, &script, true).await?;
            if json {
                print_json(Some(json!({
                    "name": spec.name,
                    "state": outcome.state,
                    "error": outcome.error,
                    "logs": outcome.logs
                })));
            }
            if outcome.state != "completed" {
                return Err(Error::Remote {
                    code: outcome.state,
                    message: outcome.error.unwrap_or_else(|| "安装失败".into()),
                });
            }
            if !json {
                println!(
                    "已安装 {}；提示词模板：~/.mixum/agents/{}/PROMPT.md",
                    spec.name, spec.name
                );
            }
        }
        AgentSubcommand::Template { name } => {
            let spec = agent::find(&name).ok_or_else(|| {
                Error::Config(format!(
                    "未知 agent: {name}（可用: {}）",
                    agent::names().join(", ")
                ))
            })?;
            println!("{}", agent::template(spec.name));
        }
    }
    Ok(())
}

async fn task_command(client: &Client, command: TaskSubcommand) -> Result<(), Error> {
    match command {
        TaskSubcommand::Submit {
            instruction,
            agent,
            params,
        } => {
            // 先订阅再提交，避免极快任务在 task.submit 与 task.events 之间丢事件。
            let _ = client.call("task.events", serde_json::json!({})).await;
            let reply = client
                .call(
                    "task.submit",
                    cmd::submit_payload(&instruction, &agent, &params),
                )
                .await?;
            let task = reply.payload.unwrap_or_default();
            let id = task["id"]
                .as_str()
                .ok_or_else(|| Error::Protocol("task.submit 未返回任务 id".into()))?
                .to_owned();
            print_json(Some(task));
            loop {
                let status = tokio::select! {
                    event = client.next_event() => {
                        if let Some(event) = event {
                            if event.op == "task.event" {
                                if let Some(payload) = event.payload {
                                    println!("event={}", serde_json::to_string(&payload).unwrap_or_default());
                                }
                            }
                        }
                        continue;
                    }
                    status = client.call("task.get", serde_json::json!({"id": id})) => status?,
                };
                let payload = status.payload.unwrap_or_default();
                let state = payload["state"].as_str().unwrap_or("unknown");
                println!("state={state}");
                if matches!(state, "completed" | "failed" | "canceled") {
                    if state != "completed" {
                        return Err(Error::Remote {
                            code: state.to_owned(),
                            message: payload["error"].as_str().unwrap_or("任务未完成").to_owned(),
                        });
                    }
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            }
        }
        TaskSubcommand::List => print_json(
            client
                .call("task.list", serde_json::json!({}))
                .await?
                .payload,
        ),
        TaskSubcommand::Get { id } => print_json(
            client
                .call("task.get", serde_json::json!({"id": id}))
                .await?
                .payload,
        ),
        TaskSubcommand::Cancel { id } => print_json(
            client
                .call("task.cancel", serde_json::json!({"id": id}))
                .await?
                .payload,
        ),
    }
    Ok(())
}

async fn snap_command(
    client: &Client,
    command: SnapSubcommand,
    default_token: Option<String>,
) -> Result<(), Error> {
    let (op, payload) = match command {
        SnapSubcommand::Tree => ("snap.tree", serde_json::json!({})),
        SnapSubcommand::Create { kind, label } => (
            "snap.create",
            serde_json::json!({"kind": kind, "label": label}),
        ),
        SnapSubcommand::Restore { node, strategy } => (
            "snap.restore",
            serde_json::json!({"node": node, "strategy": strategy}),
        ),
        SnapSubcommand::Fork { node, branch } => (
            "snap.fork",
            serde_json::json!({"node": node, "branch": branch}),
        ),
        SnapSubcommand::Push {
            node,
            remote,
            remote_token,
        } => (
            "snap.push",
            serde_json::json!({
                "node": node,
                "remote": remote,
                "token": remote_token.or(default_token)
            }),
        ),
    };
    print_json(client.call(op, payload).await?.payload);
    Ok(())
}

fn print_json(value: Option<serde_json::Value>) {
    println!(
        "{}",
        serde_json::to_string_pretty(&value.unwrap_or(serde_json::Value::Null)).expect("JSON 输出")
    );
}

#[cfg(test)]
mod tests {
    use super::resolve_fingerprint;

    const ADDR_A: &str = "wss://127.0.0.1:17601";
    const ADDR_B: &str = "wss://127.0.0.1:17602";

    #[test]
    fn explicit_fingerprint_wins() {
        let resolved = resolve_fingerprint(
            Some("aa".into()),
            Some("bb".into()),
            Some("cc".into()),
            Some(ADDR_A),
            ADDR_A,
        );
        assert_eq!(resolved.as_deref(), Some("aa"));
    }

    #[test]
    fn per_address_pin_beats_configured_device() {
        // known-hosts 中 A 地址的记录优先于 config 里 B 设备的指纹。
        let resolved = resolve_fingerprint(
            None,
            Some("bb".into()),
            Some("cc".into()),
            Some(ADDR_B),
            ADDR_A,
        );
        assert_eq!(resolved.as_deref(), Some("bb"));
    }

    #[test]
    fn configured_fingerprint_applies_only_to_paired_remote() {
        let paired = resolve_fingerprint(None, None, Some("cc".into()), Some(ADDR_A), ADDR_A);
        assert_eq!(paired.as_deref(), Some("cc"));
        let other = resolve_fingerprint(None, None, Some("cc".into()), Some(ADDR_A), ADDR_B);
        assert_eq!(other, None);
    }
}
