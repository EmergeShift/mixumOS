//! WSS/WS 服务、握手、能力路由和限速。

use crate::auth::Authorizer;
use crate::display::Broker;
use crate::snapshot::SnapshotEngine;
use crate::task::{Engine, TaskSpec};
use crate::wire::{DisplayListResponse, HelloRequest, HelloResponse, TaskSubmitRequest};
use futures_util::{SinkExt, StreamExt};
use mixum_protocol::envelope::Envelope;
use mixum_protocol::ops::{self, Cap, ErrCode};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::SystemTime;
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tokio_rustls::{rustls, TlsAcceptor};
use tokio_tungstenite::{accept_hdr_async, tungstenite::Message, WebSocketStream};

/// WS 子协议协商:服务端必须应答 mixum.v1(否则守协议的客户端会拒绝连接)。
fn accept_with_subprotocol<S>(
    stream: S,
) -> impl std::future::Future<Output = Result<WebSocketStream<S>, tokio_tungstenite::tungstenite::Error>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    // 回调错误类型为 tungstenite 约定的 Response<Option<String>>;
    // 未请求 mixum.v1 的客户端不报错、仅不回子协议头。
    let cb = |req: &Request,
              resp: Response|
     -> Result<
        Response,
        tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
    > {
        let requested = req
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .map(|p| p.split(',').any(|s| s.trim() == "mixum.v1"))
            .unwrap_or(false);
        let (mut parts, body) = resp.into_parts();
        if requested {
            if let Ok(v) = "mixum.v1".parse() {
                parts.headers.insert("Sec-WebSocket-Protocol", v);
            }
        }
        Ok(Response::from_parts(parts, body))
    };
    accept_hdr_async(stream, cb)
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_listen")]
    pub listen: String,
    #[serde(default = "default_data_dir")]
    pub data_dir: String,
    #[serde(default = "default_work_root")]
    pub work_root: String,
    #[serde(default)]
    pub hostname: String,
    #[serde(default = "default_true")]
    pub encrypt_snapshots: bool,
    #[serde(default = "default_token_ttl")]
    pub token_ttl_sec: u64,
    #[serde(default = "default_max_message")]
    pub max_message_size: usize,
    #[serde(default)]
    pub agents: Vec<AgentConfig>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentConfig {
    pub name: String,
    pub command: String,
}

fn default_listen() -> String {
    "0.0.0.0:7601".into()
}
fn default_data_dir() -> String {
    "/var/lib/mixum".into()
}
fn default_work_root() -> String {
    "/var/lib/mixum/work".into()
}
fn default_true() -> bool {
    true
}
fn default_token_ttl() -> u64 {
    3600
}
fn default_max_message() -> usize {
    32 * 1024 * 1024
}
impl Default for Config {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            data_dir: default_data_dir(),
            work_root: default_work_root(),
            hostname: String::new(),
            encrypt_snapshots: true,
            token_ttl_sec: default_token_ttl(),
            max_message_size: default_max_message(),
            agents: Vec::new(),
        }
    }
}

pub struct Server {
    pub config: Config,
    authorizer: Authorizer,
    tasks: Arc<Engine>,
    broker: Broker,
    snapshots: Arc<dyn SnapshotEngine>,
    /// 真实快照引擎就绪时提供跨节点同步;内存桩下为 None。
    #[cfg(feature = "real-snapshot")]
    pub sync: Option<Arc<crate::sync::SyncBackend>>,
}

