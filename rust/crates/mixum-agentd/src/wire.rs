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

// 任务侧视图类型整体来自 mixum-agent-driver crate(独立发布的驱动 SPI),
// 这里再导出保持 API 序列化形状与既有引用路径不变。
pub use mixum_agent_driver::{SnapshotPolicy, TaskEvent, TaskView};

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
