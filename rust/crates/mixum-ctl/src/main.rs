mod cmd;
mod power;
mod transport;
mod view;

use clap::Parser;
use cmd::{Cli, Command, SnapSubcommand, TaskSubcommand};
use std::path::PathBuf;
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
        if args.watch {
            let configured_remote = power::load()?.remote;
            let options = connection_options(&cli, None, None, None)?;
            return power::watch_sleep(
                options,
                args.remote.clone().or(configured_remote),
                args.remote_token.clone(),
            )
            .await;
        }
        if !args.simulate_sleep {
            println!("{}", power::PLATFORM_NOTES);
            return Ok(());
        }
        let configured_remote = power::load()?.remote;
        let options = connection_options(&cli, None, None, None)?;
        return power::simulate_sleep(
            options,
            args.remote.clone().or(configured_remote),
            args.remote_token.clone(),
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
        Command::Serve(args) => view::serve(client, &args.bind).await?,
        Command::Connect(_) | Command::Power(_) => unreachable!(),
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
        .or(config.remote)
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
    Ok(ConnectionOptions {
        addr,
        token,
        fingerprint: fingerprint
            .map(ToOwned::to_owned)
            .or(cli.fingerprint.clone())
            .or(config.fingerprint),
        known_hosts: known_hosts_path(),
        device: cli.device.clone(),
    })
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
