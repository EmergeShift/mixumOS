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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HelloResponse {
    pub session_id: String,
    pub server: String,
    pub arch: String,
    pub kernel: String,
    pub caps: Vec<Cap>,
    pub versions: Vec<u32>,
    pub heartbeat: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotPolicy {
    pub pre: bool,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub on_failure: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TaskSubmitRequest {
    pub instruction: String,
    #[serde(default)]
    pub agent: String,
    #[serde(default)]
    pub runner: String,
    #[serde(default)]
    pub params: std::collections::HashMap<String, String>,
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
    #[serde(default, skip_serializing_if = "std::collections::HashMap::is_empty")]
    pub params: std::collections::HashMap<String, String>,
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
