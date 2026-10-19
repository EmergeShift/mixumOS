//! 跨节点快照同步(协议 §6):接收端暂存区与推送端远端客户端。
//!
//! 仅在 `real-snapshot` 特性下编译——块对账依赖真实 CAS 与树存储。
//! 行为与已移除的 Go 参考实现逐字对齐,由 tests/cross-protocol 与 e2e 锁定:
//! 推送流程严格单飞行(import → 缺失块二进制流 → commit)。

use crate::snapshot::SharedEngine;
use futures_util::{SinkExt, StreamExt};
use mixum_protocol::envelope::Envelope;
use mixum_protocol::ops::op;
use mixum_snapshot::{Manifest, TreeNode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, Message};
use tokio_tungstenite::Connector;

/// 远端导入的节点元数据。空字符串字段由接收端按 Go strOr 语义补默认值。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct RemoteNodeMeta {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub parent: String,
    #[serde(default)]
    pub branch: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub kind: String,
    #[serde(default)]
    pub created_at: i64,
    #[serde(default)]
    pub kernel: String,
    #[serde(default)]
    pub arch: String,
    #[serde(default)]
    pub origin: String,
}

#[derive(Debug, Deserialize)]
pub struct ImportRequest {
    pub node: RemoteNodeMeta,
    pub manifest: Manifest,
    pub chunks: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MissingResponse {
    pub missing: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub struct CommitRequest {
    #[serde(default)]
    pub count: u32,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PushStats {
    pub node: String,
    pub chunks_sent: u32,
    pub bytes_sent: i64,
    pub dedup_ratio: String,
}

#[derive(Debug, thiserror::Error)]
pub enum SyncError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Unsupported(String),
    #[error("{0}")]
    Conflict(String),
    #[error("{0}")]
    Protocol(String),
}

/// 接收端单连接导入暂存:对账结果、待收块与已收块。
pub struct ImportStage {
    node: RemoteNodeMeta,
    manifest: Manifest,
    missing: Vec<String>,
    received: HashSet<String>,
}

impl ImportStage {
    /// 对账出的缺失块列表(import 应答负载)。
    pub fn missing(&self) -> &[String] {
        &self.missing
    }
}

/// 共享真实快照引擎的同步后端(与 RealSnapshotEngine 同一实例)。
pub struct SyncBackend {
    engine: SharedEngine,
}

impl SyncBackend {
    pub fn new(engine: SharedEngine) -> Self {
        Self { engine }
    }

    pub fn engine(&self) -> &SharedEngine {
        &self.engine
    }

    /// snap.push.import:逐哈希对账自有 CAS,回缺失列表并暂存导入。
    pub fn stage_import(&self, request: ImportRequest) -> ImportStage {
        let engine = self.engine.lock().expect("快照引擎锁");
        let missing = request
            .chunks
            .iter()
            .filter(|hash| !engine.store.has(hash))
            .cloned()
            .collect();
        ImportStage {
            node: request.node,
            manifest: request.manifest,
            missing,
            received: HashSet::new(),
        }
    }

    /// 二进制块帧([0x03][32B 原始 sha256][数据]):校验哈希后落 CAS。
    /// 哈希不匹配或写入失败时仅告警不计数,commit 阶段按缺失处理。
    pub fn store_chunk(&self, stage: &mut ImportStage, frame: &[u8]) {
        if frame.len() < 33 {
            return;
        }
        let hash: String = frame[1..33].iter().map(|b| format!("{b:02x}")).collect();
        let engine = self.engine.lock().expect("快照引擎锁");
        match engine.store.put_hashed(&hash, &frame[33..]) {
            Ok(()) => {
                stage.received.insert(hash);
            }
            Err(e) => eprintln!("远端块写入失败: {e}"),
        }
    }

    /// snap.push.commit:校验缺失块全部到齐后落树。
    /// 父节点不在本地树中时,节点成为新分支根,label 追加「(远端根)」。
    pub fn commit(&self, stage: ImportStage) -> Result<String, SyncError> {
        for hash in &stage.missing {
            if !stage.received.contains(hash) {
                let head = hash.get(..12).unwrap_or(hash);
                return Err(SyncError::Conflict(format!("缺失块未送达: {head}")));
            }
        }
        let engine = self.engine.lock().expect("快照引擎锁");
        // 幂等:同 id 节点此前已落树(commit 恒发后的幂等重推路径),回既有 id。
        if !stage.node.id.is_empty() {
            if let Ok(existing) = engine.tree.get(&stage.node.id) {
                return Ok(existing.id);
            }
        }
        let mut node = TreeNode::new(
            stage.node.id.clone(),
            stage.node.parent.clone(),
            if stage.node.branch.is_empty() {
                "imported".to_owned()
            } else {
                stage.node.branch.clone()
            },
            if stage.node.kind.is_empty() {
                mixum_snapshot::KIND_DISK.to_owned()
            } else {
                stage.node.kind.clone()
            },
        );
        node.label = stage.node.label.clone();
        node.kernel = stage.node.kernel.clone();
        node.arch = stage.node.arch.clone();
        node.origin = if stage.node.origin.is_empty() {
            "remote".to_owned()
        } else {
            stage.node.origin.clone()
        };
        node.disk = Some(stage.manifest);
        if !node.parent.is_empty() && engine.tree.get(&node.parent).is_err() {
            node.parent = String::new();
            node.label = format!("{}（远端根）", node.label);
        }
        let added = engine
            .tree
            .add(node)
            .map_err(|e| SyncError::Conflict(format!("落树失败: {e}")))?;
        Ok(added.id)
    }
}

/// snap.push:把本端节点推给远端(A 侧),返回推送统计。
/// `origin` 是本端主机名,随导入元数据发往远端。
pub async fn push_node(
    engine: &SharedEngine,
    node_id: &str,
    remote: &str,
    token: &str,
    origin: &str,
) -> Result<PushStats, SyncError> {
    // 1. 本端取节点与清单;不跨 await 持锁。
    let (meta, manifest, chunks) = {
        let engine = engine.lock().expect("快照引擎锁");
        let node = engine
            .tree
            .get(node_id)
            .map_err(|e| SyncError::NotFound(e.to_string()))?;
        let manifest = node
            .disk
            .clone()
            .ok_or_else(|| SyncError::Unsupported("纯内存快照暂不支持流转".into()))?;
        let meta = RemoteNodeMeta {
            id: node.id.clone(),
            parent: node.parent.clone(),
            branch: node.branch.clone(),
            label: node.label.clone(),
            kind: node.kind.clone(),
            created_at: node.created_at,
            kernel: node.kernel.clone(),
            arch: node.arch.clone(),
            origin: origin.to_owned(),
        };
        let chunks = manifest.chunks();
        (meta, manifest, chunks)
    };

    // 2. 拨号远端并握手(mixumd-sync/0.1)。
    let mut client = RemoteClient::connect(remote, token).await?;

    // 3. import 对账,拿缺失块列表。
    let reply = client
        .call(
            format!("{}.import", op::SNAP_PUSH),
            serde_json::json!({"node": meta, "manifest": manifest, "chunks": chunks}),
            Duration::from_secs(180),
        )
        .await?;
    let missing: MissingResponse =
        serde_json::from_value(reply).map_err(|e| SyncError::Protocol(e.to_string()))?;
    let need: HashSet<&str> = missing.missing.iter().map(String::as_str).collect();

    // 4. 只传缺失块:[0x03][32B 原始 sha256][数据]。
    let mut sent = 0u32;
    let mut bytes_sent = 0i64;
    for hash in &chunks {
        if !need.contains(hash.as_str()) {
            continue;
        }
        let data = {
            let engine = engine.lock().expect("快照引擎锁");
            engine
                .store
                .get(hash)
                .map_err(|e| SyncError::Protocol(e.to_string()))?
        };
        let sum = Sha256::digest(&data);
        let hex: String = sum.iter().map(|b| format!("{b:02x}")).collect();
        if hex != *hash {
            let head = hash.get(..12).unwrap_or(hash);
            return Err(SyncError::Protocol(format!("块 {head} 数据不匹配")));
        }
        let mut frame = Vec::with_capacity(33 + data.len());
        frame.push(mixum_protocol::frames::chan::CHUNK);
        frame.extend_from_slice(&sum);
        frame.extend_from_slice(&data);
        client.send_binary(frame).await?;
        sent += 1;
        bytes_sent += data.len() as i64;
    }

    // 5. commit 恒发:落树发生在这一步,与是否传输了块无关——
    //    0 块节点与幂等重推的缺失列表为空,跳过 commit 会使节点永不到达远端树。
    client
        .call(
            format!("{}.commit", op::SNAP_PUSH),
            serde_json::json!({"count": sent}),
            Duration::from_secs(180),
        )
        .await?;
    let dedup = if sent > 0 && (sent as usize) < chunks.len() {
        format!("{:.2}", chunks.len() as f64 / sent as f64)
    } else {
        "1.00".to_owned()
    };
    Ok(PushStats {
        node: meta.id,
        chunks_sent: sent,
        bytes_sent,
        dedup_ratio: dedup,
    })
}

struct RemoteClient {
    sink: futures_util::stream::SplitSink<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        Message,
    >,
    replies: mpsc::Receiver<Envelope>,
}

impl RemoteClient {
    /// 拨号远端 mixumd:缺省补 wss://(自签证书跳过校验,指纹固定见 docs/security.md),
    /// 显式 ws:// 走明文(仅测试用,与 Go client.go 一致)。
    async fn connect(remote: &str, token: &str) -> Result<Self, SyncError> {
        let url = if remote.contains("://") {
            remote.to_owned()
        } else {
            format!("wss://{remote}")
        };
        let request = format!("{url}/ws")
            .into_client_request()
            .map_err(|e| SyncError::Protocol(format!("server: 连接远端失败: {e}")))?;
        let connector = if url.starts_with("ws://") {
            Connector::Plain
        } else {
            Connector::Rustls(Arc::new(insecure_tls_client()))
        };
        let (ws, _response) =
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, Some(connector))
                .await
                .map_err(|e| SyncError::Protocol(format!("server: 连接远端失败: {e}")))?;
        let (sink, mut source) = futures_util::StreamExt::split(ws);
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            while let Some(Ok(message)) = source.next().await {
                let Message::Text(text) = message else {
                    continue;
                };
                let Ok(env) = Envelope::decode(text.as_bytes()) else {
                    continue;
                };
                if tx.send(env).await.is_err() {
                    break;
                }
            }
        });
        let mut client = Self { sink, replies: rx };
        client.hello(token).await?;
        Ok(client)
    }

    async fn hello(&mut self, token: &str) -> Result<(), SyncError> {
        self.request(
            op::HELLO,
            serde_json::json!({"token": token, "client": "mixumd-sync/0.1"}),
            Duration::from_secs(15),
        )
        .await
        .map_err(|e| match e {
            CallError::Remote(code, message) => {
                SyncError::Protocol(format!("远端拒绝握手: {code} {message}"))
            }
            other => SyncError::Protocol(other.to_string()),
        })?;
        Ok(())
    }

    /// 发送一条控制消息并等待 replyTo 匹配的应答(单飞行)。
    async fn call(
        &mut self,
        op_name: String,
        payload: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, SyncError> {
        match self.request(&op_name, payload, timeout).await {
            Ok(value) => Ok(value),
            Err(CallError::Remote(code, message)) => {
                Err(SyncError::Protocol(format!("{code}: {message}")))
            }
            Err(e) => Err(SyncError::Protocol(e.to_string())),
        }
    }

    async fn request(
        &mut self,
        op_name: &str,
        payload: serde_json::Value,
        timeout: Duration,
    ) -> Result<serde_json::Value, CallError> {
        let envelope = Envelope::new(op_name, Some(payload));
        let id = envelope.id.clone();
        self.sink
            .send(Message::Text(envelope.to_json().into()))
            .await
            .map_err(|e| CallError::Io(e.to_string()))?;
        let reply = tokio::time::timeout(timeout, self.next_reply())
            .await
            .map_err(|_| CallError::Timeout(op_name.to_owned()))??;
        if reply.op.ends_with(".err") {
            let code = reply
                .payload
                .as_ref()
                .and_then(|p| p.get("code"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or("internal")
                .to_owned();
            let message = reply
                .payload
                .as_ref()
                .and_then(|p| p.get("message"))
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            return Err(CallError::Remote(code, message));
        }
        if reply.reply_to != id {
            return Err(CallError::Protocol("应答与请求不匹配".into()));
        }
        Ok(reply.payload.unwrap_or(serde_json::Value::Null))
    }

    async fn next_reply(&mut self) -> Result<Envelope, CallError> {
        self.replies
            .recv()
            .await
            .ok_or_else(|| CallError::Io("远端连接中断".into()))
    }

    async fn send_binary(&mut self, frame: Vec<u8>) -> Result<(), SyncError> {
        self.sink
            .send(Message::Binary(frame.into()))
            .await
            .map_err(|e| SyncError::Protocol(e.to_string()))
    }
}

#[derive(Debug, thiserror::Error)]
enum CallError {
    #[error("{0}")]
    Io(String),
    #[error("远端响应超时: {0}")]
    Timeout(String),
    #[error("{0}: {1}")]
    Remote(String, String),
    #[error("{0}")]
    Protocol(String),
}

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};

/// 自签环境的服务端证书跳过校验(TLS 1.3);生产路径必须用指纹固定。
#[derive(Debug)]
struct SkipServerVerification;

impl ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        use rustls::SignatureScheme::*;
        vec![
            RSA_PKCS1_SHA256,
            RSA_PKCS1_SHA384,
            RSA_PKCS1_SHA512,
            RSA_PSS_SHA256,
            RSA_PSS_SHA384,
            RSA_PSS_SHA512,
            ECDSA_NISTP256_SHA256,
            ECDSA_NISTP384_SHA384,
            ED25519,
        ]
    }
}

