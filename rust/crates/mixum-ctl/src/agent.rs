//! 内置 agent 目录、一键安装与提示词模板。
//!
//! 安装通过 shell 任务在容器内执行：先按包管理器补依赖，再跑官方安装器，
//! 最后把提示词模板写到 $HOME/.mixum/agents/<name>/PROMPT.md。

use serde_json::json;

use crate::transport::{Client, Error};

pub struct AgentSpec {
    pub name: &'static str,
    pub summary: &'static str,
    /// exit 0 表示已安装。
    pub probe: &'static str,
    /// POSIX sh 安装脚本；`set -e` 由调用方统一加。
    pub install: &'static str,
}

const NPM_BOOTSTRAP: &str = r#"if ! command -v npm >/dev/null 2>&1; then
  if [ "$(id -u)" != "0" ]; then
    echo "缺少 npm；当前任务不是 root，无法用包管理器安装。请管理员预装 nodejs/npm。" >&2
    exit 1
  fi
  if command -v pacman >/dev/null 2>&1; then
    pacman -Sy --noconfirm nodejs npm
  elif command -v apk >/dev/null 2>&1; then
    apk add --no-cache nodejs npm
  elif command -v apt-get >/dev/null 2>&1; then
    apt-get update && apt-get install -y nodejs npm
  else
    echo "缺少 npm，且未识别包管理器" >&2
    exit 1
  fi
fi"#;

const CURL_BOOTSTRAP: &str = r#"if ! command -v curl >/dev/null 2>&1; then
  if [ "$(id -u)" != "0" ]; then
    echo "缺少 curl；当前任务不是 root，无法用包管理器安装。请管理员预装 curl。" >&2
    exit 1
  fi
  if command -v pacman >/dev/null 2>&1; then
    pacman -Sy --noconfirm curl
  elif command -v apk >/dev/null 2>&1; then
    apk add --no-cache curl
  elif command -v apt-get >/dev/null 2>&1; then
    apt-get update && apt-get install -y curl
  else
    echo "缺少 curl，且未识别包管理器" >&2
    exit 1
  fi
fi"#;

pub const AGENTS: &[AgentSpec] = &[
    AgentSpec {
        name: "claude-code",
        summary: "Anthropic Claude Code（npm 官方包）",
        probe: "command -v claude >/dev/null 2>&1 || [ -x \"$HOME/.local/bin/claude\" ]",
        install: r#"npm install -g --prefix "$HOME/.local" @anthropic-ai/claude-code
command -v claude || test -x "$HOME/.local/bin/claude""#,
    },
    AgentSpec {
        name: "codex",
        summary: "OpenAI Codex CLI（npm 官方包）",
        probe: "command -v codex >/dev/null 2>&1 || [ -x \"$HOME/.local/bin/codex\" ]",
        install: r#"npm install -g --prefix "$HOME/.local" @openai/codex
command -v codex || test -x "$HOME/.local/bin/codex""#,
    },
    AgentSpec {
        name: "gemini-cli",
        summary: "Google Gemini CLI（npm 官方包）",
        probe: "command -v gemini >/dev/null 2>&1 || [ -x \"$HOME/.local/bin/gemini\" ]",
        install: r#"npm install -g --prefix "$HOME/.local" @google/gemini-cli
command -v gemini || test -x "$HOME/.local/bin/gemini""#,
    },
    AgentSpec {
        name: "opencode",
        summary: "OpenCode（官方安装脚本）",
        probe: "command -v opencode >/dev/null 2>&1 || [ -x \"$HOME/.opencode/bin/opencode\" ]",
        install: r#"curl -fsSL https://opencode.ai/install | bash
test -x "$HOME/.opencode/bin/opencode""#,
    },
];

pub fn find(name: &str) -> Option<&'static AgentSpec> {
    AGENTS.iter().find(|agent| agent.name == name)
}

pub fn names() -> Vec<&'static str> {
    AGENTS.iter().map(|agent| agent.name).collect()
}

/// 复制到任意 agent 的提示词；`{{instruction}}` 由使用者替换。
pub fn template(name: &str) -> String {
    format!(
        r#"你正在 mixumOS 容器内工作（由 mixumOS 管理，agent 名称：{name}）。
工作区：/var/lib/mixum/work。任务开始时 mixumOS 会自动创建快照，长任务失败可回退重跑。
任务：{{{{instruction}}}}
要求：
- 先读代码和文档，再动手修改；一次只做一件事；
- 命令在容器内执行，网络与文件系统都限制在容器里；
- 不要修改 /var/lib/mixum、/run/mixum 下的运行时文件；
- 完成后给出改动清单与验证结果。"#
    )
}

