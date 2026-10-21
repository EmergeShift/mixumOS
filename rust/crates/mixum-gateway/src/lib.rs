//! gateway 路由核心(设计见 docs/gateway.md)。
//!
//! 三类连接:节点(mixumd 出站注册,gateway.register)、客户端(hello 握手,
//! 信封按 envelope.node 路由)、无。本模块只实现与网络无关的路由核心,
//! WSS 泵在 bin 里。
//!
//! 节点通道私有约定(非公有协议):
//! - gateway→节点文本:信封 + `gatewaySession` 字段(gateway 为每个客户端
//!   连接分配的会话 id);`op=gateway.session.open/close` 由 relay 消费,
//!   按会话开/关到本机 mixumd 的回环连接;
//! - 节点→gateway 文本:同上带 `gatewaySession`,gateway 据此回送客户端;
//! - 节点→gateway 二进制:`u32 LE 会话 id 长度 + 会话 id 字节 + 原始帧`,
//!   帧本身(0x01 + 27 字节头)不改。

pub mod identity;
pub mod mesh;
pub mod registry;
pub mod registry_k8s;

pub use mesh::MeshHub;
pub use registry::{MemoryRegistry, NodeRecord, NodeRegistry, RegistryError};

use async_trait::async_trait;
use mixum_protocol::ops::{Cap, ErrCode};
use mixum_protocol::token::{Claims, Signer};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex, RwLock};

/// 单条连接的出站口(网络层把 Text/Binary 写回对端)。
pub type OutTx = mpsc::Sender<Outbound>;

#[derive(Debug, Clone)]
pub enum Outbound {
    Text(String),
    Binary(Vec<u8>),
    /// 连接关闭前发一条 Close(可选)。
    Close,
}

struct ClientConn {
    out: OutTx,
    sub: String,
    caps: Vec<Cap>,
    /// 该客户端已路由过的节点集合(信息性)。
    nodes: HashSet<String>,
}

#[derive(Debug, Clone)]
pub struct PairCode {
    pub user_sub: String,
    pub expires_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceEntry {
    pub name: String,
    pub sub: String,
    /// 签发该设备的用户主体;列表/吊销按 owner 隔离。
    pub owner: String,
    pub iat: i64,
}

/// 跨副本 mesh 会话 id 前缀(owner 侧据此把节点应答路由回请求方)。
pub const MESH_SESSION_PREFIX: &str = "ms-";

pub fn is_mesh_session(session: &str) -> bool {
    session.starts_with(MESH_SESSION_PREFIX)
}

/// 跨副本转发(M3b)。实现见 `mesh.rs`;gateway 核心只依赖这个窄接口。
#[async_trait]
pub trait MeshRouter: Send + Sync {
    /// 把一条客户端信封转给节点 owner 副本;成功后返回本次 mesh 会话 id
    /// (用于节点应答的反向寻址)。
    async fn forward_text(
        &self,
        client_session: &str,
        record: &NodeRecord,
        text: &str,
    ) -> Result<String, String>;
    /// mesh 会话 → 本地客户端会话(本副本是请求发起方时)。
    async fn client_session(&self, mesh_session: &str) -> Option<String>;
    /// 节点应答回送发起方副本(本副本是节点 owner 时)。
    async fn reply_text(&self, mesh_session: &str, text: &str);
    async fn reply_binary(&self, mesh_session: &str, frame: &[u8]);
    /// 客户端断开:关闭它在该 owner 上的所有 mesh 会话。
    async fn close_client(&self, client_session: &str);
    /// 节点 owner 侧收到 mesh.close:关闭回环会话。
    async fn close_inbound(&self, mesh_session: &str);
}

pub struct Gateway {
    signer: Signer,
    /// 节点注册表(元数据):内存(M1/M2)或 k8s Lease(M3)。
    nodes: Arc<dyn NodeRegistry>,
    /// 本副本持有的节点出站连接。
    local: RwLock<HashMap<String, OutTx>>,
    clients: Mutex<HashMap<String, ClientConn>>,
    revoked: RwLock<HashSet<String>>,
    /// 配对码 → 用户主体(5 分钟有效,单次使用)。
    pairs: Mutex<HashMap<String, PairCode>>,
    /// 已签发设备(jti → 条目);吊销即移入 revoked 并移除。
    devices: Mutex<HashMap<String, DeviceEntry>>,
    /// 配对码失败尝试(来源 → 时间戳),用于按来源限速。
    pair_failures: Mutex<HashMap<String, Vec<i64>>>,
    /// 持久化状态文件(--data-dir/gateway-state.json);None 表示不持久化。
    state_path: std::sync::Mutex<Option<std::path::PathBuf>>,
    /// 最近一次签发的 bootstrap jti:重启时轮换(旧令牌立即失效)。
    bootstrap_jti: std::sync::Mutex<Option<String>>,
    /// 本 gateway 实例标识(单副本 `local`,k8s 为 Pod 名)。
    instance_id: String,
    /// 本实例的 mesh 地址(M3b 跨副本寻址)。
    advertise: String,
    /// 跨副本转发器;单副本为 None。
    mesh: Option<Arc<dyn MeshRouter>>,
    /// owner 侧已打开的 mesh 回环会话(mesh session → 节点名)。
    mesh_open: Mutex<HashMap<String, String>>,
    seq: AtomicU64,
    started_at: i64,
}

/// RFC 0003:节点名 1..=64 字符、不含冒号(envelope.node 与注册名同规则)。
pub fn valid_node_name(name: &str) -> bool {
    // RFC 0003 限制的是字符数;按字节数会让中文名提前触发上限。
    !name.is_empty() && name.chars().count() <= 64 && !name.contains(':')
}

/// 解析 envelope.node:缺省返回 `None`(客户端可自动路由到唯一在线节点);
/// 出现但不是合法节点名(空串/非字符串/null)一律返回 `Err`,由调用方回
/// `bad_request`——不静默降级为自动路由。
fn envelope_node(env: &Value) -> Result<Option<&str>, ()> {
    match env.get("node") {
        None => Ok(None),
        Some(Value::String(name)) if valid_node_name(name) => Ok(Some(name)),
        Some(_) => Err(()),
    }
}

/// 节点注册(连接)失败类别:bin 层据此映射 RFC 0003 错误码,不解析文本。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// 节点名不合法 → `bad_request`。
    InvalidName(String),
    /// 同名节点在线 → `conflict`。
    Conflict(String),
    /// 注册表/租约后端故障 → `internal`。
    Internal(String),
}

impl std::fmt::Display for ConnectError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidName(message) | Self::Conflict(message) | Self::Internal(message) => {
                f.write_str(message)
            }
        }
    }
}

impl std::error::Error for ConnectError {}

impl From<RegistryError> for ConnectError {
    fn from(err: RegistryError) -> Self {
        match err {
            RegistryError::Conflict(message) => Self::Conflict(message),
            RegistryError::Internal(message) => Self::Internal(message),
        }
    }
}

