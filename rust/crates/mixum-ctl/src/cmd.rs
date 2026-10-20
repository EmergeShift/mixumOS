//! CLI 参数与协议命令实现。

use clap::{Args, Parser, Subcommand};
use serde_json::{json, Value};

#[derive(Debug, Parser)]
#[command(name = "mixum-ctl", version, about = "mixumOS 主机侧控制工具")]
pub struct Cli {
    #[arg(long, global = true, env = "MIXUM_ADDR")]
    pub addr: Option<String>,
    #[arg(long, global = true, env = "MIXUM_TOKEN")]
    pub token: Option<String>,
    #[arg(long, global = true)]
    pub fingerprint: Option<String>,
    #[arg(long, global = true, default_value = "mixum-host")]
    pub device: String,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Connect(ConnectArgs),
    Task(TaskCommand),
    Snap(SnapCommand),
    Display(DisplayCommand),
    File(FileCommand),
    Serve(ServeArgs),
    Power(PowerArgs),
}

#[derive(Debug, Args)]
pub struct ConnectArgs {
    #[arg(long)]
    pub addr: String,
    #[arg(long)]
    pub token: Option<String>,
    #[arg(long)]
    pub token_path: Option<String>,
    #[arg(long)]
    pub fingerprint: Option<String>,
}

#[derive(Debug, Args)]
pub struct TaskCommand {
    #[command(subcommand)]
    pub command: TaskSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum TaskSubcommand {
    Submit {
        instruction: String,
        #[arg(long)]
        agent: Option<String>,
        #[arg(long = "param", value_parser = parse_param)]
        params: Vec<(String, String)>,
    },
    List,
    Get {
        id: String,
    },
    Cancel {
        id: String,
    },
}

#[derive(Debug, Args)]
pub struct SnapCommand {
    #[command(subcommand)]
    pub command: SnapSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum SnapSubcommand {
    Tree,
    Create {
        #[arg(long, default_value = "disk")]
        kind: String,
        #[arg(long)]
        label: Option<String>,
    },
    Restore {
        node: String,
        #[arg(long, default_value = "inplace")]
        strategy: String,
    },
    Fork {
        node: String,
        branch: String,
    },
    Push {
        node: String,
        #[arg(long)]
        remote: String,
        #[arg(long = "remote-token")]
        remote_token: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct DisplayCommand {
    #[command(subcommand)]
    pub command: DisplaySubcommand,
}

#[derive(Debug, Subcommand)]
pub enum DisplaySubcommand {
    List,
    Record {
        window: u32,
        #[arg(long)]
        out: String,
        #[arg(long, default_value_t = 3)]
        frames: usize,
    },
}

#[derive(Debug, Args)]
pub struct FileCommand {
    #[command(subcommand)]
    pub command: FileSubcommand,
}

#[derive(Debug, Subcommand)]
pub enum FileSubcommand {
    /// Upload a local file into the node's work_root/transfers directory.
    Put {
        path: String,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Download a file from the node's work_root.
    Get {
        path: String,
        #[arg(long)]
        out: Option<String>,
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    #[arg(long, default_value = "127.0.0.1:7602")]
    pub bind: String,
}

#[derive(Debug, Args)]
pub struct PowerArgs {
    #[arg(long)]
    pub simulate_sleep: bool,
    #[arg(long)]
    pub remote: Option<String>,
    #[arg(long = "remote-token")]
    pub remote_token: Option<String>,
    #[arg(long)]
    pub watch: bool,
}

fn parse_param(raw: &str) -> Result<(String, String), String> {
    raw.split_once('=')
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .ok_or_else(|| "参数必须是 k=v".to_owned())
}

pub fn params(values: &[(String, String)]) -> Value {
    values
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect()
}

pub fn submit_payload(
    instruction: &str,
    agent: &Option<String>,
    values: &[(String, String)],
) -> Value {
    let mut payload = json!({
        "instruction": instruction,
        "params": params(values),
        "snapshotPolicy": { "pre": true, "kind": "disk" }
    });
    if let Some(agent) = agent {
        payload["agent"] = Value::String(agent.clone());
    }
    payload
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn params_and_optional_agent_match_wire_shape() {
        let value = submit_payload(
            "执行",
            &Some("shell".into()),
            &[("cmd".into(), "true".into())],
        );
        assert_eq!(value["instruction"], "执行");
        assert_eq!(value["agent"], "shell");
        assert_eq!(value["params"]["cmd"], "true");
        assert_eq!(value["snapshotPolicy"]["pre"], true);
        assert_eq!(value["snapshotPolicy"]["kind"], "disk");
    }
}
