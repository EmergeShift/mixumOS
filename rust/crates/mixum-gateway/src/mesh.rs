//! 跨副本转发(M3b):gateway 之间的私有 WebSocket mesh。
//!
//! 拓扑:节点只向一个 gateway 实例注册(owner);客户端连到任意实例。
//! 非 owner 实例把信封经 mesh 转给 owner,owner 用自己的本地连接送达节点,
//! 节点的应答再经同一条 mesh 回送请求方。
//!
//! 私有约定(非公有协议,仅副本间):
//! - 首条消息 `mesh.hello {instance, token}`,共享密钥鉴权;
//! - `mesh.text {session, node?, text}`:请求方→owner 带 `node`;owner→请求方
//!   只带 `session`;
//! - `mesh.binary {session, data(base64)}`(视频帧等二进制);
//! - `mesh.close {session}`:客户端断开或节点重连时清理回环会话。
//!
//! `session` 形如 `ms-<发起实例>-<序号>`,owner 侧据此把应答原路送回。

use crate::{is_mesh_session, MeshRouter, NodeRecord, MESH_SESSION_PREFIX};
use async_trait::async_trait;
use base64::Engine;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, WebSocketStream};

/// mesh 消费者的入站事件(由 gateway 的后台任务处理)。
#[derive(Debug)]
pub enum MeshEvent {
    Text { session: String, text: String },
    Binary { session: String, data: Vec<u8> },
    Close { session: String },
}

struct OutboundSession {
    client: String,
    peer: String,
}

#[derive(Clone)]
struct Shared {
    /// peer 实例 → 出站口(入站连接与出站连接都注册在这里)。
    peers: Arc<RwLock<HashMap<String, mpsc::Sender<Value>>>>,
    /// mesh 地址 → peer 实例(避免重复拨号)。
    addrs: Arc<RwLock<HashMap<String, String>>>,
    /// 请求方侧:mesh 会话 → (本地客户端会话, peer)。
    outbound: Arc<Mutex<HashMap<String, OutboundSession>>>,
    /// owner 侧:mesh 会话 → peer 实例。
    inbound: Arc<Mutex<HashMap<String, String>>>,
    events: mpsc::Sender<MeshEvent>,
}

pub struct MeshHub {
    instance: String,
    token: String,
    shared: Shared,
    seq: AtomicU64,
    events_rx: Mutex<mpsc::Receiver<MeshEvent>>,
}

impl MeshHub {
    pub fn new(instance: impl Into<String>, token: impl Into<String>) -> Arc<Self> {
        let (events_tx, events_rx) = mpsc::channel(512);
        let shared = Shared {
            peers: Arc::new(RwLock::new(HashMap::new())),
            addrs: Arc::new(RwLock::new(HashMap::new())),
            outbound: Arc::new(Mutex::new(HashMap::new())),
            inbound: Arc::new(Mutex::new(HashMap::new())),
            events: events_tx,
        };
        Arc::new(Self {
            instance: instance.into(),
            token: token.into(),
            shared,
            seq: AtomicU64::new(1),
            events_rx: Mutex::new(events_rx),
        })
    }

    pub fn instance(&self) -> &str {
        &self.instance
    }

    pub async fn next_event(&self) -> Option<MeshEvent> {
        self.events_rx.lock().await.recv().await
    }

