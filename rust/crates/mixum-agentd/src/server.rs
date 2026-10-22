//! WSS/WS 服务、握手、能力路由和限速。

use crate::auth::Authorizer;
use crate::display::{Broker, DisplayError};
use crate::memory::{MemoryStore, FORMAT_MAP_JSON, PROVIDER_BUILTIN};
use crate::snapshot::SnapshotEngine;
use crate::task::{Engine, InputReplyError, TaskSpec};
use crate::wire::{DisplayListResponse, HelloRequest, HelloResponse, TaskSubmitRequest};
use futures_util::{SinkExt, StreamExt};
use mixum_protocol::envelope::{new_id, Envelope};
use mixum_protocol::ops::{self, Cap, ErrCode};
use mixum_protocol::token::Claims;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tokio_rustls::{rustls, TlsAcceptor};
use tokio_tungstenite::{accept_hdr_async, tungstenite::Message, WebSocketStream};

/// session.hello.ok / sys.info / session.resume.ok 的实现标识。
pub const SERVER_NAME: &str = concat!("mixum-agentd/", env!("CARGO_PKG_VERSION"));
/// 本节点实现的 MAP profile(core/display/memory;vm/mobility 尚未实现)。
pub const PROFILES: &[&str] = &["core", "display", "memory"];
/// session.resume 的每会话环形缓冲容量(MAP session.md 要求 ≥256)。
pub const SESSION_RING_CAPACITY: usize = 512;
/// 会话空闲多久后不再允许 resume。
const SESSION_TTL: Duration = Duration::from_secs(3600);

/// WS 子协议协商:MAP 规范名 `map.v1`;过渡期同时接受旧的 `mixum.v1`。
/// 两者同时出现时优先 `map.v1`,并在应答头里回显选中的那个。
// allow:回调签名被 accept_hdr_async 钉死为 tungstenite 的 ErrorResponse
// (自身就超过 result_large_err 阈值),错误类型不可更换;错误走冷路径,
// 装箱收益仅在这一个边界上存在,故整体 allow 并在返回类型里装箱。
#[allow(clippy::result_large_err)]
fn accept_with_subprotocol<S>(
    stream: S,
) -> impl std::future::Future<
    Output = Result<WebSocketStream<S>, Box<tokio_tungstenite::tungstenite::Error>>,
>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    use tokio_tungstenite::tungstenite::handshake::server::{Request, Response};

    // 回调错误类型为 tungstenite 约定的 Response<Option<String>>;
    // 未请求任何已知子协议的客户端不报错、仅不回子协议头。
    let cb = |req: &Request,
              resp: Response|
     -> Result<
        Response,
        tokio_tungstenite::tungstenite::handshake::server::ErrorResponse,
    > {
        let offered: Vec<String> = req
            .headers()
            .get("sec-websocket-protocol")
            .and_then(|v| v.to_str().ok())
            .map(|header| header.split(',').map(|s| s.trim().to_owned()).collect())
            .unwrap_or_default();
        let selected = if offered.iter().any(|p| p == "map.v1") {
            Some("map.v1")
        } else if offered.iter().any(|p| p == "mixum.v1") {
            Some("mixum.v1")
        } else {
            None
        };
        let (mut parts, body) = resp.into_parts();
        if let Some(protocol) = selected {
            if let Ok(value) = protocol.parse() {
                parts.headers.insert("Sec-WebSocket-Protocol", value);
            }
        }
        Ok(Response::from_parts(parts, body))
    };
    async move { accept_hdr_async(stream, cb).await.map_err(Box::new) }
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
    /// 音频采集命令（输出 s16le 48k 双声道 PCM）；未配置时 audio.attach 返回 unsupported。
    #[serde(default)]
    pub audio_command: Option<String>,
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
            audio_command: None,
        }
    }
}

struct SessionRecord {
    /// 推送方向 envelope 的会话内单调序号(session.resume 的 lastSeq)。
    seq: i64,
    ring: VecDeque<Envelope>,
    tx: Option<mpsc::Sender<Message>>,
    claims: Option<Claims>,
    task_events: bool,
    display_windows: bool,
    /// 当前绑定该会话的连接 id:旧连接关闭时不得清掉新连接的 tx。
    conn: String,
    last_seen: Instant,
}

/// 跨连接会话状态:重放环、订阅标志与握手 claims 都在这里,
/// 因而 `session.resume` 无需重新 hello / 重新订阅即可继续收推送。
pub struct SessionStore {
    records: Mutex<HashMap<String, SessionRecord>>,
    capacity: usize,
    ttl: Duration,
}

impl SessionStore {
    pub fn new(capacity: usize) -> Self {
        Self {
            records: Mutex::new(HashMap::new()),
            capacity,
            ttl: SESSION_TTL,
        }
    }

    fn prune(&self, records: &mut HashMap<String, SessionRecord>) {
        let now = Instant::now();
        // 已连接的会话不按 TTL 回收;断开的会话从断开时刻起算,
        // 服务器推送本身不刷新 last_seen(否则长任务会让会话永不过期)。
        records.retain(|_, record| {
            record.tx.is_some() || now.duration_since(record.last_seen) < self.ttl
        });
        if records.len() >= 1024 {
            if let Some(oldest) = records
                .iter()
                .min_by_key(|(_, record)| record.last_seen)
                .map(|(id, _)| id.clone())
            {
                records.remove(&oldest);
            }
        }
    }

    pub async fn create(&self, conn: &str, claims: Claims, tx: mpsc::Sender<Message>) -> String {
        let id = new_id();
        let mut records = self.records.lock().await;
        self.prune(&mut records);
        records.insert(
            id.clone(),
            SessionRecord {
                seq: 0,
                ring: VecDeque::with_capacity(self.capacity),
                tx: Some(tx),
                claims: Some(claims),
                task_events: false,
                display_windows: false,
                conn: conn.to_owned(),
                last_seen: Instant::now(),
            },
        );
        id
    }