fn prompt_write(spec: &AgentSpec) -> String {
    format!(
        "mkdir -p \"$HOME/.mixum/agents/{name}\"\ncat > \"$HOME/.mixum/agents/{name}/PROMPT.md\" <<'MIXUM_PROMPT_EOF'\n{template}\nMIXUM_PROMPT_EOF\n",
        name = spec.name,
        template = template(spec.name)
    )
}

/// `custom` 非空时替换官方安装命令（仍写入提示词），用于测试或私有源。
pub fn install_script_with(spec: &AgentSpec, custom: Option<&str>) -> String {
    let mut script = String::from("set -e\nexport PATH=\"$HOME/.local/bin:$HOME/.opencode/bin:$PATH\"\nmkdir -p \"$HOME/.local/bin\"\n");
    if let Some(custom) = custom {
        script.push_str(custom);
        script.push('\n');
    } else {
        if spec.name != "opencode" {
            script.push_str(NPM_BOOTSTRAP);
            script.push('\n');
        } else {
            script.push_str(CURL_BOOTSTRAP);
            script.push('\n');
        }
        script.push_str(spec.install);
        script.push('\n');
    }
    script.push_str(&prompt_write(spec));
    script
}

/// `agent list` 的探测脚本：每行输出 `installed <name>` 或 `missing <name>`。
pub fn probe_script() -> String {
    let mut script = String::new();
    for spec in AGENTS {
        script.push_str(&format!(
            "if {}; then echo 'installed {}'; else echo 'missing {}'; fi\n",
            spec.probe, spec.name, spec.name
        ));
    }
    script
}

pub struct TaskOutcome {
    pub state: String,
    pub error: Option<String>,
    pub logs: Vec<String>,
}

/// 提交 shell 任务并等待终态；`stream` 为真时实时打印日志行。
pub async fn run_shell(client: &Client, script: &str, stream: bool) -> Result<TaskOutcome, Error> {
    // 先订阅再提交，避免快任务丢事件。
    let _ = client.call("task.events", json!({})).await;
    let reply = client
        .call(
            "task.submit",
            json!({
                "instruction": script,
                "agent": "shell",
                "params": {},
                "snapshotPolicy": { "pre": false }
            }),
        )
        .await?;
    let task = reply.payload.unwrap_or_default();
    let id = task["id"]
        .as_str()
        .ok_or_else(|| Error::Protocol("task.submit 未返回任务 id".into()))?
        .to_owned();
    let mut logs = Vec::new();
    loop {
        let status = tokio::select! {
            event = client.next_event() => {
                if let Some(event) = event {
                    if event.op == "task.event" {
                        if let Some(payload) = event.payload {
                            if payload["taskId"].as_str() == Some(id.as_str())
                                && payload["type"].as_str() == Some("log")
                            {
                                if let Some(line) = payload["data"]["line"].as_str() {
                                    if stream {
                                        println!("{line}");
                                    }
                                    logs.push(line.to_owned());
                                }
                            }
                        }
                    }
                }
                continue;
            }
            status = client.call("task.get", json!({"id": id})) => status?,
        };
        let payload = status.payload.unwrap_or_default();
        let state = payload["state"].as_str().unwrap_or("unknown").to_owned();
        if matches!(state.as_str(), "completed" | "failed" | "canceled") {
            return Ok(TaskOutcome {
                state,
                error: payload["error"].as_str().map(str::to_owned),
                logs,
            });
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_is_unique_and_known() {
        let names = names();
        for name in ["claude-code", "codex", "gemini-cli", "opencode"] {
            assert!(names.contains(&name), "缺少 {name}");
            assert!(find(name).is_some());
        }
        let mut sorted = names.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), names.len(), "agent 名称重复");
    }

    #[test]
    fn template_exposes_instruction_placeholder() {
        let text = template("codex");
        assert!(text.contains("{{instruction}}"));
        assert!(text.contains("codex"));
        assert!(text.contains("/var/lib/mixum/work"));
    }

    #[test]
    fn install_script_writes_prompt_and_installs() {
        let spec = find("claude-code").unwrap();
        let script = install_script_with(spec, None);
        assert!(script.starts_with("set -e\n"));
        assert!(
            script.contains("npm install -g --prefix \"$HOME/.local\" @anthropic-ai/claude-code")
        );
        assert!(script.contains("$HOME/.mixum/agents/claude-code/PROMPT.md"));
        assert!(script.contains("MIXUM_PROMPT_EOF"));
        let opencode = install_script_with(find("opencode").unwrap(), None);
        assert!(opencode.contains("curl -fsSL https://opencode.ai/install"));
        assert!(!opencode.contains("npm install -g @anthropic-ai/claude-code"));
    }

    #[test]
    fn probe_script_covers_every_agent() {
        let script = probe_script();
        for name in names() {
            assert!(script.contains(&format!("installed {name}")));
            assert!(script.contains(&format!("missing {name}")));
        }
    }
}