fn insecure_tls_client() -> rustls::ClientConfig {
    crate::auth::ensure_crypto_provider();
    rustls::ClientConfig::builder_with_protocol_versions(&[&rustls::version::TLS13])
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth()
}

#[cfg(test)]
mod tests {
    use super::*;
    use mixum_snapshot::{EngineOptions, KIND_DISK};

    fn open_backend(tag: &str) -> (tempfile::TempDir, SyncBackend) {
        let dir = tempfile::tempdir().unwrap();
        let engine = mixum_snapshot::Engine::open(EngineOptions {
            data_dir: dir.path().join("snapshots"),
            root: dir.path().join("work"),
            skip: vec![],
            encrypt: false,
        })
        .unwrap_or_else(|e| panic!("{tag}: {e}"));
        let shared = Arc::new(std::sync::Mutex::new(engine));
        (dir, SyncBackend::new(shared))
    }

    fn sample_manifest(data: &[u8]) -> (Manifest, Vec<String>) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("blob");
        std::fs::write(&path, data).unwrap();
        let store_dir = tempfile::tempdir().unwrap();
        let store = mixum_snapshot::Store::open(store_dir.path(), None, "test").unwrap();
        let (manifest, _stats) =
            mixum_snapshot::Manifest::pack_directory(dir.path(), &store, &[]).unwrap();
        let chunks = manifest.chunks();
        (manifest, chunks)
    }

    #[test]
    fn commit_requires_every_missing_chunk() {
        let (_dir, backend) = open_backend("commit-missing");
        let (manifest, chunks) = sample_manifest(b"payload");
        let stage = backend.stage_import(ImportRequest {
            node: RemoteNodeMeta {
                id: "snap_remote_1".into(),
                branch: "main".into(),
                kind: KIND_DISK.into(),
                origin: "node-a".into(),
                ..Default::default()
            },
            manifest,
            chunks: chunks.clone(),
        });
        assert_eq!(stage.missing, chunks);
        // 一个块都没收就 commit:冲突并点名缺失块。
        let err = backend.commit(stage).unwrap_err().to_string();
        assert!(err.contains("缺失块未送达"), "实际：{err}");
    }

    #[test]
    fn commit_accepts_bad_hash_frame_but_rejects_at_commit() {
        let (_dir, backend) = open_backend("commit-bad-hash");
        let (manifest, chunks) = sample_manifest(b"payload");
        let mut stage = backend.stage_import(ImportRequest {
            node: RemoteNodeMeta {
                id: "snap_remote_2".into(),
                branch: "main".into(),
                kind: KIND_DISK.into(),
                ..Default::default()
            },
            manifest,
            chunks: chunks.clone(),
        });
        // 帧内数据与声明哈希不符:写入被拒,commit 报缺失。
        let mut frame = vec![mixum_protocol::frames::chan::CHUNK];
        frame.extend_from_slice(&[0u8; 32]);
        frame.extend_from_slice(b"payload");
        backend.store_chunk(&mut stage, &frame);
        let err = backend.commit(stage).unwrap_err().to_string();
        assert!(err.contains("缺失块未送达"), "实际：{err}");
    }

    #[test]
    fn commit_lands_zero_chunk_node() {
        // 回归:空清单节点(0 块)无缺失、无传输,commit 必须照常落树。
        let (_dir, backend) = open_backend("commit-zero-chunk");
        let stage = backend.stage_import(ImportRequest {
            node: RemoteNodeMeta {
                id: "snap_empty_1".into(),
                branch: "main".into(),
                kind: KIND_DISK.into(),
                origin: "node-a".into(),
                ..Default::default()
            },
            manifest: Manifest {
                version: 1,
                files: vec![],
                dirs: vec![],
            },
            chunks: vec![],
        });
        assert!(stage.missing().is_empty());
        let id = backend.commit(stage).unwrap();
        assert_eq!(id, "snap_empty_1");
        let engine = backend.engine.lock().unwrap();
        assert!(engine.tree.get("snap_empty_1").is_ok());
    }

    #[test]
    fn commit_is_idempotent_for_existing_node() {
        // commit 恒发后,幂等重推会以同 id 再次 commit:回既有 id,不报落树失败。
        let (_dir, backend) = open_backend("commit-idempotent");
        let data = b"payload";
        let (manifest, chunks) = sample_manifest(data);

        let mut stage = backend.stage_import(ImportRequest {
            node: RemoteNodeMeta {
                id: "snap_dup_1".into(),
                branch: "main".into(),
                kind: KIND_DISK.into(),
                ..Default::default()
            },
            manifest: manifest.clone(),
            chunks: chunks.clone(),
        });
        for hash in &chunks {
            let mut frame = vec![mixum_protocol::frames::chan::CHUNK];
            frame.extend_from_slice(&Sha256::digest(data));
            frame.extend_from_slice(data);
            let _ = hash; // 帧内含哈希,store_chunk 按帧内哈希落 CAS
            backend.store_chunk(&mut stage, &frame);
        }
        let id = backend.commit(stage).unwrap();
        assert_eq!(id, "snap_dup_1");

        // 第二次:同 id、缺失列表为空(块已在 CAS)。
        let stage2 = backend.stage_import(ImportRequest {
            node: RemoteNodeMeta {
                id: "snap_dup_1".into(),
                branch: "main".into(),
                kind: KIND_DISK.into(),
                ..Default::default()
            },
            manifest,
            chunks,
        });
        assert!(stage2.missing().is_empty());
        let id2 = backend.commit(stage2).unwrap();
        assert_eq!(id2, "snap_dup_1");
    }

    #[test]
    fn commit_lands_node_and_marks_remote_root_when_parent_missing() {
        let (_dir, backend) = open_backend("commit-remote-root");
        let data = b"imported-payload";
        let (manifest, chunks) = sample_manifest(data);
        let mut stage = backend.stage_import(ImportRequest {
            node: RemoteNodeMeta {
                id: String::new(), // 空 id:由接收端生成
                parent: "snap_not_here".into(),
                branch: String::new(),
                label: "来自 mac".into(),
                kind: KIND_DISK.into(),
                origin: String::new(),
                ..Default::default()
            },
            manifest,
            chunks: chunks.clone(),
        });
        assert_eq!(stage.missing, chunks);
        assert_eq!(chunks.len(), 1, "16 字节文件应只产生一个块");
        let mut frame = vec![mixum_protocol::frames::chan::CHUNK];
        frame.extend_from_slice(&Sha256::digest(data));
        frame.extend_from_slice(data);
        backend.store_chunk(&mut stage, &frame);
        let id = backend.commit(stage).unwrap();
        assert!(id.starts_with("snap_"));
        let engine = backend.engine.lock().unwrap();
        let node = engine.tree.get(&id).unwrap();
        assert_eq!(node.parent, "");
        assert!(node.label.ends_with("（远端根）"));
        assert_eq!(node.origin, "remote"); // 空字符串按 Go strOr 补默认
        assert_eq!(node.branch, "imported");
    }
}