    /// 会话是否存在且未过期(并刷新活跃时间)。
    pub async fn touch(&self, id: &str) -> Option<Claims> {
        let mut records = self.records.lock().await;
        let expired = records
            .get(id)
            .is_some_and(|record| record.tx.is_none() && record.last_seen.elapsed() > self.ttl);
        if expired {
            records.remove(id);
            return None;
        }
        let record = records.get_mut(id)?;
        record.last_seen = Instant::now();
        record.claims.clone()
    }

    /// 绑定新连接并重放 `seq > last_seq` 的推送 envelope。
    /// 依次发送 `session.resume.ok` 与重放帧;全程持有会话锁,
    /// 因此新推送不会插到重放之前(顺序保证见 session.md §Resume)。
    pub async fn resume(
        &self,
        id: &str,
        conn: &str,
        last_seq: i64,
        request: &Envelope,
        tx: mpsc::Sender<Message>,
    ) -> Option<Claims> {
        let mut records = self.records.lock().await;
        if records
            .get(id)
            .is_some_and(|record| record.tx.is_none() && record.last_seen.elapsed() > self.ttl)
        {
            records.remove(id);
            return None;
        }
        let record = records.get_mut(id)?;
        record.conn = conn.to_owned();
        record.tx = Some(tx.clone());
        record.last_seen = Instant::now();
        let claims = record.claims.clone();
        let replay: Vec<Envelope> = record
            .ring
            .iter()
            .filter(|envelope| envelope.seq > last_seq)
            .cloned()
            .collect();
        let replayed = replay.len();
        let response = json!({
            "resumed": true,
            "replayed": replayed,
            // MAP 只规定 resumed/replayed;以下为附加字段,便于客户端恢复协商状态。
            "sessionId": id,
            "server": SERVER_NAME,
            "arch": std::env::consts::ARCH,
            "kernel": kernel_name(),
            "caps": claims.as_ref().map(|claims| &claims.caps),
            "profiles": PROFILES,
            "versions": [1],
            "heartbeat": 30,
        });
        let _ = tx
            .send(Message::Text(
                request.reply(Some(response)).to_json().into(),
            ))
            .await;
        for envelope in replay {
            let _ = tx.send(Message::Text(envelope.to_json().into())).await;
        }
        claims
    }

    /// 推送一条控制消息:分配 seq、入环、发给当前连接。
    /// 返回 false 表示会话已不存在(订阅泵可据此退出)。
    pub async fn push(&self, id: &str, mut envelope: Envelope) -> bool {
        let mut records = self.records.lock().await;
        let Some(record) = records.get_mut(id) else {
            return false;
        };
        record.seq += 1;
        envelope.seq = record.seq;
        if record.ring.len() >= self.capacity {
            record.ring.pop_front();
        }
        record.ring.push_back(envelope.clone());
        if let Some(tx) = &record.tx {
            let _ = tx.send(Message::Text(envelope.to_json().into())).await;
        }
        true
    }

    /// 向所有持有 admin 能力的在线会话推送一条事件；只用于审批类请求。
    pub async fn broadcast_admin(&self, mut envelope: Envelope) {
        let mut records = self.records.lock().await;
        for record in records.values_mut() {
            let admin = record
                .claims
                .as_ref()
                .is_some_and(|claims| mixum_protocol::token::has_cap(claims, Cap::Admin));
            if !admin {
                continue;
            }
            let Some(tx) = &record.tx else {
                continue;
            };
            record.seq += 1;
            envelope.seq = record.seq;
            if record.ring.len() >= self.capacity {
                record.ring.pop_front();
            }
            record.ring.push_back(envelope.clone());
            let _ = tx.send(Message::Text(envelope.to_json().into())).await;
        }
    }

    /// 标记 task.events 已订阅;返回 true 表示本次是首次(需要启动订阅泵)。
    pub async fn subscribe_task_events(&self, id: &str) -> bool {
        let mut records = self.records.lock().await;
        match records.get_mut(id) {
            Some(record) if !record.task_events => {
                record.task_events = true;
                true
            }
            _ => false,
        }
    }

    pub async fn subscribe_display_windows(&self, id: &str) -> bool {
        let mut records = self.records.lock().await;
        match records.get_mut(id) {
            Some(record) if !record.display_windows => {
                record.display_windows = true;
                true
            }
            _ => false,
        }
    }

    /// 连接断开:仅当它仍是当前连接时解绑(保留环供 resume)。
    pub async fn close(&self, id: &str, conn: &str) -> bool {
        let mut records = self.records.lock().await;
        match records.get_mut(id) {
            Some(record) if record.conn == conn => {
                record.tx = None;
                // 断开时刻起算 resume TTL;之后的服务端推送不续期。
                record.last_seen = Instant::now();
                true
            }
            _ => false,
        }
    }
}

pub struct Server {
    pub config: Config,
    authorizer: Authorizer,
    tasks: Arc<Engine>,
    broker: Broker,
    snapshots: Arc<dyn SnapshotEngine>,
    /// MAP-Memory 节点本地存储(dataDir/memory)。
    memory: Arc<MemoryStore>,
    /// 跨连接会话(session.resume 重放环)。
    sessions: Arc<SessionStore>,
    /// 容器音频采集（audio.attach 的首个订阅者启动 capture 进程）。
    audio: Arc<crate::audio::AudioHub>,
    /// 单条 sudo 请求的审批与 root 执行。
    privilege: Arc<crate::privilege::PrivilegeBroker>,
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
        let memory = Arc::new(MemoryStore::open(
            std::path::Path::new(&config.data_dir).join("memory"),
        ));
        let audio = crate::audio::AudioHub::new(config.audio_command.clone());
        let sessions = Arc::new(SessionStore::new(SESSION_RING_CAPACITY));
        let privilege =
            crate::privilege::PrivilegeBroker::new(Arc::clone(&sessions), config.data_dir.clone());
        Arc::new(Self {
            config,
            authorizer,
            tasks,
            broker,
            snapshots,
            memory,
            audio,
            privilege,
            sessions,
            #[cfg(feature = "real-snapshot")]
            sync: None,
        })
    }

    pub fn privilege(&self) -> Arc<crate::privilege::PrivilegeBroker> {
        Arc::clone(&self.privilege)
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
        stream: Result<WebSocketStream<S>, Box<tokio_tungstenite::tungstenite::Error>>,
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
            id: String::new(),
            conn_id: new_id(),
            bound: false,
            forwarders: Vec::new(),
            audio_forwarder: None,
            audio_guard: None,
            #[cfg(feature = "real-snapshot")]
            sync_stage: None,
            file_uploads: HashMap::new(),
            file_seq: 0,
        };
        while let Some(message) = source.next().await {
            match message.map_err(Box::new)? {
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
        if session.bound
            && session
                .server
                .sessions
                .close(&session.id, &session.conn_id)
                .await
        {
            // 当前连接断开:清掉它挂在该会话名下的窗口订阅(帧不重放)。
            session
                .server
                .broker
                .detach_prefix(&format!("{}-", session.id))
                .await;
        }
        writer.abort();
        // 帧转发任务持有会话出站通道,断连后必须终止,否则永久泄漏。
        for forwarder in session.forwarders.drain(..) {
            forwarder.abort();
        }
        if let Some(forwarder) = session.audio_forwarder.take() {
            forwarder.abort();
        }
        Ok(())
    }
}