impl Server {
    pub fn new(
        config: Config,
        authorizer: Authorizer,
        tasks: Arc<Engine>,
        broker: Broker,
        snapshots: Arc<dyn SnapshotEngine>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            authorizer,
            tasks,
            broker,
            snapshots,
            #[cfg(feature = "real-snapshot")]
            sync: None,
        })
    }

    pub async fn serve_plain(
        self: Arc<Self>,
        addr: impl Into<SocketAddr>,
    ) -> Result<(), ServerError> {
        let listener = TcpListener::bind(addr.into()).await?;
        self.serve_plain_listener(listener).await
    }

    /// 在已绑定的 listener 上提供明文 WS 服务(测试用 0 端口回环)。
    pub async fn serve_plain_listener(
        self: Arc<Self>,
        listener: TcpListener,
    ) -> Result<(), ServerError> {
        loop {
            let (stream, _) = listener.accept().await?;
            let this = self.clone();
            tokio::spawn(async move {
                let _ = this
                    .handle_stream(accept_with_subprotocol(stream).await)
                    .await;
            });
        }
    }

    pub async fn serve_tls(
        self: Arc<Self>,
        addr: impl Into<SocketAddr>,
        config: Arc<rustls::ServerConfig>,
    ) -> Result<(), ServerError> {
        let listener = TcpListener::bind(addr.into()).await?;
        let acceptor = TlsAcceptor::from(config);
        loop {
            let (stream, _) = listener.accept().await?;
            let acceptor = acceptor.clone();
            let this = self.clone();
            tokio::spawn(async move {
                if let Ok(tls) = acceptor.accept(stream).await {
                    let _ = this.handle_stream(accept_with_subprotocol(tls).await).await;
                }
            });
        }
    }

    pub async fn handle_stream<S>(
        self: Arc<Self>,
        stream: Result<WebSocketStream<S>, tokio_tungstenite::tungstenite::Error>,
    ) -> Result<(), ServerError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        let ws = stream?;
        let (mut sink, mut source) = ws.split();
        let (out_tx, mut out_rx) = mpsc::channel::<Message>(512);
        let writer = tokio::spawn(async move {
            while let Some(message) = out_rx.recv().await {
                if sink.send(message).await.is_err() {
                    break;
                }
            }
        });
        let mut session = Session {
            server: self,
            tx: out_tx,
            claims: None,
            limiter: RateLimiter::new(50.0, 100.0),
            id: mixum_protocol::envelope::new_id(),
            #[cfg(feature = "real-snapshot")]
            sync_stage: None,
        };
        while let Some(message) = source.next().await {
            match message? {
                Message::Text(text) => {
                    if text.len() > session.server.config.max_message_size {
                        session
                            .send_raw(error_envelope(None, ErrCode::BadRequest, "消息过大"))
                            .await;
                        continue;
                    }
                    session.handle_text(text.as_bytes()).await;
                }
                Message::Binary(binary) => {
                    if binary.len() > 16 * 1024 * 1024 {
                        session
                            .send_raw(error_envelope(None, ErrCode::BadRequest, "二进制帧过大"))
                            .await;
                    } else {
                        session.handle_binary(&binary).await;
                    }
                }
                Message::Close(_) => break,
                Message::Ping(bytes) => {
                    let _ = session.tx.send(Message::Pong(bytes)).await;
                }
                _ => {}
            }
        }
        writer.abort();
        Ok(())
    }
}

pub struct Session {
    server: Arc<Server>,
    tx: mpsc::Sender<Message>,
    claims: Option<mixum_protocol::token::Claims>,
    limiter: RateLimiter,
    id: String,
    /// 远端导入暂存:一次 import 对账到 commit 之间有效(协议 §6)。
    #[cfg(feature = "real-snapshot")]
    sync_stage: Option<crate::sync::ImportStage>,
}