    /// 接收一条入站 mesh 连接(服务端):校验 hello 后进入消息循环。
    pub async fn serve_connection<S>(self: Arc<Self>, ws: WebSocketStream<S>) -> Result<(), String>
    where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sink, mut source) = ws.split();
        let first = tokio::time::timeout(std::time::Duration::from_secs(10), source.next())
            .await
            .map_err(|_| "mesh hello 超时".to_owned())?
            .ok_or_else(|| "mesh 连接关闭".to_owned())?
            .map_err(|e| e.to_string())?;
        let hello: Value = match first {
            Message::Text(text) => serde_json::from_str(&text).map_err(|e| e.to_string())?,
            _ => return Err("mesh 首条消息必须是 hello".into()),
        };
        if hello.get("op").and_then(Value::as_str) != Some("mesh.hello") {
            return Err("mesh 首条消息必须是 hello".into());
        }
        let peer = hello
            .pointer("/payload/instance")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let token = hello
            .pointer("/payload/token")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if peer.is_empty() || token != self.token {
            return Err("mesh hello 鉴权失败".into());
        }
        let (tx, rx) = mpsc::channel::<Value>(256);
        spawn_writer(sink, rx);
        // hello.ok 先于消息注册。
        let _ = tx
            .send(json!({"op":"mesh.hello.ok","payload":{"instance": self.instance}}))
            .await;
        self.shared.peers.write().await.insert(peer.clone(), tx);

        while let Some(message) = source.next().await {
            let message = message.map_err(|e| e.to_string())?;
            let text = match message {
                Message::Text(text) => text.to_string(),
                Message::Close(_) => break,
                _ => continue,
            };
            let Ok(wire) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            handle_wire(&self.shared, &peer, wire).await;
        }
        drop_peer(&self.shared, &peer).await;
        Ok(())
    }

    /// 请求方拨号 owner 的 mesh 地址,返回 peer 实例。
    async fn ensure_peer(&self, addr: &str) -> Result<String, String> {
        if addr.is_empty() {
            return Err("节点 owner 未公布 mesh 地址".to_owned());
        }
        if let Some(peer) = self.shared.addrs.read().await.get(addr).cloned() {
            if self.shared.peers.read().await.contains_key(&peer) {
                return Ok(peer);
            }
        }
        let (mut ws, _) = connect_async(addr)
            .await
            .map_err(|e| format!("连接 mesh {addr} 失败: {e}"))?;
        ws.send(Message::Text(
            json!({"op":"mesh.hello","payload":{"instance": self.instance, "token": self.token}})
                .to_string()
                .into(),
        ))
        .await
        .map_err(|e| e.to_string())?;
        let reply = tokio::time::timeout(std::time::Duration::from_secs(10), ws.next())
            .await
            .map_err(|_| "mesh hello.ok 超时".to_owned())?
            .ok_or_else(|| "mesh 连接在 hello.ok 前关闭".to_owned())?
            .map_err(|e| e.to_string())?;
        let reply: Value = match reply {
            Message::Text(text) => serde_json::from_str(&text).map_err(|e| e.to_string())?,
            _ => return Err("mesh hello.ok 非文本".into()),
        };
        if reply.get("op").and_then(Value::as_str) != Some("mesh.hello.ok") {
            return Err("mesh hello 被拒绝".into());
        }
        let peer = reply
            .pointer("/payload/instance")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if peer.is_empty() {
            return Err("mesh hello.ok 缺少实例名".into());
        }
        let (sink, source) = ws.split();
        let (tx, rx) = mpsc::channel::<Value>(256);
        spawn_writer(sink, rx);
        spawn_reader(self.shared.clone(), peer.clone(), source);
        self.shared.peers.write().await.insert(peer.clone(), tx);
        self.shared
            .addrs
            .write()
            .await
            .insert(addr.to_owned(), peer.clone());
        Ok(peer)
    }

    async fn send_peer(&self, peer: &str, wire: Value) -> Result<(), String> {
        let tx = self
            .shared
            .peers
            .read()
            .await
            .get(peer)
            .cloned()
            .ok_or_else(|| format!("mesh peer 不在线: {peer}"))?;
        tx.send(wire)
            .await
            .map_err(|_| format!("mesh peer 发送失败: {peer}"))
    }
}

fn spawn_writer<S>(mut sink: SplitSink<WebSocketStream<S>, Message>, mut rx: mpsc::Receiver<Value>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(wire) = rx.recv().await {
            if sink
                .send(Message::Text(wire.to_string().into()))
                .await
                .is_err()
            {
                break;
            }
        }
    });
}

fn spawn_reader<S>(shared: Shared, peer: String, mut source: SplitStream<WebSocketStream<S>>)
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        while let Some(message) = source.next().await {
            let Ok(message) = message else { break };
            let text = match message {
                Message::Text(text) => text.to_string(),
                Message::Close(_) => break,
                _ => continue,
            };
            let Ok(wire) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            handle_wire(&shared, &peer, wire).await;
        }
        drop_peer(&shared, &peer).await;
    });
}

