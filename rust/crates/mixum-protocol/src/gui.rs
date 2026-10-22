//! 快照里的 GUI 会话元数据（OS 侧扩展，不影响文件清单/CAS 格式）。
//!
//! 只由 `LocalSandbox::spawn_gui` 路径记录：命令、参数、工作目录、环境变量
//! 与客户端 pid。restore 后按清单重放；外部驱动自己拉起的 GUI 进程不在其中。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GuiSession {
    #[serde(default)]
    pub app_id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
    /// 重放命令（绝对路径或 PATH 可解析的名字）。
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 快照时对应的客户端 pid；仅用于本地窗口匹配，跨设备无意义。
    #[serde(default)]
    pub pid: i32,
}

impl GuiSession {
    pub fn has_launch(&self) -> bool {
        !self.command.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_camel_case_and_defaults() {
        let session = GuiSession {
            app_id: "foot".into(),
            title: "shell".into(),
            width: 640,
            height: 400,
            command: "/usr/bin/foot".into(),
            args: vec!["--hold".into()],
            cwd: "/var/lib/mixum/work".into(),
            env: BTreeMap::from([("PATH".to_string(), "/usr/bin".to_string())]),
            pid: 42,
        };
        let json = serde_json::to_value(&session).unwrap();
        assert_eq!(json["appId"], "foot");
        assert_eq!(json["cwd"], "/var/lib/mixum/work");
        assert_eq!(json["env"]["PATH"], "/usr/bin");
        let back: GuiSession = serde_json::from_value(json).unwrap();
        assert_eq!(back, session);
    }
}