struct FileUpload {
    temp: PathBuf,
    final_path: PathBuf,
    size: u64,
    received: u64,
    sha256: Sha256,
    file: Option<tokio::fs::File>,
    committed: bool,
}

impl Drop for FileUpload {
    fn drop(&mut self) {
        if !self.committed {
            let _ = std::fs::remove_file(&self.temp);
        }
    }
}

pub struct Session {
    server: Arc<Server>,
    tx: mpsc::Sender<Message>,
    claims: Option<Claims>,
    limiter: RateLimiter,
    /// MAP 会话 id(session.hello 分配,session.resume 复用)。
    id: String,
    /// 本连接的内部 id;用于避免旧连接清理新连接的会话状态。
    conn_id: String,
    bound: bool,
    /// display.attach 的帧转发任务:会话结束时统一 abort(队列关闭是常规路径)。
    forwarders: Vec<tokio::task::JoinHandle<()>>,
    /// audio.attach 的转发任务与退订守卫（会话结束/重挂时清理）。
    audio_forwarder: Option<tokio::task::JoinHandle<()>>,
    audio_guard: Option<crate::audio::AudioGuard>,
    /// 远端导入暂存:一次 import 对账到 commit 之间有效(协议 §6)。
    #[cfg(feature = "real-snapshot")]
    sync_stage: Option<crate::sync::ImportStage>,
    /// 本连接进行中的文件上传。
    file_uploads: HashMap<u32, FileUpload>,
    file_seq: u32,
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
        if self.claims.is_none() {
            // 握手前允许:hello/session.resume 与 sys.ping;sys.info 仅回
            // profiles/server 供客户端渲染连接错误;其余一律 unauthorized。
            match env.op.as_str() {
                ops::op::SESSION_HELLO
                | ops::op::HELLO
                | ops::op::SESSION_RESUME
                | ops::op::SYS_PING => {}
                ops::op::SYS_INFO => {
                    self.sys_info(&env).await;
                    return;
                }
                _ => {
                    self.send_raw(
                        env.error_reply(ErrCode::Unauthorized, "请先 session.hello")
                            .to_json(),
                    )
                    .await;
                    return;
                }
            }
        }
        match env.op.as_str() {
            ops::op::SESSION_HELLO | ops::op::HELLO => self.hello(&env).await,
            ops::op::SESSION_RESUME => self.resume_session(&env).await,
            ops::op::SYS_PING => self.reply(&env, json!({"pong": env.id})).await,
            ops::op::SYS_INFO => self.sys_info(&env).await,
            ops::op::TASK_SUBMIT => self.task_submit(&env).await,
            ops::op::TASK_LIST => self.task_list(&env).await,
            ops::op::TASK_GET => self.task_get(&env).await,
            ops::op::TASK_CANCEL => self.task_cancel(&env).await,
            ops::op::TASK_EVENTS => self.task_events(&env).await,
            ops::op::TASK_REPLY => self.task_reply(&env).await,
            ops::op::MEM_PROVIDERS => self.mem_providers(&env).await,
            ops::op::MEM_SEARCH => self.mem_search(&env).await,
            ops::op::MEM_GET => self.mem_get(&env).await,
            ops::op::MEM_PUT => self.mem_put(&env).await,
            ops::op::MEM_DELETE => self.mem_delete(&env).await,
            ops::op::MEM_LIST => self.mem_list(&env).await,
            ops::op::MEM_IMPORT => self.mem_import(&env).await,
            ops::op::MEM_EXPORT => self.mem_export(&env).await,
            ops::op::SNAP_CREATE => self.snap_create(&env).await,
            ops::op::SNAP_TREE => self.snap_tree(&env).await,
            ops::op::SNAP_RESTORE => self.snap_restore(&env).await,
            ops::op::SNAP_FORK => self.snap_fork(&env).await,
            ops::op::SNAP_DELETE => self.snap_delete(&env).await,
            ops::op::SNAP_GC => self.snap_gc(&env).await,
            ops::op::SNAP_EXPORT => self.snap_export(&env).await,
            ops::op::SNAP_IMPORT => self.snap_import(&env).await,
            ops::op::SNAP_PUSH => self.snap_push(&env).await,
            "snap.push.import" => self.snap_push_import(&env).await,
            "snap.push.commit" => self.snap_push_commit(&env).await,
            ops::op::DISPLAY_LIST => self.display_list(&env).await,
            ops::op::DISPLAY_ATTACH => self.display_attach(&env).await,
            ops::op::DISPLAY_DETACH => self.display_detach(&env).await,
            ops::op::DISPLAY_WINDOWS => self.display_windows(&env).await,
            ops::op::DISPLAY_TAKEOVER => self.display_takeover(&env).await,
            ops::op::DISPLAY_INPUT => self.display_input(&env).await,
            ops::op::DISPLAY_CLIPBOARD => self.display_clipboard(&env).await,
            ops::op::PRIVILEGE_DECISION => self.privilege_decision(&env).await,
            ops::op::PRIVILEGE_LIST => self.privilege_list(&env).await,
            ops::op::AUDIO_ATTACH => self.audio_attach(&env).await,
            ops::op::AUDIO_DETACH => self.audio_detach(&env).await,
            ops::op::FILE_PUT => self.file_put(&env).await,
            ops::op::FILE_PUT_COMMIT => self.file_put_commit(&env).await,
            ops::op::FILE_GET => self.file_get(&env).await,
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
            mixum_protocol::frames::chan::FILE => self.handle_file_chunk(binary).await,
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
                self.id = self
                    .server
                    .sessions
                    .create(&self.conn_id, claims.clone(), self.tx.clone())
                    .await;
                self.bound = true;
                self.claims = Some(claims.clone());
                let response = HelloResponse {
                    session_id: self.id.clone(),
                    server: SERVER_NAME.into(),
                    arch: std::env::consts::ARCH.into(),
                    kernel: kernel_name(),
                    caps: claims.caps,
                    profiles: PROFILES.iter().map(|p| (*p).to_owned()).collect(),
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
    async fn resume_session(&mut self, env: &Envelope) {
        let payload = payload_value(env);
        let session_id = payload
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let last_seq = payload.get("lastSeq").and_then(Value::as_i64).unwrap_or(0);
        if session_id.is_empty() {
            self.err(env, ErrCode::BadRequest, "sessionId 不能为空")
                .await;
            return;
        }
        if self.server.sessions.touch(&session_id).await.is_none() {
            self.err(
                env,
                ErrCode::NotFound,
                "会话不存在或已过期,请重新 session.hello",
            )
            .await;
            return;
        }
        match self
            .server
            .sessions
            .resume(&session_id, &self.conn_id, last_seq, env, self.tx.clone())
            .await
        {
            Some(claims) => {
                self.claims = Some(claims);
                self.id = session_id;
                self.bound = true;
            }
            None => {
                self.err(
                    env,
                    ErrCode::NotFound,
                    "会话不存在或已过期,请重新 session.hello",
                )
                .await
            }
        }
    }
    async fn sys_info(&mut self, env: &Envelope) {
        if self.claims.is_none() {
            // 握手前只回 profiles/server(session.md §System ops)。
            self.reply(env, json!({"server": SERVER_NAME, "profiles": PROFILES}))
                .await;
            return;
        }
        if !self.require(env, Cap::TaskObserve).await {
            return;
        }
        let windows = self.server.broker.list().await.len();
        self.reply(env, json!({"server":SERVER_NAME,"arch":std::env::consts::ARCH,"kernel":kernel_name(),"os":"Arch Linux","hostname":self.server.config.hostname,"caps":self.claims.as_ref().map(|c| &c.caps),"profiles":PROFILES,"snapshotBackend":self.server.snapshots.backend_name(),"criuAvailable":self.server.snapshots.criu_available(),"compositor":self.server.broker.online().await,"windows":windows,"memoryProviders":["builtin"]})).await;
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
        // 订阅属于会话而非连接:resume 后无需重新订阅。
        let first_subscription = self.server.sessions.subscribe_task_events(&self.id).await;
        self.reply(env, json!({"subscribed": true})).await;
        if !first_subscription {
            return;
        }
        let mut events = self.server.tasks.subscribe();
        let sessions = self.server.sessions.clone();
        let session_id = self.id.clone();
        let task_scope = self
            .claims
            .as_ref()
            .map(|claims| claims.task_id.clone())
            .unwrap_or_default();
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => {
                        if !task_scope.is_empty() && event.task_id != task_scope {
                            continue;
                        }
                        let envelope = Envelope::new(
                            ops::op::TASK_EVENT,
                            Some(serde_json::to_value(event).unwrap_or(Value::Null)),
                        );
                        // 会话被回收后订阅泵退出。
                        if !sessions.push(&session_id, envelope).await {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        eprintln!("任务事件积压,丢弃 {skipped} 条");
                    }
                    Err(_) => break,
                }
            }
        });
    }
    async fn task_reply(&mut self, env: &Envelope) {
        if !self.require(env, Cap::TaskObserve).await {
            return;
        }
        let payload = payload_value(env);
        let task_id = payload
            .get("taskId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let reply_id = payload
            .get("replyId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let value = payload
            .get("value")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if task_id.is_empty() || reply_id.is_empty() {
            self.err(env, ErrCode::BadRequest, "taskId/replyId 不能为空")
                .await;
            return;
        }
        let Some(value) = value else {
            self.err(env, ErrCode::BadRequest, "value 必须是字符串")
                .await;
            return;
        };
        if self
            .claims
            .as_ref()
            .is_some_and(|claims| !claims.task_id.is_empty() && claims.task_id != task_id)
        {
            self.err(env, ErrCode::Forbidden, "令牌绑定了其他任务")
                .await;
            return;
        }
        if self.server.tasks.get(&task_id).await.is_none() {
            self.err(env, ErrCode::NotFound, "任务不存在").await;
            return;
        }
        match self
            .server
            .tasks
            .reply_input(&task_id, &reply_id, value)
            .await
        {
            Ok(()) => self.reply(env, json!({"delivered": true})).await,
            Err(InputReplyError::NotFound) => {
                self.err(env, ErrCode::NotFound, "输入请求不存在或已过期")
                    .await
            }
            Err(InputReplyError::Conflict) => {
                self.err(env, ErrCode::Conflict, "该输入请求已答复").await
            }
        }
    }
    // ---------- MAP-Memory(profile: MAP-Memory) ----------
    async fn mem_providers(&mut self, env: &Envelope) {
        if !self.require(env, Cap::MemRead).await {
            return;
        }
        self.reply(env, self.server.memory.providers()).await;
    }
    async fn mem_search(&mut self, env: &Envelope) {
        if !self.require(env, Cap::MemRead).await {
            return;
        }
        let payload = payload_value(env);
        let query = payload
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let kinds = string_list(payload.get("kinds"));
        let providers = string_list(payload.get("providers"));
        let limit = payload
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        let hits = self.server.memory.search(
            query,
            kinds.as_deref(),
            providers.as_deref(),
            payload.get("scope"),
            limit,
        );
        self.reply(env, hits).await;
    }
    async fn mem_get(&mut self, env: &Envelope) {
        if !self.require(env, Cap::MemRead).await {
            return;
        }
        let id = payload_value(env)
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if id.is_empty() {
            self.err(env, ErrCode::BadRequest, "id 不能为空").await;
            return;
        }
        match self.server.memory.get(&id) {
            Ok(record) => self.reply(env, record).await,
            Err(error) => self.err(env, error.code(), &error.to_string()).await,
        }
    }
    async fn mem_put(&mut self, env: &Envelope) {
        if !self.require(env, Cap::MemWrite).await {
            return;
        }
        let Some(record) = payload_value(env).get("record").cloned() else {
            self.err(env, ErrCode::BadRequest, "缺少 record").await;
            return;
        };
        match self.server.memory.put(record) {
            Ok(saved) => self.reply(env, saved).await,
            Err(error) => self.err(env, error.code(), &error.to_string()).await,
        }
    }
    async fn mem_delete(&mut self, env: &Envelope) {
        if !self.require(env, Cap::MemWrite).await {
            return;
        }
        let id = payload_value(env)
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if id.is_empty() {
            self.err(env, ErrCode::BadRequest, "id 不能为空").await;
            return;
        }
        match self.server.memory.delete(&id) {
            Ok(()) => self.reply(env, json!({"deleted": true})).await,
            Err(error) => self.err(env, error.code(), &error.to_string()).await,
        }
    }
    async fn mem_list(&mut self, env: &Envelope) {
        if !self.require(env, Cap::MemRead).await {
            return;
        }
        let payload = payload_value(env);
        let provider = payload.get("provider").and_then(Value::as_str);
        let kind = payload.get("kind").and_then(Value::as_str);
        let cursor = payload.get("cursor").and_then(Value::as_str);
        let limit = payload
            .get("limit")
            .and_then(Value::as_u64)
            .map(|value| value as usize);
        self.reply(env, self.server.memory.list(provider, kind, cursor, limit))
            .await;
    }
    async fn mem_import(&mut self, env: &Envelope) {
        if !self.require(env, Cap::MemWrite).await {
            return;
        }
        let payload = payload_value(env);
        let provider = payload
            .get("provider")
            .and_then(Value::as_str)
            .unwrap_or(PROVIDER_BUILTIN)
            .to_owned();
        let format = payload
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or(FORMAT_MAP_JSON);
        let records = payload
            .get("records")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        match self.server.memory.import(&provider, format, records) {
            Ok(result) => self.reply(env, result).await,
            Err(error) => self.err(env, error.code(), &error.to_string()).await,
        }
    }
    async fn mem_export(&mut self, env: &Envelope) {
        if !self.require(env, Cap::MemRead).await {
            return;
        }
        let payload = payload_value(env);
        let provider = payload.get("provider").and_then(Value::as_str);
        let format = payload
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or(FORMAT_MAP_JSON);
        match self.server.memory.export(provider, format) {
            Ok(result) => self.reply(env, result).await,
            Err(error) => self.err(env, error.code(), &error.to_string()).await,
        }
    }
    async fn snap_create(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapManage).await {
            return;
        }
        let p = payload_value(env);
        let kind = p.get("kind").and_then(Value::as_str).unwrap_or("disk");
        let mut tags = std::collections::BTreeMap::new();
        if let Some(base) = std::env::var("MIXUM_BASE").ok().filter(|v| !v.is_empty()) {
            tags.insert("base".into(), base);
        }
        if let Some(digest) = std::env::var("MIXUM_BASE_DIGEST")
            .ok()
            .filter(|v| !v.is_empty())
        {
            tags.insert("baseDigest".into(), digest);
        }
        if let Some(extra) = p.get("tags").and_then(Value::as_object) {
            for (key, value) in extra {
                if let Some(value) = value.as_str() {
                    tags.insert(key.clone(), value.to_owned());
                }
            }
        }
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
            Ok(mut node) => {
                if !tags.is_empty() {
                    if let Err(error) = self.server.snapshots.annotate(&node.id, &tags).await {
                        self.err(env, ErrCode::Internal, &error.to_string()).await;
                        return;
                    }
                    node.tags = tags;
                }
                self.reply(env, serde_json::to_value(node).unwrap()).await
            }
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
        let node = p.get("node").and_then(Value::as_str).unwrap_or_default();
        // A base image is part of the filesystem contract: refuse to restore a
        // snapshot recorded against a different base instead of silently mixing
        // package trees.
        if let (Ok(current), Ok(tree)) = (
            std::env::var("MIXUM_BASE_DIGEST"),
            self.server.snapshots.tree().await,
        ) {
            if let Some(required) = tree
                .nodes
                .iter()
                .find(|item| item.id == node)
                .and_then(|item| item.tags.get("baseDigest"))
            {
                if !current.is_empty() && required != &current {
                    self.err(
                        env,
                        ErrCode::SnapshotInvalid,
                        &format!(
                            "快照属于 base {required}，当前 base 是 {current}；请先切回同一 base"
                        ),
                    )
                    .await;
                    return;
                }
            }
        }
        match self.server.snapshots.restore(node, p.get("strategy").and_then(Value::as_str).unwrap_or("inplace"), p.get("label").and_then(Value::as_str)).await { Ok(r) => self.reply(env, json!({"restoredTo":r.restored_to,"newBranch":r.new_branch,"newNode":r.new_node})).await, Err(e) => self.err(env, ErrCode::SnapshotInvalid, &e.to_string()).await }
    }
    /// 导出节点为 `.mart`，文件落在 work_root/transfers，客户端用 file.get 下载。
    async fn snap_export(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapManage).await {
            return;
        }
        let p = payload_value(env);
        let node = p.get("node").and_then(Value::as_str).unwrap_or_default();
        if node.is_empty() {
            self.err(env, ErrCode::BadRequest, "node 为空").await;
            return;
        }
        let safe: String = node
            .chars()
            .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '-' || *ch == '_')
            .collect();
        if safe.is_empty() {
            self.err(env, ErrCode::BadRequest, "node 含非法字符").await;
            return;
        }
        let name = format!("mixum-{safe}.mart");
        let Some(path) = transfer_path(&self.server.config.work_root, &name) else {
            self.err(env, ErrCode::Internal, "无法定位 transfers 目录")
                .await;
            return;
        };
        if let Some(dir) = path.parent() {
            if let Err(error) = tokio::fs::create_dir_all(dir).await {
                self.err(env, ErrCode::Internal, &format!("创建传输目录: {error}"))
                    .await;
                return;
            }
        }
        match self.server.snapshots.export_mart(node, &path).await {
            Ok(size) => {
                let sha256 = sha256_file(&path).await.unwrap_or_default();
                self.reply(env, json!({"file": name, "size": size, "sha256": sha256}))
                    .await;
            }
            Err(error) => {
                self.err(env, ErrCode::SnapshotInvalid, &error.to_string())
                    .await
            }
        }
    }

    /// 从 transfers 里的 `.mart` 导入节点，挂到当前 head。
    async fn snap_import(&mut self, env: &Envelope) {
        if !self.require(env, Cap::SnapManage).await {
            return;
        }
        let p = payload_value(env);
        let name = p.get("file").and_then(Value::as_str).unwrap_or_default();
        let Some(path) = transfer_path(&self.server.config.work_root, name) else {
            self.err(env, ErrCode::BadRequest, "文件名无效").await;
            return;
        };
        if !path.is_file() {
            self.err(env, ErrCode::NotFound, "迁移文件不存在").await;
            return;
        }
        let origin = p.get("origin").and_then(Value::as_str).unwrap_or("unknown");
        match self.server.snapshots.import_mart(&path, origin).await {
            Ok(node) => self.reply(env, json!({"node": node})).await,
            Err(error) => {
                self.err(env, ErrCode::SnapshotInvalid, &error.to_string())
                    .await
            }
        }
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
        let codec = payload
            .get("codec")
            .and_then(Value::as_str)
            .unwrap_or(crate::display::CODEC_JPEG);
        let quality = payload.get("quality").and_then(Value::as_u64).unwrap_or(0) as u32;
        match self
            .server
            .broker
            .attach(
                id,
                sub_id,
                payload.get("fps").and_then(Value::as_u64).unwrap_or(30) as u32,
                codec,
                quality,
            )
            .await
        {
            Ok((width, height, fps, mut sub)) => {
                let codec = sub.codec.clone();
                let tx = self.tx.clone();
                let forwarder = tokio::spawn(async move {
                    while let Some(frame) = sub.frames.recv().await {
                        let _ = tx.send(Message::Binary(frame.into())).await;
                    }
                });
                self.forwarders.push(forwarder);
                self.reply(
                    env,
                    json!({"window":id,"codec":codec,"width":width,"height":height,"fps":fps}),
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
        let first_subscription = self
            .server
            .sessions
            .subscribe_display_windows(&self.id)
            .await;
        self.reply(env, json!({"subscribed": true})).await;
        if !first_subscription {
            return;
        }
        let mut windows = self.server.broker.subscribe_windows();
        let sessions = self.server.sessions.clone();
        let session_id = self.id.clone();
        tokio::spawn(async move {
            loop {
                match windows.recv().await {
                    Ok(window) => {
                        let envelope = Envelope::new(
                            ops::op::DISPLAY_WINDOWS,
                            Some(serde_json::to_value(window).unwrap_or(Value::Null)),
                        );
                        if !sessions.push(&session_id, envelope).await {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                        eprintln!("窗口事件积压,丢弃 {skipped} 条");
                    }
                    Err(_) => break,
                }
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
        if p.get("type").and_then(Value::as_str) == Some("text") {
            let ok = p
                .get("text")
                .and_then(Value::as_str)
                .is_some_and(|t| !t.is_empty() && t.len() <= 8192);
            if !ok {
                self.err(env, ErrCode::BadRequest, "text 为空或超长(上限 8192 字节)")
                    .await;
                return;
            }
        }
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
            Err(DisplayError::NotFound(_)) => {
                self.err(env, ErrCode::NotFound, &format!("窗口不存在: {id}"))
                    .await;
            }
            Err(e) => self.err(env, ErrCode::Conflict, &e.to_string()).await,
        }
    }
    async fn privilege_list(&mut self, env: &Envelope) {
        if !self.require(env, Cap::Admin).await {
            return;
        }
        let pending = self.server.privilege.list().await;
        self.reply(env, json!({"requests": pending})).await;
    }

    async fn privilege_decision(&mut self, env: &Envelope) {
        if !self.require(env, Cap::Admin).await {
            return;
        }
        let p = payload_value(env);
        let id = p.get("id").and_then(Value::as_str).unwrap_or_default();
        if id.is_empty() {
            self.err(env, ErrCode::BadRequest, "缺少 id").await;
            return;
        }
        let decision = crate::privilege::Decision {
            approve: p.get("approve").and_then(Value::as_bool).unwrap_or(false),
            reason: p
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            scope: p
                .get("scope")
                .and_then(Value::as_str)
                .unwrap_or("once")
                .to_owned(),
        };
        match self.server.privilege.decide(id, decision).await {
            Ok(()) => self.reply(env, json!({"ok": true})).await,
            Err(error) => self.err(env, ErrCode::Conflict, &error).await,
        }
    }

    async fn audio_attach(&mut self, env: &Envelope) {
        if !self.require(env, Cap::DisplayView).await {
            return;
        }
        self.drop_audio();
        match self.server.audio.subscribe() {
            Ok(mut sub) => {
                let tx = self.tx.clone();
                let forwarder = tokio::spawn(async move {
                    while let Ok(frame) = sub.rx.recv().await {
                        let _ = tx.send(Message::Binary(frame.to_vec().into())).await;
                    }
                });
                self.audio_forwarder = Some(forwarder);
                self.audio_guard = Some(crate::audio::AudioGuard::new(self.server.audio.clone()));
                self.reply(
                    env,
                    json!({
                        "sampleRate": sub.sample_rate,
                        "channels": sub.channels,
                        "format": "s16le",
                        "chunkMs": sub.chunk_ms
                    }),
                )
                .await;
            }
            Err(e) => self.err(env, ErrCode::Unsupported, &e).await,
        }
    }
    async fn audio_detach(&mut self, env: &Envelope) {
        if !self.require(env, Cap::DisplayView).await {
            return;
        }
        self.drop_audio();
        self.reply(env, json!({"ok": true})).await;
    }
    fn drop_audio(&mut self) {
        if let Some(forwarder) = self.audio_forwarder.take() {
            forwarder.abort();
        }
        self.audio_guard = None;
    }
    async fn display_clipboard(&mut self, env: &Envelope) {
        if !self.require(env, Cap::DisplayView).await {
            return;
        }
        // B compositor 的 socket 由 /run/mixum + mixum-art-0 固定；wl-clipboard
        // 作为独立客户端由 compositor 正常 brokering，避免在 compositor 里
        // 解析 Smithay selection source。
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            tokio::process::Command::new("wl-paste")
                .arg("--no-newline")
                .env("XDG_RUNTIME_DIR", "/run/mixum")
                .env("WAYLAND_DISPLAY", "mixum-art-0")
                .output(),
        )
        .await;
        let text = match output {
            Ok(Ok(output)) if output.status.success() => {
                String::from_utf8_lossy(&output.stdout).to_string()
            }
            _ => String::new(),
        };
        self.reply(env, json!({"text": text})).await;
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
    async fn file_put(&mut self, env: &Envelope) {
        if !self.require(env, Cap::Admin).await {
            return;
        }
        #[derive(serde::Deserialize)]
        struct Request {
            name: String,
            size: u64,
            #[serde(default)]
            sha256: Option<String>,
        }
        let request: Request = match payload(env) {
            Ok(request) => request,
            Err(error) => {
                self.err(env, ErrCode::BadRequest, &error).await;
                return;
            }
        };
        if request.size > 4 * 1024 * 1024 * 1024 {
            self.err(env, ErrCode::BadRequest, "文件超过 4GiB 上限")
                .await;
            return;
        }
        let Some(name) = Path::new(&request.name)
            .file_name()
            .and_then(|n| n.to_str())
        else {
            self.err(env, ErrCode::BadRequest, "文件名无效").await;
            return;
        };
        let name = name.to_owned();
        if name.is_empty() {
            self.err(env, ErrCode::BadRequest, "文件名为空").await;
            return;
        }
        let dir = Path::new(&self.server.config.work_root).join("transfers");
        if let Err(error) = tokio::fs::create_dir_all(&dir).await {
            self.err(env, ErrCode::Internal, &format!("创建传输目录: {error}"))
                .await;
            return;
        }
        let mut final_path = dir.join(&name);
        let mut suffix = 1u32;
        while final_path.exists() {
            final_path = dir.join(format!("{name}.{suffix}"));
            suffix += 1;
        }
        self.file_seq = self.file_seq.wrapping_add(1);
        let transfer = self.file_seq.max(1);
        let temp = dir.join(format!(".upload-{}-{transfer}", self.conn_id));
        let Ok(file) = tokio::fs::File::create(&temp).await else {
            self.err(env, ErrCode::Internal, "创建上传暂存失败").await;
            return;
        };
        self.file_uploads.insert(
            transfer,
            FileUpload {
                temp,
                final_path: final_path.clone(),
                size: request.size,
                received: 0,
                sha256: Sha256::new(),
                file: Some(file),
                committed: false,
            },
        );
        self.reply(
            env,
            json!({
                "transfer": transfer,
                "path": final_path.to_string_lossy(),
                "name": name,
                "size": request.size,
                "sha256": request.sha256,
            }),
        )
        .await;
    }

    async fn file_put_commit(&mut self, env: &Envelope) {
        if !self.require(env, Cap::Admin).await {
            return;
        }
        #[derive(serde::Deserialize)]
        struct Request {
            transfer: u32,
            #[serde(default)]
            sha256: Option<String>,
        }
        let request: Request = match payload(env) {
            Ok(request) => request,
            Err(error) => {
                self.err(env, ErrCode::BadRequest, &error).await;
                return;
            }
        };
        let Some(mut upload) = self.file_uploads.remove(&request.transfer) else {
            self.err(env, ErrCode::NotFound, "上传不存在").await;
            return;
        };
        if upload.received != upload.size {
            self.err(env, ErrCode::BadRequest, "上传大小不完整").await;
            return;
        }
        if let Some(mut file) = upload.file.take() {
            if let Err(error) = file.flush().await {
                self.err(env, ErrCode::Internal, &format!("flush 上传: {error}"))
                    .await;
                return;
            }
        }
        let digest = upload.sha256.clone().finalize();
        let hex = digest
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>();
        if let Some(expected) = request.sha256.as_deref() {
            if !expected.eq_ignore_ascii_case(&hex) {
                self.err(env, ErrCode::BadRequest, "sha256 不匹配").await;
                return;
            }
        }
        if let Err(error) = tokio::fs::rename(&upload.temp, &upload.final_path).await {
            self.err(env, ErrCode::Internal, &format!("提交上传: {error}"))
                .await;
            return;
        }
        upload.committed = true;
        self.reply(
            env,
            json!({
                "path": upload.final_path.to_string_lossy(),
                "size": upload.size,
                "sha256": hex,
            }),
        )
        .await;
    }

    async fn handle_file_chunk(&mut self, binary: &[u8]) {
        let Ok((transfer, offset, data)) = mixum_protocol::frames::unmarshal_file(binary) else {
            self.send_raw(error_envelope(None, ErrCode::BadRequest, "文件块格式错误"))
                .await;
            return;
        };
        let Some(upload) = self.file_uploads.get_mut(&transfer) else {
            self.send_raw(error_envelope(None, ErrCode::NotFound, "上传不存在"))
                .await;
            return;
        };
        if offset != upload.received || upload.received + data.len() as u64 > upload.size {
            self.file_uploads.remove(&transfer);
            self.send_raw(error_envelope(
                None,
                ErrCode::BadRequest,
                "文件块偏移/大小错误",
            ))
            .await;
            return;
        }
        let Some(file) = upload.file.as_mut() else {
            self.file_uploads.remove(&transfer);
            return;
        };
        if let Err(error) = file.write_all(data).await {
            self.file_uploads.remove(&transfer);
            self.send_raw(error_envelope(
                None,
                ErrCode::Internal,
                &format!("写上传: {error}"),
            ))
            .await;
            return;
        }
        upload.sha256.update(data);
        upload.received += data.len() as u64;
    }

    fn safe_work_file(&self, raw: &str) -> Option<PathBuf> {
        let base = Path::new(&self.server.config.work_root);
        std::fs::create_dir_all(base).ok()?;
        let base = base.canonicalize().ok()?;
        let candidate = if Path::new(raw).is_absolute() {
            PathBuf::from(raw)
        } else {
            base.join(raw)
        };
        let canonical = candidate.canonicalize().ok()?;
        canonical.starts_with(&base).then_some(canonical)
    }

    async fn file_get(&mut self, env: &Envelope) {
        if !self.require(env, Cap::Admin).await {
            return;
        }
        #[derive(serde::Deserialize)]
        struct Request {
            path: String,
        }
        let request: Request = match payload(env) {
            Ok(request) => request,
            Err(error) => {
                self.err(env, ErrCode::BadRequest, &error).await;
                return;
            }
        };
        let Some(path) = self.safe_work_file(&request.path) else {
            self.err(env, ErrCode::Forbidden, "路径不在 work_root 内")
                .await;
            return;
        };
        let Ok(metadata) = tokio::fs::metadata(&path).await else {
            self.err(env, ErrCode::NotFound, "文件不存在").await;
            return;
        };
        if !metadata.is_file() {
            self.err(env, ErrCode::BadRequest, "不是普通文件").await;
            return;
        }
        self.file_seq = self.file_seq.wrapping_add(1);
        let transfer = self.file_seq.max(1);
        let size = metadata.len();
        self.reply(
            env,
            json!({
                "transfer": transfer,
                "path": path.to_string_lossy(),
                "name": path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
                "size": size,
            }),
        )
        .await;
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let Ok(mut file) = tokio::fs::File::open(&path).await else {
                return;
            };
            let mut offset = 0u64;
            let mut buffer = vec![0u8; 256 * 1024];
            while let Ok(read) = file.read(&mut buffer).await {
                if read == 0 {
                    break;
                }
                let frame = mixum_protocol::frames::marshal_file(transfer, offset, &buffer[..read]);
                if tx.send(Message::Binary(frame.into())).await.is_err() {
                    return;
                }
                offset += read as u64;
            }
            let done = Envelope::new(
                "file.get.done",
                Some(json!({"transfer": transfer, "size": size})),
            );
            let _ = tx.send(Message::Text(done.to_json().into())).await;
        });
    }

    async fn send_raw(&self, raw: String) {
        let _ = self.tx.send(Message::Text(raw.into())).await;
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("网络：{0}")]
    Io(#[from] std::io::Error),
    // tungstenite::Error 体积超过 result_large_err 阈值,装箱以保
    // Result<(), ServerError> 在热路径签名上的传递成本。
    #[error("WebSocket：{0}")]
    Ws(#[from] Box<tokio_tungstenite::tungstenite::Error>),
}

fn payload<T: serde::de::DeserializeOwned>(env: &Envelope) -> Result<T, String> {
    serde_json::from_value(env.payload.clone().unwrap_or(Value::Null)).map_err(|e| e.to_string())
}
/// 把迁移文件名限制在 `work_root/transfers/` 下，拒绝路径穿越。
fn transfer_path(work_root: &str, name: &str) -> Option<PathBuf> {
    let file = Path::new(name).file_name()?.to_str()?;
    if file.is_empty() {
        return None;
    }
    Some(Path::new(work_root).join("transfers").join(file))
}

async fn sha256_file(path: &Path) -> std::io::Result<String> {
    let mut file = tokio::fs::File::open(path).await?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn payload_value(env: &Envelope) -> Value {
    env.payload.clone().unwrap_or_else(|| json!({}))
}
/// 可选字符串数组字段(mem.search 的 kinds/providers)。
fn string_list(value: Option<&Value>) -> Option<Vec<String>> {
    value.and_then(Value::as_array).map(|items| {
        items
            .iter()
            .filter_map(|item| item.as_str().map(str::to_owned))
            .collect()
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use mixum_protocol::token::Claims;

    fn claims() -> Claims {
        Claims {
            sub: "session-ttl-test".into(),
            caps: vec![Cap::TaskObserve],
            iat: 0,
            exp: 0,
            jti: String::new(),
            task_id: String::new(),
            windows: vec![],
        }
    }

    /// 断开的会话按 TTL 过期;仍连接的会话不因 TTL 被回收。
    #[tokio::test]
    async fn session_ttl_applies_only_after_disconnect() {
        let store = SessionStore::new(4);
        let (tx, _rx) = mpsc::channel(4);
        let connected = store.create("conn-1", claims(), tx).await;
        let (tx2, _rx2) = mpsc::channel(4);
        let disconnected = store.create("conn-2", claims(), tx2).await;
        assert!(store.close(&disconnected, "conn-2").await);

        {
            let mut records = store.records.lock().await;
            let old = Instant::now() - SESSION_TTL - Duration::from_secs(1);
            records
                .get_mut(&connected)
                .expect("connected session")
                .last_seen = old;
            records
                .get_mut(&disconnected)
                .expect("disconnected session")
                .last_seen = old;
        }

        assert!(
            store.touch(&connected).await.is_some(),
            "仍连接的会话不应被 TTL 回收"
        );
        assert!(
            store.touch(&disconnected).await.is_none(),
            "断开的会话应按 TTL 过期"
        );
    }

    #[test]
    fn transfer_path_stays_inside_transfers() {
        let path = transfer_path("/var/lib/mixum/work", "../../etc/passwd").unwrap();
        assert_eq!(path, PathBuf::from("/var/lib/mixum/work/transfers/passwd"));
        assert!(transfer_path("/var/lib/mixum/work", "").is_none());
    }
}
