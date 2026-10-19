//! server 使用的 payload 类型。字段名与 Go reference 的 wire 包保持驼峰。

use mixum_protocol::ops::Cap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HelloRequest {
    pub token: String,
    #[serde(default)]
    pub client: String,
    #[serde(default)]
    pub device: String,
    #[serde(default)]
    pub versions: Vec<String>,
    /// 客户端声明的 profile 提示;仅作参考,服务端按自身实现回告。
    #[serde(default)]
    pub profiles: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloResponse {
    pub session_id: String,
    pub server: String,
    pub arch: String,
    pub kernel: String,
    pub caps: Vec<Cap>,
    #[serde(default)]
    pub profiles: Vec<String>,
    pub versions: Vec<u32>,
    pub heartbeat: u32,
}

/// 任务前置快照策略(MAP tasks.md §Ops)。
/// MAP 的 `pre` 是 `"always" | "never"`;过渡期同时接受旧客户端的 bool。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPolicy {
    #[serde(default, deserialize_with = "deserialize_pre")]
    pub pre: bool,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub on_failure: String,
}

impl Default for SnapshotPolicy {
    fn default() -> Self {
        // policy 一旦出现,MAP 缺省 pre=always、kind=disk、onFailure=keep。
        Self {
            pre: true,
            kind: "disk".into(),
            on_failure: "keep".into(),
        }
    }
}

fn deserialize_pre<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Pre {
        Bool(bool),
        MapMode(String),
    }
    match Pre::deserialize(deserializer)? {
        Pre::Bool(value) => Ok(value),
        Pre::MapMode(mode) => match mode.as_str() {
            "always" => Ok(true),
            "never" => Ok(false),
            other => Err(serde::de::Error::custom(format!(
                "snapshotPolicy.pre 应为 always|never(旧值 bool 亦可),收到 {other}"
            ))),
        },
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TaskSubmitRequest {
    pub instruction: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub runner: String,
    /// MAP: 任意 JSON 对象(agent 私有参数)。
    #[serde(default)]
    pub params: serde_json::Map<String, Value>,
    #[serde(default)]
    pub idempotency_key: String,
    #[serde(default)]
    pub snapshot_policy: Option<SnapshotPolicy>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskView {
    pub id: String,
    pub instruction: String,
    pub agent: String,
    pub runner: String,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pre_snap_node: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Map::is_empty")]
    pub params: serde_json::Map<String, Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskEvent {
    pub task_id: String,
    #[serde(rename = "type")]
    pub event_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
    pub ts: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SnapNode {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    pub branch: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub kind: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub auto: bool,
    pub created_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
    pub size_bytes: i64,
    pub chunks: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub arch: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapTreeResponse {
    pub head: String,
    pub branch: String,
    pub nodes: Vec<SnapNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Window {
    pub id: u32,
    pub app_id: String,
    pub title: String,
    pub width: u32,
    pub height: u32,
    pub focused: bool,
    pub mode: String,
    /// 窗口流的实际编码(mixcomp 能力回告后由 broker 填写);缺省 jpeg
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub codec: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayListResponse {
    pub windows: Vec<Window>,
}