async fn handle_wire(shared: &Shared, peer: &str, wire: Value) {
    let op = wire.get("op").and_then(Value::as_str).unwrap_or_default();
    match op {
        "mesh.text" => {
            let session = wire
                .pointer("/payload/session")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let text = wire
                .pointer("/payload/text")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if session.is_empty() {
                return;
            }
            let is_reply = shared.outbound.lock().await.contains_key(&session);
            if !is_reply {
                shared
                    .inbound
                    .lock()
                    .await
                    .insert(session.clone(), peer.to_owned());
            }
            let _ = shared.events.send(MeshEvent::Text { session, text }).await;
        }
        "mesh.binary" => {
            let session = wire
                .pointer("/payload/session")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let data = wire
                .pointer("/payload/data")
                .and_then(Value::as_str)
                .and_then(|encoded| {
                    base64::engine::general_purpose::STANDARD
                        .decode(encoded)
                        .ok()
                })
                .unwrap_or_default();
            if session.is_empty() || data.is_empty() {
                return;
            }
            if shared.outbound.lock().await.contains_key(&session) {
                let _ = shared
                    .events
                    .send(MeshEvent::Binary { session, data })
                    .await;
            }
        }
        "mesh.close" => {
            let session = wire
                .pointer("/payload/session")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            if session.is_empty() {
                return;
            }
            shared.inbound.lock().await.remove(&session);
            let _ = shared.events.send(MeshEvent::Close { session }).await;
        }
        "mesh.error" => {
            let message = wire
                .pointer("/payload/message")
                .and_then(Value::as_str)
                .unwrap_or("mesh error");
            eprintln!("mesh peer {peer}: {message}");
        }
        _ => {}
    }
}

async fn drop_peer(shared: &Shared, peer: &str) {
    shared.peers.write().await.remove(peer);
    shared.addrs.write().await.retain(|_, p| p != peer);
    shared.inbound.lock().await.retain(|_, p| p != peer);
    shared
        .outbound
        .lock()
        .await
        .retain(|_, session| session.peer != peer);
}

#[async_trait]
impl MeshRouter for MeshHub {
    async fn forward_text(
        &self,
        client_session: &str,
        record: &NodeRecord,
        text: &str,
    ) -> Result<String, String> {
        let peer = self.ensure_peer(&record.owner_addr).await?;
        if peer != record.owner {
            return Err(format!(
                "mesh 地址归属不符: {} 声称 {}",
                record.owner_addr, peer
            ));
        }
        let session = format!(
            "{MESH_SESSION_PREFIX}{}-{}",
            self.instance,
            self.seq.fetch_add(1, Ordering::Relaxed)
        );
        self.shared.outbound.lock().await.insert(
            session.clone(),
            OutboundSession {
                client: client_session.to_owned(),
                peer: peer.clone(),
            },
        );
        let wire = json!({
            "op":"mesh.text",
            "payload":{"session": session, "node": record.name, "text": text}
        });
        if let Err(e) = self.send_peer(&peer, wire).await {
            self.shared.outbound.lock().await.remove(&session);
            return Err(e);
        }
        Ok(session)
    }

    async fn client_session(&self, mesh_session: &str) -> Option<String> {
        self.shared
            .outbound
            .lock()
            .await
            .get(mesh_session)
            .map(|session| session.client.clone())
    }

    async fn reply_text(&self, mesh_session: &str, text: &str) {
        if !is_mesh_session(mesh_session) {
            return;
        }
        let Some(peer) = self.shared.inbound.lock().await.get(mesh_session).cloned() else {
            eprintln!("mesh 应答找不到会话 {mesh_session}");
            return;
        };
        let wire = json!({"op":"mesh.text","payload":{"session": mesh_session, "text": text}});
        if let Err(e) = self.send_peer(&peer, wire).await {
            eprintln!("mesh 应答失败: {e}");
        }
    }

    async fn reply_binary(&self, mesh_session: &str, frame: &[u8]) {
        let Some(peer) = self.shared.inbound.lock().await.get(mesh_session).cloned() else {
            return;
        };
        let wire = json!({
            "op":"mesh.binary",
            "payload":{
                "session": mesh_session,
                "data": base64::engine::general_purpose::STANDARD.encode(frame)
            }
        });
        if let Err(e) = self.send_peer(&peer, wire).await {
            eprintln!("mesh 二进制应答失败: {e}");
        }
    }

    async fn close_client(&self, client_session: &str) {
        let sessions: Vec<(String, String)> = self
            .shared
            .outbound
            .lock()
            .await
            .iter()
            .filter(|(_, session)| session.client == client_session)
            .map(|(mesh, session)| (mesh.clone(), session.peer.clone()))
            .collect();
        for (session, peer) in sessions {
            let _ = self
                .send_peer(
                    &peer,
                    json!({"op":"mesh.close","payload":{"session": session}}),
                )
                .await;
            self.shared.outbound.lock().await.remove(&session);
        }
    }

    async fn close_inbound(&self, mesh_session: &str) {
        self.shared.inbound.lock().await.remove(mesh_session);
    }
}