impl Session {
    async fn handle_text(&mut self, raw: &[u8]) {
        if !self.limiter.allow() {
            self.send_raw(
                Envelope::new(
                    "session.err",
                    Some(json!({"code": ErrCode::RateLimited, "message": "请求过于频繁"})),
                )
                .to_json(),
            )
            .await;
            return;
        }
        let env = match Envelope::decode(raw) {
            Ok(e) => e,
            Err(_) => {
                self.send_raw(error_envelope(
                    None,
                    ErrCode::BadRequest,
                    "消息不是合法协议 envelope",
                ))
                .await;
                return;
            }
        };
        if self.claims.is_none() && env.op != ops::op::HELLO {
            self.send_raw(
                env.error_reply(ErrCode::Unauthorized, "请先 hello")
                    .to_json(),
            )
            .await;
            return;
        }
        match env.op.as_str() {
            ops::op::HELLO => self.hello(&env).await,
            ops::op::SYS_PING => self.reply(&env, json!({"pong": env.id})).await,
            ops::op::SYS_INFO => self.sys_info(&env).await,
            ops::op::TASK_SUBMIT => self.task_submit(&env).await,
            ops::op::TASK_LIST => self.task_list(&env).await,
            ops::op::TASK_GET => self.task_get(&env).await,
            ops::op::TASK_CANCEL => self.task_cancel(&env).await,
            ops::op::TASK_EVENTS => self.task_events(&env).await,
            ops::op::SNAP_CREATE => self.snap_create(&env).await,
            ops::op::SNAP_TREE => self.snap_tree(&env).await,
            ops::op::SNAP_RESTORE => self.snap_restore(&env).await,
            ops::op::SNAP_FORK => self.snap_fork(&env).await,
            ops::op::SNAP_DELETE => self.snap_delete(&env).await,
            ops::op::SNAP_GC => self.snap_gc(&env).await,
            ops::op::SNAP_PUSH => self.snap_push(&env).await,
            "snap.push.import" => self.snap_push_import(&env).await,
            "snap.push.commit" => self.snap_push_commit(&env).await,
            ops::op::DISPLAY_LIST => self.display_list(&env).await,
            ops::op::DISPLAY_ATTACH => self.display_attach(&env).await,
            ops::op::DISPLAY_DETACH => self.display_detach(&env).await,
            ops::op::DISPLAY_WINDOWS => self.display_windows(&env).await,
            ops::op::DISPLAY_TAKEOVER => self.display_takeover(&env).await,
            ops::op::DISPLAY_INPUT => self.display_input(&env).await,
            _ => {
                self.send_raw(
                    env.error_reply(ErrCode::Unsupported, &format!("未知操作: {}", env.op))
                        .to_json(),
                )
                .await
            }
        }
    }
    async fn handle_binary(&mut self, binary: &[u8]) {
        if binary.is_empty() {
            return;
        }
        if !self.limiter.allow() {
            self.send_raw(
                Envelope::new(
                    "session.err",
                    Some(json!({"code": ErrCode::RateLimited, "message": "请求过于频繁"})),
                )
                .to_json(),
            )
            .await;
            return;
        }
        match binary[0] {
            mixum_protocol::frames::chan::CHUNK => self.handle_chunk_frame(binary).await,
            _ => {
                self.send_raw(error_envelope(None, ErrCode::BadRequest, "未知二进制通道"))
                    .await
            }
        }
    }
    #[cfg(not(feature = "real-snapshot"))]
    async fn handle_chunk_frame(&mut self, _frame: &[u8]) {}
    #[cfg(feature = "real-snapshot")]
    async fn handle_chunk_frame(&mut self, frame: &[u8]) {
        if let (Some(backend), Some(stage)) = (&self.server.sync, self.sync_stage.as_mut()) {
            backend.store_chunk(stage, frame);
        }
    }
    async fn hello(&mut self, env: &Envelope) {
        let request: HelloRequest = match payload(env) {
            Ok(v) => v,
            Err(e) => {
                self.send_raw(env.error_reply(ErrCode::BadRequest, &e).to_json())
                    .await;
                return;
            }
        };
        match self.server.authorizer.verify(&request.token) {
            Ok(claims) => {
                self.claims = Some(claims.clone());
                let response = HelloResponse {
                    session_id: self.id.clone(),
                    server: "mixumd/0.1.0".into(),
                    arch: std::env::consts::ARCH.into(),
                    kernel: kernel_name(),
                    caps: claims.caps,
                    versions: vec![1],
                    heartbeat: 30,
                };
                self.reply(env, serde_json::to_value(response).unwrap())
                    .await;
            }
            Err(err) => {
                self.send_raw(
                    env.error_reply(ErrCode::Unauthorized, &err.to_string())
                        .to_json(),
                )
                .await
            }
        }
    }
    async fn sys_info(&mut self, env: &Envelope) {
        if !self.require(env, Cap::TaskObserve).await {
            return;
        }
        let windows = self.server.broker.list().await.len();
        self.reply(env, json!({"server":"mixumd/0.1.0","arch":std::env::consts::ARCH,"kernel":kernel_name(),"os":"Arch Linux","hostname":self.server.config.hostname,"caps":self.claims.as_ref().map(|c| &c.caps),"snapshotBackend":self.server.snapshots.backend_name(),"criuAvailable":self.server.snapshots.criu_available(),"compositor":self.server.broker.online().await,"windows":windows})).await;
    }
    async fn task_submit(&mut self, env: &Envelope) {
        if !self.require(env, Cap::TaskRun).await {
            return;
        }
        let request: TaskSubmitRequest = match payload(env) {
            Ok(v) => v,
            Err(e) => {
                self.err(env, ErrCode::BadRequest, &e).await;
                return;
            }
        };
        if request.instruction.trim().is_empty() {
            self.err(env, ErrCode::BadRequest, "instruction 不能为空")
                .await;
            return;
        }
        if self
            .claims
            .as_ref()
            .is_some_and(|claims| !claims.task_id.is_empty())
        {
            self.err(env, ErrCode::Forbidden, "令牌已绑定其他任务")
                .await;
            return;
        }
        match self
            .server
            .tasks
            .submit(TaskSpec {
                instruction: request.instruction,
                agent: request.agent,
                runner: request.runner,
                params: request.params,
                snapshot_policy: request.snapshot_policy,
            })
            .await
        {
            Ok(task) => {
                self.reply(env, serde_json::to_value(task.view().await).unwrap())
                    .await
            }
            Err(e) => self.err(env, ErrCode::BadRequest, &e).await,
        }
    }
    async fn task_list(&mut self, env: &Envelope) {
        if !self.require(env, Cap::TaskObserve).await {
            return;
        }
        let mut list = Vec::new();
        for task in self.server.tasks.list().await {
            list.push(task.view().await);
        }
        self.reply(env, json!({"tasks":list})).await;
    }
    async fn task_get(&mut self, env: &Envelope) {
        if !self.require(env, Cap::TaskObserve).await {
            return;
        }
        let payload = payload_value(env);
        let id = payload
            .get("id")
            .or_else(|| payload.get("task"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if self
            .claims
            .as_ref()
            .is_some_and(|claims| !claims.task_id.is_empty() && claims.task_id != id)
        {
            self.err(env, ErrCode::Forbidden, "令牌绑定了其他任务")
                .await;
            return;
        }
        match self.server.tasks.get(&id).await {
            Some(task) => {
                self.reply(env, serde_json::to_value(task.view().await).unwrap())
                    .await
            }
            None => self.err(env, ErrCode::NotFound, "任务不存在").await,
        }
    }
    async fn task_cancel(&mut self, env: &Envelope) {
        if !self.require(env, Cap::TaskRun).await {
            return;
        }
        let payload = payload_value(env);
        let id = payload
            .get("id")
            .or_else(|| payload.get("task"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if self
            .claims
            .as_ref()
            .is_some_and(|claims| !claims.task_id.is_empty() && claims.task_id != id)
        {
            self.err(env, ErrCode::Forbidden, "令牌绑定了其他任务")
                .await;
            return;
        }
        match self.server.tasks.cancel(&id).await {
            Ok(()) => self.reply(env, json!({"id":id})).await,
            Err(e) => self.err(env, ErrCode::NotFound, &e).await,
        }
    }
    async fn task_events(&mut self, env: &Envelope) {
        if !self.require(env, Cap::TaskObserve).await {
            return;
        }
        let mut events = self.server.tasks.subscribe();
        self.reply(env, json!({"subscribed":self.id})).await;
        let tx = self.tx.clone();
        let task_scope = self
            .claims
            .as_ref()
            .map(|claims| claims.task_id.clone())
            .unwrap_or_default();
        tokio::spawn(async move {
            while let Ok(event) = events.recv().await {
                if !task_scope.is_empty() && event.task_id != task_scope {
                    continue;
                }
                if let Ok(message) = serde_json::to_string(&Envelope::new(
                    ops::op::TASK_EVENT,
                    Some(serde_json::to_value(event).unwrap()),
                )) {
                    let _ = tx.send(Message::Text(message.into())).await;
                }
            }
        });
    }
    async fn snap_create(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapManage).await {
            return;
        }
        let p = payload_value(env);
        let kind = p.get("kind").and_then(Value::as_str).unwrap_or("disk");
        match self
            .server
            .snapshots
            .create(
                kind,
                p.get("label").and_then(Value::as_str),
                p.get("parent").and_then(Value::as_str),
                p.get("branch").and_then(Value::as_str),
                p.get("auto").and_then(Value::as_bool).unwrap_or(false),
            )
            .await
        {
            Ok(node) => self.reply(env, serde_json::to_value(node).unwrap()).await,
            Err(e) => self.err(env, ErrCode::Internal, &e.to_string()).await,
        }
    }
    async fn snap_tree(&mut self, env: &Envelope) {
        if !self.require(env, Cap::TaskObserve).await {
            return;
        }
        match self.server.snapshots.tree().await {
            Ok(tree) => self.reply(env, serde_json::to_value(tree).unwrap()).await,
            Err(e) => self.err(env, ErrCode::Internal, &e.to_string()).await,
        }
    }
    async fn snap_restore(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapManage).await {
            return;
        }
        let p = payload_value(env);
        match self.server.snapshots.restore(p.get("node").and_then(Value::as_str).unwrap_or_default(), p.get("strategy").and_then(Value::as_str).unwrap_or("inplace"), p.get("label").and_then(Value::as_str)).await { Ok(r) => self.reply(env, json!({"restoredTo":r.restored_to,"newBranch":r.new_branch,"newNode":r.new_node})).await, Err(e) => self.err(env, ErrCode::SnapshotInvalid, &e.to_string()).await }
    }
    async fn snap_fork(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapManage).await {
            return;
        }
        let p = payload_value(env);
        match self
            .server
            .snapshots
            .fork(
                p.get("node").and_then(Value::as_str).unwrap_or_default(),
                p.get("branch").and_then(Value::as_str).unwrap_or_default(),
            )
            .await
        {
            Ok(branch) => self.reply(env, json!({"branch":branch})).await,
            Err(e) => self.err(env, ErrCode::NotFound, &e.to_string()).await,
        }
    }
    async fn snap_delete(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapManage).await {
            return;
        }
        let payload = payload_value(env);
        let node = payload
            .get("node")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match self.server.snapshots.delete(&node).await {
            Ok(_) => self.reply(env, json!({"deleted":node})).await,
            Err(e) => self.err(env, ErrCode::BadRequest, &e.to_string()).await,
        }
    }
    async fn snap_gc(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapManage).await {
            return;
        }
        match self.server.snapshots.gc().await {
            Ok(removed) => self.reply(env, json!({"removed":removed})).await,
            Err(e) => self.err(env, ErrCode::Internal, &e.to_string()).await,
        }
    }
    // ---------- 跨节点快照同步(协议 §6) ----------
    // snap.pull 无服务端处理器(与 reference 一致):拉取 = 反向调用对方 push,
    // 由客户端发起,未识别的 op 走「未知操作」分支。

    #[cfg(not(feature = "real-snapshot"))]
    async fn snap_push(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapSync).await {
            return;
        }
        self.err(env, ErrCode::Unsupported, "内存快照后端不支持跨节点块同步")
            .await;
    }

    #[cfg(not(feature = "real-snapshot"))]
    async fn snap_push_import(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapSync).await {
            return;
        }
        self.err(env, ErrCode::Unsupported, "内存快照后端不支持跨节点块同步")
            .await;
    }

    #[cfg(not(feature = "real-snapshot"))]
    async fn snap_push_commit(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapSync).await {
            return;
        }
        self.err(env, ErrCode::Unsupported, "内存快照后端不支持跨节点块同步")
            .await;
    }

    #[cfg(feature = "real-snapshot")]
    async fn snap_push(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapSync).await {
            return;
        }
        let Some(backend) = self.server.sync.clone() else {
            self.err(env, ErrCode::Unsupported, "快照同步后端未启用")
                .await;
            return;
        };
        let p = payload_value(env);
        let node = p
            .get("node")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let remote = p
            .get("remote")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let token = p
            .get("token")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        match crate::sync::push_node(
            backend.engine(),
            &node,
            &remote,
            &token,
            &self.server.config.hostname,
        )
        .await
        {
            Ok(stats) => self.reply(env, serde_json::to_value(stats).unwrap()).await,
            Err(crate::sync::SyncError::NotFound(m)) => self.err(env, ErrCode::NotFound, &m).await,
            Err(crate::sync::SyncError::Unsupported(m)) => {
                self.err(env, ErrCode::Unsupported, &m).await
            }
            Err(e) => {
                self.err(env, ErrCode::Internal, &format!("推送失败: {e}"))
                    .await
            }
        }
    }

    #[cfg(feature = "real-snapshot")]
    async fn snap_push_import(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapSync).await {
            return;
        }
        let Some(backend) = self.server.sync.clone() else {
            self.err(env, ErrCode::Unsupported, "快照同步后端未启用")
                .await;
            return;
        };
        let request: crate::sync::ImportRequest = match payload(env) {
            Ok(request) => request,
            Err(e) => {
                self.err(env, ErrCode::BadRequest, &e).await;
                return;
            }
        };
        let stage = backend.stage_import(request);
        self.reply(
            env,
            serde_json::to_value(crate::sync::MissingResponse {
                missing: stage.missing().to_vec(),
            })
            .unwrap(),
        )
        .await;
        self.sync_stage = Some(stage);
    }

    #[cfg(feature = "real-snapshot")]
    async fn snap_push_commit(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapSync).await {
            return;
        }
        let Some(backend) = self.server.sync.clone() else {
            self.err(env, ErrCode::Unsupported, "快照同步后端未启用")
                .await;
            return;
        };
        // 无进行中的导入时静默返回(与 reference handleRemoteCommit 一致)。
        let Some(stage) = self.sync_stage.take() else {
            return;
        };
        match backend.commit(stage) {
            Ok(node) => self.reply(env, json!({"node": node})).await,
            Err(crate::sync::SyncError::Conflict(m)) => self.err(env, ErrCode::Conflict, &m).await,
            Err(e) => self.err(env, ErrCode::Internal, &e.to_string()).await,
        }
    }
    async fn display_list(&mut self, env: &Envelope) {
        if !self.require(env, Cap::DisplayView).await {
            return;
        }
        self.reply(
            env,
            serde_json::to_value(DisplayListResponse {
                windows: self.server.broker.list().await,
            })
            .unwrap(),
        )
        .await;
    }
    async fn display_attach(&mut self, env: &Envelope) {
        if !self.require(env, Cap::DisplayView).await {
            return;
        }
        let payload = payload_value(env);
        let id = payload
            .get("window")
            .and_then(Value::as_u64)
            .unwrap_or_default() as u32;
        let sub_id = format!("{}-{}", self.id, id);
        match self
            .server
            .broker
            .attach(
                id,
                sub_id,
                payload.get("fps").and_then(Value::as_u64).unwrap_or(30) as u32,
            )
            .await
        {
            Ok((width, height, fps, mut frames)) => {
                let tx = self.tx.clone();
                tokio::spawn(async move {
                    while let Some(frame) = frames.recv().await {
                        let _ = tx.send(Message::Binary(frame.into())).await;
                    }
                });
                self.reply(
                    env,
                    json!({"window":id,"codec":"jpeg","width":width,"height":height,"fps":fps}),
                )
                .await;
            }
            Err(e) => self.err(env, ErrCode::NotFound, &e.to_string()).await,
        }
    }
    async fn display_detach(&mut self, env: &Envelope) {
        if !self.require(env, Cap::DisplayView).await {
            return;
        }
        let payload = payload_value(env);
        let id = payload
            .get("window")
            .and_then(Value::as_u64)
            .unwrap_or_default() as u32;
        self.server
            .broker
            .detach(id, &format!("{}-{}", self.id, id))
            .await;
        self.reply(env, json!({"window":id})).await;
    }
    async fn display_windows(&mut self, env: &Envelope) {
        if !self.require(env, Cap::DisplayView).await {
            return;
        }
        self.reply(env, json!({"subscribed":self.id})).await;
        let mut windows = self.server.broker.subscribe_windows();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Ok(window) = windows.recv().await {
                let envelope = Envelope::new(
                    ops::op::DISPLAY_WINDOWS,
                    Some(serde_json::to_value(window).unwrap_or(Value::Null)),
                );
                let _ = tx.send(Message::Text(envelope.to_json().into())).await;
            }
        });
    }
    async fn display_takeover(&mut self, env: &Envelope) {
        if !self.require(env, Cap::DisplayInput).await {
            return;
        }
        let p = payload_value(env);
        let id = p.get("window").and_then(Value::as_u64).unwrap_or_default() as u32;
        if !self
            .server
            .authorizer
            .window_allowed(self.claims.as_ref().unwrap(), id)
        {
            self.err(env, ErrCode::Forbidden, "令牌未授权该窗口").await;
            return;
        }
        match self
            .server
            .broker
            .takeover(
                id,
                p.get("action").and_then(Value::as_str).unwrap_or_default(),
                p.get("force").and_then(Value::as_bool).unwrap_or(false),
            )
            .await
        {
            Ok(mode) => self.reply(env, json!({"mode":mode})).await,
            Err(e) => self.err(env, ErrCode::Conflict, &e.to_string()).await,
        }
    }
    async fn display_input(&mut self, env: &Envelope) {
        if !self.require(env, Cap::DisplayInput).await {
            return;
        }
        let p = payload_value(env);
        let id = p.get("window").and_then(Value::as_u64).unwrap_or_default() as u32;
        if !self
            .server
            .authorizer
            .window_allowed(self.claims.as_ref().unwrap(), id)
        {
            self.err(env, ErrCode::Forbidden, "令牌未授权该窗口").await;
            return;
        }
        match self.server.broker.inject(id, p.clone()).await {
            Ok(()) => self.reply(env, json!({"ok":true})).await,
            Err(e) => self.err(env, ErrCode::Conflict, &e.to_string()).await,
        }
    }
    async fn require(&mut self, env: &Envelope, cap: Cap) -> bool {
        match self.claims.as_ref() {
            Some(claims) if self.server.authorizer.require(claims, cap).is_ok() => true,
            Some(_) => {
                self.err(env, ErrCode::Forbidden, "缺少能力").await;
                false
            }
            None => {
                self.err(env, ErrCode::Unauthorized, "未握手").await;
                false
            }
        }
    }
    async fn reply(&self, env: &Envelope, payload: Value) {
        self.send_raw(env.reply(Some(payload)).to_json()).await;
    }
    async fn err(&self, env: &Envelope, code: ErrCode, message: &str) {
        self.send_raw(env.error_reply(code, message).to_json())
            .await;
    }
    async fn send_raw(&self, raw: String) {
        let _ = self.tx.send(Message::Text(raw.into())).await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("网络：{0}")]
    Io(#[from] std::io::Error),
    #[error("WebSocket：{0}")]
    Ws(#[from] tokio_tungstenite::tungstenite::Error),
}

fn payload<T: serde::de::DeserializeOwned>(env: &Envelope) -> Result<T, String> {
    serde_json::from_value(env.payload.clone().unwrap_or(Value::Null)).map_err(|e| e.to_string())
}
fn payload_value(env: &Envelope) -> Value {
    env.payload.clone().unwrap_or_else(|| json!({}))
}
fn error_envelope(request: Option<&Envelope>, code: ErrCode, message: &str) -> String {
    request
        .map(|e| e.error_reply(code, message).to_json())
        .unwrap_or_else(|| {
            Envelope::new("error.err", Some(json!({"code":code,"message":message}))).to_json()
        })
}
fn kernel_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().into())
        .unwrap_or_else(|_| "unknown".into())
}