/// 配对限速桶键:优先取 IP,去掉 ephemeral port;非 SocketAddr 输入原样保留(单测)。
fn pair_rate_key(source: &str) -> String {
    source
        .parse::<std::net::SocketAddr>()
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| source.to_owned())
}

fn err_envelope(env: Option<&Value>, code: ErrCode, message: &str) -> String {
    let id = env
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let op = env
        .and_then(|v| v.get("op"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let reply = if id.is_empty() {
        json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"error.err","payload":{"code":code,"message":message}})
    } else {
        json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":format!("{op}.err"),"replyTo":id,"payload":{"code":code,"message":message}})
    };
    reply.to_string()
}

impl Gateway {
    pub fn new(signer: Signer) -> Arc<Self> {
        Self::with_options(
            signer,
            Arc::new(MemoryRegistry::new()),
            "local".into(),
            String::new(),
            None,
        )
    }

    /// 注入注册表后端(测试与 M3 k8s 后端装配点)。
    pub fn with_registry(signer: Signer, nodes: Arc<dyn NodeRegistry>) -> Arc<Self> {
        Self::with_options(signer, nodes, "local".into(), String::new(), None)
    }

    /// 完整装配:注册表 + 实例标识 + mesh 地址 + 跨副本转发器。
    pub fn with_options(
        signer: Signer,
        nodes: Arc<dyn NodeRegistry>,
        instance_id: String,
        advertise: String,
        mesh: Option<Arc<dyn MeshRouter>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            signer,
            nodes,
            local: RwLock::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            revoked: RwLock::new(HashSet::new()),
            pairs: Mutex::new(HashMap::new()),
            devices: Mutex::new(HashMap::new()),
            pair_failures: Mutex::new(HashMap::new()),
            state_path: std::sync::Mutex::new(None),
            bootstrap_jti: std::sync::Mutex::new(None),
            instance_id,
            advertise,
            mesh,
            mesh_open: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
            started_at: mixum_protocol::envelope::now_ms(),
        })
    }

    /// 加载持久化状态(撤销表/设备表/bootstrap jti)。bin 在启动时调用。
    pub async fn load_state(&self, path: std::path::PathBuf) {
        *self.state_path.lock().expect("state path lock") = Some(path.clone());
        let Ok(text) = std::fs::read_to_string(&path) else {
            return;
        };
        #[derive(Default, Serialize, Deserialize)]
        struct Persisted {
            revoked: HashSet<String>,
            devices: HashMap<String, DeviceEntry>,
            bootstrap_jti: Option<String>,
        }
        let Ok(state) = serde_json::from_str::<Persisted>(&text) else {
            eprintln!("gateway 状态文件解析失败,忽略: {}", path.display());
            return;
        };
        *self.revoked.write().await = state.revoked;
        *self.devices.lock().await = state.devices;
        *self.bootstrap_jti.lock().expect("bootstrap lock") = state.bootstrap_jti;
    }

    async fn persist(&self) {
        let path = self.state_path.lock().expect("state path lock").clone();
        let Some(path) = path else {
            return;
        };
        #[derive(Serialize)]
        struct Persisted<'a> {
            revoked: &'a HashSet<String>,
            devices: &'a HashMap<String, DeviceEntry>,
            bootstrap_jti: Option<String>,
        }
        let revoked = self.revoked.read().await.clone();
        let devices = self.devices.lock().await.clone();
        let bootstrap_jti = self.bootstrap_jti.lock().expect("bootstrap lock").clone();
        let state = Persisted {
            revoked: &revoked,
            devices: &devices,
            bootstrap_jti,
        };
        let Ok(text) = serde_json::to_string_pretty(&state) else {
            return;
        };
        let tmp = path.with_extension("json.tmp");
        if let Err(e) = std::fs::write(&tmp, text) {
            eprintln!("gateway 状态写入失败: {e}");
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        if let Err(e) = std::fs::rename(&tmp, &path) {
            eprintln!("gateway 状态落盘失败: {e}");
        }
    }

    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// 启动 mesh 入站消费任务:把跨副本事件接到本 gateway 的路由核心。
    pub fn start_mesh(self: &Arc<Self>, hub: Arc<mesh::MeshHub>) {
        let gateway = self.clone();
        tokio::spawn(async move {
            while let Some(event) = hub.next_event().await {
                match event {
                    mesh::MeshEvent::Text { session, text } => {
                        gateway.on_mesh_text(&session, &text).await;
                    }
                    mesh::MeshEvent::Binary { session, data } => {
                        gateway.on_mesh_binary(&session, &data).await;
                    }
                    mesh::MeshEvent::Close { session } => {
                        gateway.on_mesh_close(&session).await;
                    }
                }
            }
        });
    }

    /// 启动时轮换 bootstrap:旧 jti 进撤销表,签发新的短期管理令牌。
    /// 启动时轮换 bootstrap。
    pub async fn bootstrap_token(&self) -> Result<String, String> {
        let now = mixum_protocol::envelope::now_ms();
        let previous = self.bootstrap_jti.lock().expect("bootstrap lock").take();
        if let Some(old) = previous {
            self.revoked.write().await.insert(old);
        }
        let jti = mixum_protocol::envelope::new_id();
        *self.bootstrap_jti.lock().expect("bootstrap lock") = Some(jti.clone());
        let token = self
            .signer
            .mint(Claims {
                sub: "gateway-admin".into(),
                caps: vec![Cap::Admin],
                iat: now,
                exp: now + 30 * 24 * 3600,
                jti,
                task_id: String::new(),
                windows: vec![],
            })
            .map_err(|e| e.to_string())?;
        self.persist().await;
        Ok(token)
    }

    pub async fn verify(&self, token: &str) -> Result<Claims, String> {
        let verifier = mixum_protocol::token::Verifier::new(self.signer.verifying_key());
        let claims = verifier.verify(token).map_err(|e| e.to_string())?;
        if self.revoked.read().await.contains(&claims.jti) && !claims.jti.is_empty() {
            return Err("令牌已吊销".into());
        }
        Ok(claims)
    }

    pub async fn node_connected(
        &self,
        name: &str,
        version: &str,
        out: OutTx,
    ) -> Result<(), ConnectError> {
        if !valid_node_name(name) {
            return Err(ConnectError::InvalidName(format!(
                "节点名非法(1..=64 字符、无冒号): {name}"
            )));
        }
        let record = NodeRecord {
            name: name.to_owned(),
            version: version.to_owned(),
            since: mixum_protocol::envelope::now_ms(),
            owner: self.instance_id.clone(),
            owner_addr: self.advertise.clone(),
        };
        self.nodes.register(record).await?;
        self.local.write().await.insert(name.to_owned(), out);
        Ok(())
    }

    pub async fn node_disconnected(&self, name: &str) {
        self.nodes.remove(name, &self.instance_id).await;
        self.local.write().await.remove(name);
        // 该节点上的客户端会话标记清除:节点回归后下一封重开 session。
        let mut clients = self.clients.lock().await;
        for client in clients.values_mut() {
            client.nodes.remove(name);
        }
    }

    pub async fn node_offline(&self) -> Vec<String> {
        self.nodes.online().await
    }

    /// 节点心跳续约(relay 每 20s 一次)。
    pub async fn renew_node(&self, name: &str) -> Result<(), RegistryError> {
        self.nodes
            .renew(name, &self.instance_id, mixum_protocol::envelope::now_ms())
            .await
    }

    pub async fn local_outbox(&self, name: &str) -> Option<OutTx> {
        self.local.read().await.get(name).cloned()
    }

    pub async fn client_connected(&self, sub: &str, out: OutTx) -> String {
        self.client_connected_with_caps(sub, Vec::new(), out).await
    }

    /// 带能力集的客户端会话(bin 从 hello claims 传入;能力决定能否签发令牌)。
    pub async fn client_connected_with_caps(
        &self,
        sub: &str,
        caps: Vec<Cap>,
        out: OutTx,
    ) -> String {
        let id = format!(
            "gs-{}-{}",
            self.started_at,
            self.seq.fetch_add(1, Ordering::Relaxed)
        );
        self.clients.lock().await.insert(
            id.clone(),
            ClientConn {
                out,
                sub: sub.to_owned(),
                caps,
                nodes: HashSet::new(),
            },
        );
        id
    }

    async fn caller(&self, session: &str) -> (String, Vec<Cap>) {
        self.clients
            .lock()
            .await
            .get(session)
            .map(|c| (c.sub.clone(), c.caps.clone()))
            .unwrap_or_default()
    }

    fn is_admin(sub: &str, caps: &[Cap]) -> bool {
        sub == "gateway-admin" || caps.contains(&Cap::Admin)
    }

    pub async fn client_disconnected(&self, session: &str) {
        // 通知对应节点关会话(relay 据此关闭回环连接)。
        let notice = json!({
            "v":1,"id":mixum_protocol::envelope::new_id(),
            "op":"gateway.session.close","payload":{"gatewaySession":session}
        })
        .to_string();
        let outboxes: Vec<OutTx> = self.local.read().await.values().cloned().collect();
        for out in outboxes {
            let _ = out.send(Outbound::Text(notice.clone())).await;
        }
        // 跨副本会话:通知 owner 侧关闭对应回环。
        if let Some(mesh) = &self.mesh {
            mesh.close_client(session).await;
        }
        self.clients.lock().await.remove(session);
    }

    /// 客户端文本信封入口:gateway 自有 op 就地应答,其余按 node 路由。
    pub async fn on_client_text(&self, session: &str, text: &str) -> Vec<Outbound> {
        let env: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => {
                return vec![Outbound::Text(err_envelope(
                    None,
                    ErrCode::BadRequest,
                    "消息不是合法 JSON",
                ))]
            }
        };
        let op = env.get("op").and_then(Value::as_str).unwrap_or_default();
        match op {
            "gateway.token.issue" => {
                vec![self.issue_token(session, &env).await]
            }
            "gateway.pair.create" => {
                vec![self.pair_create(session, &env).await]
            }
            "gateway.device.list" => {
                vec![self.device_list(session, &env).await]
            }
            "gateway.device.revoke" => {
                vec![self.device_revoke(session, &env).await]
            }
            "gateway.token.revoke" => {
                vec![self.token_revoke(session, &env).await]
            }
            "gateway.node.list" => {
                let nodes = self.node_offline().await;
                vec![Outbound::Text(
                    json!({"v":1,"id":env.get("id").cloned().unwrap_or_default(),"op":"gateway.node.list.ok","replyTo":env.get("id").cloned().unwrap_or_default(),"payload":{"nodes":nodes}})
                        .to_string(),
                )]
            }
            _ => self.route_client(session, env).await,
        }
    }

    async fn route_client(&self, session: &str, env: Value) -> Vec<Outbound> {
        let op = env
            .get("op")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let Ok(field) = envelope_node(&env) else {
            return vec![Outbound::Text(err_envelope(
                Some(&env),
                ErrCode::BadRequest,
                "node 非法(1..=64 字符、无冒号)",
            ))];
        };
        let node_name = match field {
            Some(name) => name.to_owned(),
            None => {
                let online = self.node_offline().await;
                if online.len() == 1 {
                    online[0].clone()
                } else {
                    return vec![Outbound::Text(err_envelope(
                        Some(&env),
                        ErrCode::BadRequest,
                        "请指定 node(多节点在线)",
                    ))];
                }
            }
        };
        let out = Vec::new();
        let Some(node) = self.nodes.get(&node_name).await else {
            return vec![Outbound::Text(err_envelope(
                Some(&env),
                ErrCode::NotFound,
                &format!("节点不在线: {node_name}"),
            ))];
        };
        // 跨副本(M3b):把信封转给 owner 副本,应答由 mesh 通道回送。
        if node.owner != self.instance_id {
            let Some(mesh) = &self.mesh else {
                return vec![Outbound::Text(err_envelope(
                    Some(&env),
                    ErrCode::Internal,
                    &format!("节点由其他 gateway 副本持有: {node_name}"),
                ))];
            };
            let mut forwarded = env.clone();
            forwarded["node"] = json!(node_name);
            return match mesh
                .forward_text(session, &node, &forwarded.to_string())
                .await
            {
                Ok(_) => {
                    if let Some(client) = self.clients.lock().await.get_mut(session) {
                        client.nodes.insert(node_name.clone());
                    }
                    out
                }
                Err(e) => vec![Outbound::Text(err_envelope(
                    Some(&env),
                    ErrCode::Internal,
                    &e,
                ))],
            };
        }
        let Some(node_out) = self.local_outbox(&node_name).await else {
            return vec![Outbound::Text(err_envelope(
                Some(&env),
                ErrCode::Internal,
                &format!("节点连接已失效: {node_name}"),
            ))];
        };
        // 该 (会话, 节点) 首次路由:先开 relay 侧回环会话(重复 open
        // 由 relay 幂等忽略),再转信封。
        let need_open = self
            .clients
            .lock()
            .await
            .get(session)
            .map(|c| !c.nodes.contains(&node_name))
            .unwrap_or(false);
        if need_open {
            let open = json!({
                "v":1,"id":mixum_protocol::envelope::new_id(),
                "op":"gateway.session.open",
                "payload":{"gatewaySession":session,"node":node_name}
            });
            if node_out
                .send(Outbound::Text(open.to_string()))
                .await
                .is_err()
            {
                return vec![Outbound::Text(err_envelope(
                    Some(&env),
                    ErrCode::Internal,
                    &format!("节点连接异常: {node_name}"),
                ))];
            }
        }
        // 会话标记:gateway↔节点私有通道约定(见模块注释)。
        let mut tagged = env.clone();
        tagged["gatewaySession"] = json!(session);
        if node_out
            .send(Outbound::Text(tagged.to_string()))
            .await
            .is_err()
        {
            return vec![Outbound::Text(err_envelope(
                Some(&env),
                ErrCode::Internal,
                &format!("节点连接异常: {node_name}"),
            ))];
        }
        if let Some(client) = self.clients.lock().await.get_mut(session) {
            client.nodes.insert(node_name.clone());
        }
        let _ = op;
        out
    }

    /// 节点文本入口:带 gatewaySession → 剥离标记后直投客户端;heartbeat
    /// 就地续约;`ms-` 前缀是跨副本会话,交 mesh 回送 owner 的请求方。
    /// 投递直接走客户端连接的出站口——bin 层不得把返回值发回节点。
    pub async fn on_node_text(&self, node: &str, text: &str) {
        let Ok(env) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let op = env.get("op").and_then(Value::as_str).unwrap_or_default();
        if op == "gateway.heartbeat" {
            let _ = self.renew_node(node).await;
            return;
        }
        let Some(session) = env.get("gatewaySession").and_then(Value::as_str) else {
            return;
        };
        let mut clean = env.clone();
        if let Some(obj) = clean.as_object_mut() {
            obj.remove("gatewaySession");
        }
        if is_mesh_session(session) {
            if let Some(mesh) = &self.mesh {
                mesh.reply_text(session, &clean.to_string()).await;
            }
            return;
        }
        let clients = self.clients.lock().await;
        if let Some(client) = clients.get(session) {
            let _ = client.out.send(Outbound::Text(clean.to_string())).await;
        }
    }

    /// 节点二进制入口:剥离会话前缀后直投对应客户端;跨副本会话走 mesh。
    pub async fn on_node_binary(&self, bytes: &[u8]) {
        if bytes.len() < 4 {
            return;
        }
        let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if len == 0 || bytes.len() < 4 + len {
            return;
        }
        let session = String::from_utf8_lossy(&bytes[4..4 + len]).to_string();
        let frame = bytes[4 + len..].to_vec();
        if is_mesh_session(&session) {
            if let Some(mesh) = &self.mesh {
                mesh.reply_binary(&session, &frame).await;
            }
            return;
        }
        let clients = self.clients.lock().await;
        if let Some(client) = clients.get(&session) {
            let _ = client.out.send(Outbound::Binary(frame)).await;
        }
    }

    /// mesh 文本入口。
    /// - 本副本是请求发起方(会话在 `client_session` 表里)→ 投给本地客户端;
    /// - 本副本是节点 owner → 校验归属后送本地节点,失败用 mesh 回错误信封。
    pub async fn on_mesh_text(&self, session: &str, text: &str) {
        if let Some(mesh) = &self.mesh {
            if let Some(client_session) = mesh.client_session(session).await {
                let mut clean: Value = match serde_json::from_str(text) {
                    Ok(value) => value,
                    Err(_) => return,
                };
                if let Some(obj) = clean.as_object_mut() {
                    obj.remove("gatewaySession");
                }
                let clients = self.clients.lock().await;
                if let Some(client) = clients.get(&client_session) {
                    let _ = client.out.send(Outbound::Text(clean.to_string())).await;
                }
                return;
            }
        }
        self.route_mesh_to_local(session, text).await;
    }

    pub async fn on_mesh_binary(&self, session: &str, frame: &[u8]) {
        let Some(mesh) = &self.mesh else {
            return;
        };
        if let Some(client_session) = mesh.client_session(session).await {
            let clients = self.clients.lock().await;
            if let Some(client) = clients.get(&client_session) {
                let _ = client.out.send(Outbound::Binary(frame.to_vec())).await;
            }
        }
    }

    pub async fn on_mesh_close(&self, session: &str) {
        let node = self.mesh_open.lock().await.remove(session);
        if let (Some(mesh), Some(node)) = (&self.mesh, node) {
            mesh.close_inbound(session).await;
            if let Some(out) = self.local_outbox(&node).await {
                let close = json!({
                    "v":1,"id":mixum_protocol::envelope::new_id(),
                    "op":"gateway.session.close","payload":{"gatewaySession":session}
                })
                .to_string();
                let _ = out.send(Outbound::Text(close)).await;
            }
        }
    }

    async fn route_mesh_to_local(&self, session: &str, text: &str) {
        let Ok(env) = serde_json::from_str::<Value>(text) else {
            return;
        };
        // mesh 转发必须带 node:缺省视为请求非法,不做单节点自动路由。
        let Ok(Some(node_name)) = envelope_node(&env) else {
            self.mesh_error(
                session,
                &env,
                ErrCode::BadRequest,
                "node 必填且为 1..=64 字符、不含冒号",
            )
            .await;
            return;
        };
        let Some(record) = self.nodes.get(node_name).await else {
            self.mesh_error(
                session,
                &env,
                ErrCode::NotFound,
                &format!("节点不在线: {node_name}"),
            )
            .await;
            return;
        };
        if record.owner != self.instance_id {
            self.mesh_error(
                session,
                &env,
                ErrCode::Internal,
                &format!("节点由其他 gateway 副本持有: {node_name}"),
            )
            .await;
            return;
        }
        let Some(out) = self.local_outbox(node_name).await else {
            self.mesh_error(
                session,
                &env,
                ErrCode::Internal,
                &format!("节点连接已失效: {node_name}"),
            )
            .await;
            return;
        };
        let first = self
            .mesh_open
            .lock()
            .await
            .insert(session.to_owned(), node_name.to_owned())
            .is_none();
        if first {
            let open = json!({
                "v":1,"id":mixum_protocol::envelope::new_id(),
                "op":"gateway.session.open",
                "payload":{"gatewaySession":session,"node":node_name}
            });
            if out.send(Outbound::Text(open.to_string())).await.is_err() {
                self.mesh_open.lock().await.remove(session);
                self.mesh_error(
                    session,
                    &env,
                    ErrCode::Internal,
                    &format!("节点连接异常: {node_name}"),
                )
                .await;
                return;
            }
        }
        let mut tagged = env.clone();
        tagged["gatewaySession"] = json!(session);
        if out.send(Outbound::Text(tagged.to_string())).await.is_err() {
            self.mesh_open.lock().await.remove(session);
            self.mesh_error(
                session,
                &env,
                ErrCode::Internal,
                &format!("节点连接异常: {node_name}"),
            )
            .await;
        }
    }

    async fn mesh_error(&self, session: &str, env: &Value, code: ErrCode, message: &str) {
        if let Some(mesh) = &self.mesh {
            let error = err_envelope(Some(env), code, message);
            mesh.reply_text(session, &error).await;
        }
    }

    async fn record_pair_failure(&self, source: &str, now: i64) {
        self.pair_failures
            .lock()
            .await
            .entry(source.to_owned())
            .or_default()
            .push(now);
    }

    /// 设备配对第一步:已信任会话生成 6 位配对码(5 分钟有效,单次)。
    async fn pair_create(&self, session: &str, env: &Value) -> Outbound {
        let id = env.get("id").cloned().unwrap_or_default();
        let fail = |code, msg: &str| Outbound::Text(err_envelope(Some(env), code, msg));
        let user_sub = self
            .clients
            .lock()
            .await
            .get(session)
            .map(|c| c.sub.clone())
            .unwrap_or_default();
        if !user_sub.starts_with("user:") && user_sub != "gateway-admin" {
            return fail(ErrCode::Forbidden, "仅用户会话可生成配对码");
        }
        // 6 位数字码:操作系统 CSPRNG,不复用 seq/时间/指针作为熵。
        use rand::Rng;
        let code = {
            let mut pairs = self.pairs.lock().await;
            loop {
                let candidate = format!("{:06}", rand::rngs::OsRng.gen_range(0..1_000_000u32));
                if !pairs.contains_key(&candidate) {
                    pairs.insert(
                        candidate.clone(),
                        PairCode {
                            user_sub: user_sub.clone(),
                            expires_at: mixum_protocol::envelope::now_ms() + 300_000,
                        },
                    );
                    break candidate;
                }
            }
        };
        Outbound::Text(
            json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"gateway.pair.create.ok","replyTo":id,"payload":{"code":code,"expiresIn":300}})
                .to_string(),
        )
    }

    /// 设备配对第二步:配对码换设备令牌(未认证连接的首条消息)。
    pub async fn pair_claim(
        &self,
        source: &str,
        code: &str,
        device_name: &str,
    ) -> Result<(String, String, String), String> {
        let now = mixum_protocol::envelope::now_ms();
        // 来源统一成 IP:bin 传的是 IP:port,不能把 ephemeral port 当限速桶。
        let source = pair_rate_key(source);
        // 按来源限速:窗口内最多 10 次失败。
        {
            let mut failures = self.pair_failures.lock().await;
            if failures.len() > 1024 {
                failures.retain(|_, recent| {
                    recent.retain(|ts| now - *ts < 60_000);
                    !recent.is_empty()
                });
            }
            let recent = failures.entry(source.clone()).or_default();
            recent.retain(|ts| now - *ts < 60_000);
            if recent.len() >= 10 {
                return Err("配对尝试过于频繁,请稍后再试".into());
            }
        }
        if device_name.is_empty() || device_name.len() > 64 {
            self.record_pair_failure(&source, now).await;
            return Err("deviceName 非法".into());
        }
        // 只有正确的码才消费;错误的码不触碰 pairs。
        let pair = self.pairs.lock().await.remove(code);
        let Some(pair) = pair.filter(|p| p.expires_at > now) else {
            self.record_pair_failure(&source, now).await;
            return Err("配对码无效或已过期".into());
        };
        let user_sub = pair.user_sub;
        let device_id = format!("dev-{}", mixum_protocol::envelope::new_id());
        let sub = format!("device:{device_id}");
        let claims = Claims {
            sub: sub.clone(),
            caps: vec![
                Cap::TaskRun,
                Cap::TaskObserve,
                Cap::DisplayView,
                Cap::DisplayInput,
            ],
            iat: now,
            exp: now + 30 * 24 * 3600,
            jti: mixum_protocol::envelope::new_id(),
            task_id: String::new(),
            windows: vec![],
        };
        let jti = claims.jti.clone();
        let token = self.signer.mint(claims).map_err(|e| e.to_string())?;
        self.devices.lock().await.insert(
            jti.clone(),
            DeviceEntry {
                name: device_name.to_owned(),
                sub,
                owner: user_sub.clone(),
                iat: now,
            },
        );
        eprintln!("gateway 配对成功: device={device_id} owner={user_sub}");
        self.persist().await;
        Ok((token, device_id, user_sub))
    }

    /// 设备列表(吊销过的已移除,不出现)。
    async fn device_list(&self, session: &str, env: &Value) -> Outbound {
        let id = env.get("id").cloned().unwrap_or_default();
        let user_sub = self
            .clients
            .lock()
            .await
            .get(session)
            .map(|c| c.sub.clone())
            .unwrap_or_default();
        if !user_sub.starts_with("user:") && user_sub != "gateway-admin" {
            return Outbound::Text(err_envelope(
                Some(env),
                ErrCode::Forbidden,
                "仅用户会话可查设备",
            ));
        }
        let (caller_sub, caller_caps) = self.caller(session).await;
        let admin = Self::is_admin(&caller_sub, &caller_caps);
        let devices = self.devices.lock().await;
        let list: Vec<Value> = devices
            .iter()
            .filter(|(_, d)| admin || d.owner == caller_sub)
            .map(|(jti, d)| json!({"jti": jti, "name": d.name, "sub": d.sub, "owner": d.owner, "iat": d.iat}))
            .collect();
        Outbound::Text(
            json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"gateway.device.list.ok","replyTo":id,"payload":{"devices":list}})
                .to_string(),
        )
    }

    /// 吊销设备:jti 进撤销表并移除条目;已签令牌即刻失效。
    async fn device_revoke(&self, session: &str, env: &Value) -> Outbound {
        let id = env.get("id").cloned().unwrap_or_default();
        let fail = |code, msg: &str| Outbound::Text(err_envelope(Some(env), code, msg));
        let user_sub = self
            .clients
            .lock()
            .await
            .get(session)
            .map(|c| c.sub.clone())
            .unwrap_or_default();
        if !user_sub.starts_with("user:") && user_sub != "gateway-admin" {
            return Outbound::Text(err_envelope(
                Some(env),
                ErrCode::Forbidden,
                "仅用户会话可吊销设备",
            ));
        }
        let jti = env
            .pointer("/payload/jti")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if jti.is_empty() {
            return fail(ErrCode::BadRequest, "jti 必填");
        }
        let (caller_sub, caller_caps) = self.caller(session).await;
        let admin = Self::is_admin(&caller_sub, &caller_caps);
        let removed = {
            let mut devices = self.devices.lock().await;
            match devices.get(&jti) {
                Some(d) if admin || d.owner == caller_sub => devices.remove(&jti).is_some(),
                _ => return fail(ErrCode::Forbidden, "无权吊销该设备"),
            }
        };
        self.revoked.write().await.insert(jti.clone());
        eprintln!("gateway 设备吊销: jti={jti} by={caller_sub}");
        self.persist().await;
        Outbound::Text(
            json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"gateway.device.revoke.ok","replyTo":id,"payload":{"revoked":removed}})
                .to_string(),
        )
    }

    /// 撤销任意已签发令牌(admin):jti 进撤销表,验证时即时失效。
    async fn token_revoke(&self, session: &str, env: &Value) -> Outbound {
        let id = env.get("id").cloned().unwrap_or_default();
        let fail = |code, msg: &str| Outbound::Text(err_envelope(Some(env), code, msg));
        let (sub, caps) = self.caller(session).await;
        if !Self::is_admin(&sub, &caps) {
            return fail(ErrCode::Forbidden, "仅管理员可撤销令牌");
        }
        let jti = env
            .pointer("/payload/jti")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if jti.is_empty() {
            return fail(ErrCode::BadRequest, "jti 必填");
        }
        self.revoked.write().await.insert(jti.clone());
        eprintln!("gateway 令牌吊销: jti={jti} by={sub}");
        self.persist().await;
        Outbound::Text(
            json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"gateway.token.revoke.ok","replyTo":id,"payload":{"revoked":true}})
                .to_string(),
        )
    }

    async fn issue_token(&self, session: &str, env: &Value) -> Outbound {
        let id = env.get("id").cloned().unwrap_or_default();
        let fail = |code, msg: &str| Outbound::Text(err_envelope(Some(env), code, msg));
        // 权限:只有 admin 能力(gateway-admin 或 Cap::Admin)可签发。
        let (sub, caps) = self.caller(session).await;
        if !Self::is_admin(&sub, &caps) {
            return fail(ErrCode::Forbidden, "仅管理员可签发令牌");
        }
        let kind = env
            .pointer("/payload/kind")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let name = env
            .pointer("/payload/name")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if !valid_node_name(&name) {
            return fail(ErrCode::BadRequest, "name 非法(非空、无冒号、≤64 字符)");
        }
        let now = mixum_protocol::envelope::now_ms();
        let claims = match kind {
            "node" => Claims {
                sub: format!("node:{name}"),
                caps: vec![], // 仅用于 gateway 注册(通道只认 node:* 前缀);节点本机会话用本地令牌
                iat: now,
                exp: now + 365 * 24 * 3600,
                jti: mixum_protocol::envelope::new_id(),
                task_id: String::new(),
                windows: vec![],
            },
            "user" => Claims {
                sub: format!("user:{name}"),
                caps: vec![
                    Cap::TaskRun,
                    Cap::TaskObserve,
                    Cap::SnapManage,
                    Cap::DisplayView,
                    Cap::DisplayInput,
                ],
                iat: now,
                exp: now + 30 * 24 * 3600,
                jti: mixum_protocol::envelope::new_id(),
                task_id: String::new(),
                windows: vec![],
            },
            _ => return fail(ErrCode::BadRequest, "kind 仅 node|user"),
        };
        let token = match self.signer.mint(claims) {
            Ok(t) => t,
            Err(e) => return fail(ErrCode::Internal, &format!("签发失败: {e}")),
        };
        Outbound::Text(
            json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"gateway.token.issue.ok","replyTo":id,"payload":{"token":token}})
                .to_string(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn non_admin_cannot_issue_node_token() {
        let gw = Gateway::new(Signer::generate());
        let (ctx, _rx) = mpsc_pair();
        let session = gw.client_connected("user:alice", ctx).await;
        let req = json!({"v":1,"id":"t1","op":"gateway.token.issue","payload":{"kind":"node","name":"victim"}});
        let out = gw.on_client_text(&session, &req.to_string()).await;
        assert!(
            matches!(&out[0], Outbound::Text(t) if t.contains("forbidden")),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn pair_rate_limit_ignores_source_port() {
        let gw = Gateway::new(Signer::generate());
        for port in 4000..4010 {
            assert!(gw
                .pair_claim(&format!("203.0.113.9:{port}"), "000000", "dev")
                .await
                .is_err());
        }
        let err = gw
            .pair_claim("203.0.113.9:4999", "000000", "dev")
            .await
            .unwrap_err();
        assert!(err.contains("频繁"), "{err}");
    }

    #[tokio::test]
    async fn invalid_device_name_does_not_consume_pair_code() {
        let gw = Gateway::new(Signer::generate());
        let (ctx, _rx) = mpsc_pair();
        let alice = gw.client_connected("user:alice", ctx).await;
        let out = gw
            .on_client_text(
                &alice,
                &json!({"v":1,"id":"p1","op":"gateway.pair.create","payload":{}}).to_string(),
            )
            .await;
        let text = match &out[0] {
            Outbound::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        let code = value["payload"]["code"].as_str().unwrap().to_owned();
        assert!(gw.pair_claim("203.0.113.2", &code, "").await.is_err());
        assert!(gw.pair_claim("203.0.113.2", &code, "phone").await.is_ok());
    }

    #[tokio::test]
    async fn pair_claim_is_rate_limited_per_source() {
        let gw = Gateway::new(Signer::generate());
        for _ in 0..10 {
            assert!(gw.pair_claim("203.0.113.9", "000000", "dev").await.is_err());
        }
        let err = gw
            .pair_claim("203.0.113.9", "000000", "dev")
            .await
            .unwrap_err();
        assert!(err.contains("频繁"), "{err}");
    }

    #[tokio::test]
    async fn device_is_owned_by_pairing_user() {
        let gw = Gateway::new(Signer::generate());
        let (ctx, _rx) = mpsc_pair();
        let alice = gw.client_connected("user:alice", ctx).await;
        let create = json!({"v":1,"id":"p1","op":"gateway.pair.create","payload":{}});
        let out = gw.on_client_text(&alice, &create.to_string()).await;
        let text = match &out[0] {
            Outbound::Text(t) => t.clone(),
            other => panic!("{other:?}"),
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        let code = value["payload"]["code"].as_str().unwrap().to_owned();
        let (_, _, owner) = gw.pair_claim("203.0.113.1", &code, "phone").await.unwrap();
        assert_eq!(owner, "user:alice");

        let (ctx2, _rx2) = mpsc_pair();
        let bob = gw.client_connected("user:bob", ctx2).await;
        let list = gw
            .on_client_text(
                &bob,
                &json!({"v":1,"id":"p2","op":"gateway.device.list"}).to_string(),
            )
            .await;
        let text = match &list[0] {
            Outbound::Text(t) => t.clone(),
            o => panic!("{o:?}"),
        };
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            value["payload"]["devices"].as_array().unwrap().len(),
            0,
            "不应看到别人的设备"
        );
    }

    fn mpsc_pair() -> (OutTx, mpsc::Receiver<Outbound>) {
        mpsc::channel(16)
    }

    fn error_code(out: &Outbound) -> String {
        match out {
            Outbound::Text(text) => {
                let value: Value = serde_json::from_str(text).expect("错误信封是 JSON");
                value
                    .pointer("/payload/code")
                    .and_then(Value::as_str)
                    .expect("错误信封带 payload.code")
                    .to_owned()
            }
            other => panic!("期望文本,实际 {other:?}"),
        }
    }

    struct RecordingMesh {
        replies: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingMesh {
        fn new() -> Self {
            Self {
                replies: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn codes(&self) -> Vec<String> {
            self.replies
                .lock()
                .unwrap()
                .iter()
                .map(|text| {
                    let value: Value = serde_json::from_str(text).unwrap();
                    value
                        .pointer("/payload/code")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect()
        }
    }

    #[async_trait::async_trait]
    impl MeshRouter for RecordingMesh {
        async fn forward_text(
            &self,
            _client_session: &str,
            _record: &NodeRecord,
            _text: &str,
        ) -> Result<String, String> {
            Err("mesh 不可达".into())
        }
        async fn client_session(&self, _mesh_session: &str) -> Option<String> {
            None
        }
        async fn reply_text(&self, _mesh_session: &str, text: &str) {
            self.replies.lock().unwrap().push(text.to_owned());
        }
        async fn reply_binary(&self, _mesh_session: &str, _frame: &[u8]) {}
        async fn close_client(&self, _client_session: &str) {}
        async fn close_inbound(&self, _mesh_session: &str) {}
    }

    #[tokio::test]
    async fn client_request_routes_to_node_and_reply_returns() {
        let gw = Gateway::new(Signer::generate());
        let (ntx, mut nrx) = mpsc_pair();
        gw.node_connected("a", "0.1.0", ntx).await.unwrap();
        assert!(gw
            .node_connected("a", "0.1.0", mpsc_pair().0)
            .await
            .is_err());

        let (ctx, mut crx) = mpsc_pair();
        let session = gw.client_connected("user:u1", ctx).await;

        // 客户端请求(带 node)→ 到节点,且带 gatewaySession
        let req = json!({"v":1,"id":"r1","op":"task.list","node":"a","payload":{}});
        let out = gw.on_client_text(&session, &req.to_string()).await;
        assert!(out.is_empty(), "路由本身不直接回客户端");
        // 首条是 gateway.session.open,次条才是带标记的信封
        match nrx.recv().await.unwrap() {
            Outbound::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                assert_eq!(v["op"], "gateway.session.open");
                assert_eq!(v["payload"]["gatewaySession"], session);
            }
            other => panic!("期望 open,实际 {other:?}"),
        }
        match nrx.recv().await.unwrap() {
            Outbound::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                assert_eq!(v["gatewaySession"], session);
                assert_eq!(v["op"], "task.list");
            }
            other => panic!("期望文本,实际 {other:?}"),
        }

        // 节点应答(带 gatewaySession)→ 直投客户端通道
        let reply = json!({"v":1,"id":"r1","op":"task.list.ok","replyTo":"r1","gatewaySession":session,"payload":{"tasks":[]}});
        gw.on_node_text("a", &reply.to_string()).await;
        match crx.recv().await.unwrap() {
            Outbound::Text(t) => assert!(t.contains("task.list.ok")),
            other => panic!("期望文本,实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn single_online_node_is_default_target() {
        let gw = Gateway::new(Signer::generate());
        let (ntx, mut nrx) = mpsc_pair();
        gw.node_connected("only", "0.1.0", ntx).await.unwrap();
        let (ctx, _crx) = mpsc_pair();
        let session = gw.client_connected("user:u1", ctx).await;
        let req = json!({"v":1,"id":"r2","op":"sys.ping","payload":{}});
        gw.on_client_text(&session, &req.to_string()).await;
        match nrx.recv().await.unwrap() {
            Outbound::Text(t) => {
                assert!(t.contains("\"node\":\"only\"") || t.contains("gatewaySession"))
            }
            other => panic!("期望文本,实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn multi_node_without_node_field_is_rejected() {
        let gw = Gateway::new(Signer::generate());
        gw.node_connected("a", "0.1.0", mpsc_pair().0)
            .await
            .unwrap();
        gw.node_connected("b", "0.1.0", mpsc_pair().0)
            .await
            .unwrap();
        let (ctx, _crx) = mpsc_pair();
        let session = gw.client_connected("user:u1", ctx).await;
        let req = json!({"v":1,"id":"r3","op":"sys.ping","payload":{}});
        let out = gw.on_client_text(&session, &req.to_string()).await;
        assert!(matches!(&out[0], Outbound::Text(t) if t.contains("bad_request")));
    }

    #[tokio::test]
    async fn node_offline_request_gets_not_found() {
        let gw = Gateway::new(Signer::generate());
        let (ctx, _crx) = mpsc_pair();
        let session = gw.client_connected("user:u1", ctx).await;
        let req = json!({"v":1,"id":"r4","op":"task.list","node":"ghost","payload":{}});
        let out = gw.on_client_text(&session, &req.to_string()).await;
        assert!(matches!(&out[0], Outbound::Text(t) if t.contains("not_found")));
    }

    #[tokio::test]
    async fn binary_frames_carry_session_prefix() {
        let gw = Gateway::new(Signer::generate());
        let (ntx, nrx) = mpsc_pair();
        gw.node_connected("a", "0.1.0", ntx).await.unwrap();
        let (ctx, mut crx) = mpsc_pair();
        let session = gw.client_connected("user:u1", ctx).await;

        // 节点发"带前缀"的帧 → 客户端收到裸帧
        let sid = session.as_bytes();
        let mut wire = ((sid.len()) as u32).to_le_bytes().to_vec();
        wire.extend_from_slice(sid);
        wire.extend_from_slice(&[0x01, 2, 3, 4]);
        gw.on_node_binary(&wire).await;
        match crx.recv().await.unwrap() {
            Outbound::Binary(b) => assert_eq!(b, &[0x01, 2, 3, 4]),
            other => panic!("期望二进制,实际 {other:?}"),
        }
        drop(nrx);
    }

    #[tokio::test]
    async fn client_disconnect_closes_node_sessions() {
        let gw = Gateway::new(Signer::generate());
        let (ntx, mut nrx) = mpsc_pair();
        gw.node_connected("a", "0.1.0", ntx).await.unwrap();
        let (ctx, _crx) = mpsc_pair();
        let session = gw.client_connected("user:u1", ctx).await;
        gw.client_disconnected(&session).await;
        match nrx.recv().await.unwrap() {
            Outbound::Text(t) => {
                let v: Value = serde_json::from_str(&t).unwrap();
                assert_eq!(v["op"], "gateway.session.close");
                assert_eq!(v["payload"]["gatewaySession"], session);
            }
            other => panic!("期望 close 通知,实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn node_held_by_another_replica_without_mesh_is_internal() {
        // M3b 预留路径:注册表里有节点,本副本没有 mesh 转发器。
        let registry = Arc::new(MemoryRegistry::new());
        registry
            .register(NodeRecord {
                name: "remote".into(),
                version: "0.1.0".into(),
                since: 1,
                owner: "gw-other".into(),
                owner_addr: "ws://127.0.0.1:1/mesh".into(),
            })
            .await
            .unwrap();
        let gw = Gateway::with_registry(Signer::generate(), registry);
        let (ctx, _crx) = mpsc_pair();
        let session = gw.client_connected("user:u1", ctx).await;
        let req = json!({"v":1,"id":"r5","op":"task.list","node":"remote","payload":{}});
        let out = gw.on_client_text(&session, &req.to_string()).await;
        assert_eq!(error_code(&out[0]), "internal");
    }

    #[tokio::test]
    async fn mesh_forward_failure_is_internal() {
        let registry = Arc::new(MemoryRegistry::new());
        registry
            .register(NodeRecord {
                name: "remote".into(),
                version: "0.1.0".into(),
                since: 1,
                owner: "gw-other".into(),
                owner_addr: "ws://127.0.0.1:1/mesh".into(),
            })
            .await
            .unwrap();
        let gw = Gateway::with_options(
            Signer::generate(),
            registry,
            "local".into(),
            String::new(),
            Some(Arc::new(RecordingMesh::new())),
        );
        let (ctx, _crx) = mpsc_pair();
        let session = gw.client_connected("user:u1", ctx).await;
        let req = json!({"v":1,"id":"r6","op":"task.list","node":"remote","payload":{}});
        let out = gw.on_client_text(&session, &req.to_string()).await;
        assert_eq!(error_code(&out[0]), "internal");
    }

    #[tokio::test]
    async fn stale_local_outbox_is_internal() {
        // 注册表有本副本记录,但出站连接已不在(进程内状态不一致)。
        let registry = Arc::new(MemoryRegistry::new());
        registry
            .register(NodeRecord {
                name: "stale".into(),
                version: "0.1.0".into(),
                since: 1,
                owner: "local".into(),
                owner_addr: String::new(),
            })
            .await
            .unwrap();
        let gw = Gateway::with_registry(Signer::generate(), registry);
        let (ctx, _crx) = mpsc_pair();
        let session = gw.client_connected("user:u1", ctx).await;
        let req = json!({"v":1,"id":"r7","op":"task.list","node":"stale","payload":{}});
        let out = gw.on_client_text(&session, &req.to_string()).await;
        assert_eq!(error_code(&out[0]), "internal");
    }

    #[tokio::test]
    async fn mesh_route_errors_use_rfc_codes() {
        let registry = Arc::new(MemoryRegistry::new());
        for (name, owner) in [("remote", "gw-other"), ("stale", "local")] {
            registry
                .register(NodeRecord {
                    name: name.into(),
                    version: "0.1.0".into(),
                    since: 1,
                    owner: owner.into(),
                    owner_addr: String::new(),
                })
                .await
                .unwrap();
        }
        let mesh = Arc::new(RecordingMesh::new());
        let gw = Gateway::with_options(
            Signer::generate(),
            registry,
            "local".into(),
            String::new(),
            Some(mesh.clone()),
        );
        for (session, req) in [
            (
                "ms-1",
                json!({"v":1,"id":"m1","op":"task.list","node":"remote","payload":{}}),
            ),
            (
                "ms-2",
                json!({"v":1,"id":"m2","op":"task.list","node":"stale","payload":{}}),
            ),
            (
                "ms-3",
                json!({"v":1,"id":"m3","op":"task.list","payload":{}}),
            ),
        ] {
            gw.on_mesh_text(session, &req.to_string()).await;
        }
        assert_eq!(
            mesh.codes(),
            vec![
                "internal".to_owned(),
                "internal".to_owned(),
                "bad_request".to_owned()
            ]
        );
    }

    #[tokio::test]
    async fn token_issue_requires_admin_and_node_token_registers() {
        let gw = Gateway::new(Signer::generate());
        let bootstrap = gw.bootstrap_token().await.unwrap();
        let claims = gw.verify(&bootstrap).await.unwrap();
        assert_eq!(claims.sub, "gateway-admin");

        let (ctx, crx) = mpsc_pair();
        // 非用户会话不能签发
        let anon = gw.client_connected("device:x", ctx.clone()).await;
        let req = json!({"v":1,"id":"t0","op":"gateway.token.issue","payload":{"kind":"node","name":"n1"}});
        let out = gw.on_client_text(&anon, &req.to_string()).await;
        assert!(matches!(&out[0], Outbound::Text(t) if t.contains("forbidden")));

        // admin 会话签发节点令牌
        let admin = gw.client_connected("gateway-admin", ctx.clone()).await;
        let out = gw.on_client_text(&admin, &req.to_string()).await;
        let token = match &out[0] {
            Outbound::Text(t) => {
                let v: Value = serde_json::from_str(t).unwrap();
                v["payload"]["token"].as_str().unwrap().to_owned()
            }
            other => panic!("期望令牌应答,实际 {other:?}"),
        };
        let claims = gw.verify(&token).await.unwrap();
        assert_eq!(claims.sub, "node:n1");

        // 节点令牌注册成功;同名额拒绝
        let (ntx, _nrx) = mpsc_pair();
        gw.node_connected("n1", "0.1.0", ntx).await.unwrap();
        assert!(gw
            .node_connected("n1", "0.1.0", mpsc_pair().0)
            .await
            .is_err());
        drop(crx);
    }

    #[test]
    fn node_name_rules_follow_rfc_0003() {
        assert!(valid_node_name("a-b_1.2"));
        assert!(valid_node_name(&"x".repeat(64)));
        assert!(valid_node_name(&"节".repeat(64)));
        assert!(!valid_node_name(""));
        assert!(!valid_node_name("node:a"));
        assert!(!valid_node_name(&"x".repeat(65)));
        assert!(!valid_node_name(&"节".repeat(65)));
    }

    #[tokio::test]
    async fn invalid_envelope_node_is_bad_request() {
        let gw = Gateway::new(Signer::generate());
        let (ctx, _crx) = mpsc_pair();
        let session = gw.client_connected("user:u1", ctx).await;
        let req = json!({"v":1,"id":"n1","op":"task.list","node":"bad:name","payload":{}});
        let out = gw.on_client_text(&session, &req.to_string()).await;
        assert!(
            matches!(&out[0], Outbound::Text(t) if t.contains("bad_request") && t.contains("node 非法")),
            "{out:?}"
        );
    }

    #[tokio::test]
    async fn present_but_invalid_node_field_is_not_auto_routed() {
        // 单节点在线也不能把空串/null/数字静默降级为自动路由。
        let gw = Gateway::new(Signer::generate());
        let (ntx, _nrx) = mpsc_pair();
        gw.node_connected("only", "0.1.0", ntx).await.unwrap();
        let (ctx, _crx) = mpsc_pair();
        let session = gw.client_connected("user:u1", ctx).await;
        for node in [json!(""), json!(123), Value::Null] {
            let req = json!({"v":1,"id":"n2","op":"sys.ping","node":node,"payload":{}});
            let out = gw.on_client_text(&session, &req.to_string()).await;
            assert_eq!(error_code(&out[0]), "bad_request", "node={node}");
        }
    }

    #[tokio::test]
    async fn node_connected_rejects_invalid_name() {
        let gw = Gateway::new(Signer::generate());
        let err = gw
            .node_connected("bad:name", "0.1.0", mpsc_pair().0)
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectError::InvalidName(_)), "{err:?}");
    }

    #[tokio::test]
    async fn duplicate_node_connection_is_conflict() {
        let gw = Gateway::new(Signer::generate());
        gw.node_connected("a", "0.1.0", mpsc_pair().0)
            .await
            .unwrap();
        let err = gw
            .node_connected("a", "0.1.0", mpsc_pair().0)
            .await
            .unwrap_err();
        assert!(matches!(err, ConnectError::Conflict(_)), "{err:?}");
    }
}
