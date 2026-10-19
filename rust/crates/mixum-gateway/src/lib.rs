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

use mixum_protocol::ops::{ErrCode, Cap};
use mixum_protocol::token::{Claims, Signer};
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

struct NodeConn {
    out: OutTx,
    // 版本与注册时间:M1 未消费,留给 gateway.node.list 详情与审计。
    #[allow(dead_code)]
    version: String,
    #[allow(dead_code)]
    since: i64,
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
    pub attempts: u32,
}

#[derive(Debug, Clone)]
pub struct DeviceEntry {
    pub name: String,
    pub sub: String,
    /// 签发该设备的用户主体;列表/吊销按 owner 隔离。
    pub owner: String,
    pub iat: i64,
}

pub struct Gateway {
    signer: Signer,
    nodes: RwLock<HashMap<String, NodeConn>>,
    clients: Mutex<HashMap<String, ClientConn>>,
    revoked: RwLock<HashSet<String>>,
    /// 配对码 → 用户主体(5 分钟有效,单次使用)。
    pairs: Mutex<HashMap<String, PairCode>>,
    /// 已签发设备(jti → 条目);吊销即移入 revoked 并移除。
    devices: Mutex<HashMap<String, DeviceEntry>>,
    /// 配对码失败尝试(来源 → 时间戳),用于按来源限速。
    pair_failures: Mutex<HashMap<String, Vec<i64>>>,
    seq: AtomicU64,
    started_at: i64,
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
        Arc::new(Self {
            signer,
            nodes: RwLock::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            revoked: RwLock::new(HashSet::new()),
            pairs: Mutex::new(HashMap::new()),
            devices: Mutex::new(HashMap::new()),
            pair_failures: Mutex::new(HashMap::new()),
            seq: AtomicU64::new(1),
            started_at: mixum_protocol::envelope::now_ms(),
        })
    }

    pub fn bootstrap_token(&self) -> Result<String, String> {
        let now = mixum_protocol::envelope::now_ms();
        self.signer
            .mint(Claims {
                sub: "gateway-admin".into(),
                caps: vec![Cap::Admin],
                iat: now,
                exp: now + 10 * 365 * 24 * 3600,
                jti: String::new(),
                task_id: String::new(),
                windows: vec![],
            })
            .map_err(|e| e.to_string())
    }

    pub async fn verify(&self, token: &str) -> Result<Claims, String> {
        let verifier = mixum_protocol::token::Verifier::new(self.signer.verifying_key());
        let claims = verifier.verify(token).map_err(|e| e.to_string())?;
        if self.revoked.read().await.contains(&claims.jti) && !claims.jti.is_empty() {
            return Err("令牌已吊销".into());
        }
        Ok(claims)
    }

    pub async fn node_connected(&self, name: &str, version: &str, out: OutTx) -> Result<(), String> {
        let mut nodes = self.nodes.write().await;
        if nodes.contains_key(name) {
            return Err(format!("节点重名在线:{name}"));
        }
        nodes.insert(
            name.to_owned(),
            NodeConn {
                out,
                version: version.to_owned(),
                since: mixum_protocol::envelope::now_ms(),
            },
        );
        Ok(())
    }

    pub async fn node_disconnected(&self, name: &str) {
        self.nodes.write().await.remove(name);
        // 该节点上的客户端会话标记清除:节点回归后下一封重开 session。
        let mut clients = self.clients.lock().await;
        for client in clients.values_mut() {
            client.nodes.remove(name);
        }
    }

    pub async fn node_offline(&self) -> Vec<String> {
        self.nodes.read().await.keys().cloned().collect()
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
        let nodes = self.nodes.read().await;
        for node in nodes.values() {
            let _ = node.out.send(Outbound::Text(notice.clone())).await;
        }
        self.clients.lock().await.remove(session);
    }

    /// 客户端文本信封入口:gateway 自有 op 就地应答,其余按 node 路由。
    pub async fn on_client_text(&self, session: &str, text: &str) -> Vec<Outbound> {
        let env: Value = match serde_json::from_str(text) {
            Ok(v) => v,
            Err(_) => return vec![Outbound::Text(err_envelope(None, ErrCode::BadRequest, "消息不是合法 JSON"))],
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
        let op = env.get("op").and_then(Value::as_str).unwrap_or_default().to_owned();
        let mut node_name = env
            .get("node")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if node_name.is_empty() {
            let online = self.node_offline().await;
            if online.len() == 1 {
                node_name = online[0].clone();
            } else {
                return vec![Outbound::Text(err_envelope(
                    Some(&env),
                    ErrCode::BadRequest,
                    "请指定 node(多节点在线)",
                ))];
            }
        }
        let out = Vec::new();
        {
            let nodes = self.nodes.read().await;
            let Some(node) = nodes.get(&node_name) else {
                return vec![Outbound::Text(err_envelope(
                    Some(&env),
                    ErrCode::NotFound,
                    &format!("节点不在线: {node_name}"),
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
                if node.out.send(Outbound::Text(open.to_string())).await.is_err() {
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
            if node.out.send(Outbound::Text(tagged.to_string())).await.is_err() {
                return vec![Outbound::Text(err_envelope(
                    Some(&env),
                    ErrCode::Internal,
                    &format!("节点连接异常: {node_name}"),
                ))];
            }
        }
        if let Some(client) = self.clients.lock().await.get_mut(session) {
            client.nodes.insert(node_name.clone());
        }
        let _ = op;
        out
    }

    /// 节点文本入口:带 gatewaySession → 剥离标记后直投客户端;否则忽略
    /// (gateway.heartbeat 等节点侧控制消息)。投递直接走客户端连接的
    /// 出站口——bin 层不得把返回值发回节点(那是节点的下行口)。
    pub async fn on_node_text(&self, _node: &str, text: &str) {
        let Ok(env) = serde_json::from_str::<Value>(text) else {
            return;
        };
        let Some(session) = env.get("gatewaySession").and_then(Value::as_str) else {
            return;
        };
        let mut clean = env.clone();
        if let Some(obj) = clean.as_object_mut() {
            obj.remove("gatewaySession");
        }
        let clients = self.clients.lock().await;
        if let Some(client) = clients.get(session) {
            let _ = client.out.send(Outbound::Text(clean.to_string())).await;
        }
    }

    /// 节点二进制入口:剥离会话前缀后直投对应客户端(同上,不走返回值)。
    pub async fn on_node_binary(&self, bytes: &[u8]) {
        if bytes.len() < 4 {
            return;
        }
        let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if len == 0 || bytes.len() < 4 + len {
            return;
        }
        let session = String::from_utf8_lossy(&bytes[4..4 + len]).to_string();
        let clients = self.clients.lock().await;
        if let Some(client) = clients.get(&session) {
            let _ = client.out.send(Outbound::Binary(bytes[4 + len..].to_vec())).await;
        }
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
        let code = format!("{:06}", rand::rngs::OsRng.gen_range(0..1_000_000u32));
        self.pairs.lock().await.insert(
            code.clone(),
            PairCode {
                user_sub,
                expires_at: mixum_protocol::envelope::now_ms() + 300_000,
                attempts: 0,
            },
        );
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
        // 按来源限速:窗口内最多 10 次失败。
        {
            let mut failures = self.pair_failures.lock().await;
            let recent = failures.entry(source.to_owned()).or_default();
            recent.retain(|ts| now - *ts < 60_000);
            if recent.len() >= 10 {
                return Err("配对尝试过于频繁,请稍后再试".into());
            }
        }
        let pair = {
            let mut pairs = self.pairs.lock().await;
            let attempts = pairs.get(code).map(|p| p.attempts).unwrap_or(0);
            if attempts >= 5 {
                pairs.remove(code);
            }
            pairs
                .remove(code)
                .filter(|p| p.expires_at > now)
                .map(|p| (p.user_sub, p.attempts))
        };
        let Some((user_sub, _)) = pair else {
            self.pair_failures
                .lock()
                .await
                .entry(source.to_owned())
                .or_default()
                .push(now);
            return Err("配对码无效或已过期".into());
        };
        if device_name.is_empty() || device_name.len() > 64 {
            return Err("deviceName 非法".into());
        }
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
            return Outbound::Text(err_envelope(Some(env), ErrCode::Forbidden, "仅用户会话可查设备"));
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
            return Outbound::Text(err_envelope(Some(env), ErrCode::Forbidden, "仅用户会话可吊销设备"));
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
        Outbound::Text(
            json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"gateway.token.revoke.ok","replyTo":id,"payload":{"revoked":true}})
                .to_string(),
        )
    }

    async fn issue_token(&self, session: &str, env: &Value) -> Outbound {
        let id = env.get("id").cloned().unwrap_or_default();
        let fail = |code, msg: &str| {
            Outbound::Text(err_envelope(Some(env), code, msg))
        };
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
        if name.is_empty() || name.contains(':') || name.len() > 64 {
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
        assert!(matches!(&out[0], Outbound::Text(t) if t.contains("forbidden")), "{out:?}");
    }

    #[tokio::test]
    async fn pair_claim_is_rate_limited_per_source() {
        let gw = Gateway::new(Signer::generate());
        for _ in 0..10 {
            assert!(gw.pair_claim("203.0.113.9", "000000", "dev").await.is_err());
        }
        let err = gw.pair_claim("203.0.113.9", "000000", "dev").await.unwrap_err();
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
        let list = gw.on_client_text(&bob, &json!({"v":1,"id":"p2","op":"gateway.device.list"}).to_string()).await;
        let text = match &list[0] { Outbound::Text(t) => t.clone(), o => panic!("{o:?}") };
        let value: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(value["payload"]["devices"].as_array().unwrap().len(), 0, "不应看到别人的设备");
    }


    fn mpsc_pair() -> (OutTx, mpsc::Receiver<Outbound>) {
        mpsc::channel(16)
    }

    #[tokio::test]
    async fn client_request_routes_to_node_and_reply_returns() {
        let gw = Gateway::new(Signer::generate());
        let (ntx, mut nrx) = mpsc_pair();
        gw.node_connected("a", "0.1.0", ntx).await.unwrap();
        assert!(gw.node_connected("a", "0.1.0", mpsc_pair().0).await.is_err());

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
            Outbound::Text(t) => assert!(t.contains("\"node\":\"only\"") || t.contains("gatewaySession")),
            other => panic!("期望文本,实际 {other:?}"),
        }
    }

    #[tokio::test]
    async fn multi_node_without_node_field_is_rejected() {
        let gw = Gateway::new(Signer::generate());
        gw.node_connected("a", "0.1.0", mpsc_pair().0).await.unwrap();
        gw.node_connected("b", "0.1.0", mpsc_pair().0).await.unwrap();
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
    async fn token_issue_requires_admin_and_node_token_registers() {
        let gw = Gateway::new(Signer::generate());
        let bootstrap = gw.bootstrap_token().unwrap();
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
        assert!(gw.node_connected("n1", "0.1.0", mpsc_pair().0).await.is_err());
        drop(crx);
    }
}