struct RateLimiter {
    rate: f64,
    burst: f64,
    tokens: Mutex<(f64, SystemTime)>,
}
impl RateLimiter {
    fn new(rate: f64, burst: f64) -> Self {
        Self {
            rate,
            burst,
            tokens: Mutex::new((burst, SystemTime::now())),
        }
    }
    fn allow(&self) -> bool {
        let Ok(mut state) = self.tokens.try_lock() else {
            return false;
        };
        let now = SystemTime::now();
        let elapsed = now
            .duration_since(state.1)
            .unwrap_or_default()
            .as_secs_f64();
        state.0 = (state.0 + elapsed * self.rate).min(self.burst);
        state.1 = now;
        if state.0 < 1.0 {
            false
        } else {
            state.0 -= 1.0;
            true
        }
    }
}

/// 生成自签 TLS 1.3 配置；证书 DER 可由调用方持久化并通过指纹做 TOFU。
pub fn generate_tls_config(
    hosts: &[String],
) -> Result<Arc<rustls::ServerConfig>, Box<dyn std::error::Error + Send + Sync>> {
    crate::auth::ensure_crypto_provider();
    let certified = rcgen::generate_simple_self_signed(hosts.to_vec())?;
    let cert = rustls::pki_types::CertificateDer::from(certified.cert.der().to_vec());
    let key = rustls::pki_types::PrivateKeyDer::Pkcs8(rustls::pki_types::PrivatePkcs8KeyDer::from(
        certified.key_pair.serialize_der(),
    ));
    Ok(Arc::new(
        rustls::ServerConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
            .with_no_client_auth()
            .with_single_cert(vec![cert], key)?,
    ))
}
