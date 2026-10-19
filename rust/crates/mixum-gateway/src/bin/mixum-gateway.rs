//! mixum-gateway 二进制:M1/M2 单实例入口。`--listen` scheme 决定明文(ws://)
//! 或 TLS(wss://,自签证书持久化于 --data-dir,TOFU);启动打印 bootstrap
//! 管理令牌;节点以 gateway.register 出站注册,客户端 hello 握手后按
//! envelope.node 路由;未认证连接可经 gateway.pair.claim 换设备令牌。

use futures_util::{SinkExt, StreamExt};
use mixum_gateway::{Gateway, OutTx, Outbound};
use mixum_protocol::envelope::Envelope;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // 默认只监听回环;对外部署显式改 --listen wss://0.0.0.0:7701。
    let mut listen = "wss://127.0.0.1:7701".to_owned();
    let mut data_dir = "/var/lib/mixum-gateway".to_owned();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--listen" {
            listen = args.next().unwrap_or_else(|| listen.clone());
        } else if arg == "--data-dir" {
            data_dir = args.next().unwrap_or_else(|| data_dir.clone());
        }
    }

    let hosts = vec!["localhost".into(), "127.0.0.1".into(), "::1".into()];
    let identity = mixum_gateway::identity::load_or_create_identity(&data_dir, &hosts)?;
    let gw = Gateway::new(identity.signer);
    gw.load_state(std::path::PathBuf::from(&data_dir).join("gateway-state.json"))
        .await;
    let bootstrap = gw.bootstrap_token().await?;
    eprintln!("mixum-gateway listening on {listen}");
    eprintln!("fingerprint: {}", identity.fingerprint);
    eprintln!("bootstrap token: {bootstrap}");

    // scheme 决定明文/TLS。
    let (scheme, addr) = listen
        .split_once("://")
        .map(|(s, a)| (s.to_owned(), a.to_owned()))
        .unwrap_or_else(|| ("ws".into(), listen.clone()));
    let listener = TcpListener::bind(&addr).await?;
    let tls_acceptor = if scheme == "wss" {
        Some(tokio_rustls::TlsAcceptor::from(Arc::clone(&identity.tls)))
    } else {
        None
    };
    loop {
        let (stream, peer_addr) = listener.accept().await?;
        let peer = peer_addr.to_string();
        let gw = Arc::clone(&gw);
        let tls_acceptor = tls_acceptor.clone();
        tokio::spawn(async move {
            match tls_acceptor {
                Some(acceptor) => match acceptor.accept(stream).await {
                    Ok(tls_stream) => match tokio_tungstenite::accept_async(tls_stream).await {
                        Ok(ws) => {
                            let _ = handle_connection(gw, ws, peer.clone()).await;
                        }
                        Err(e) => eprintln!("WS 握手失败(TLS): {e}"),
                    },
                    Err(e) => eprintln!("TLS 握手失败: {e}"),
                },
                None => {
                    if let Ok(ws) = tokio_tungstenite::accept_async(stream).await {
                        let _ = handle_connection(gw, ws, peer.clone()).await;
                    }
                }
            }
        });
    }
}

#[allow(clippy::large_enum_variant)]
enum Side {
    /// 未分类:等待首条消息(gateway.register / hello / gateway.pair.claim)。
    Pending,
    Node(String),
    Client(String),
}

async fn handle_connection<S>(
    gw: Arc<Gateway>,
    ws: tokio_tungstenite::WebSocketStream<S>,
    peer: String,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    let (mut sink, mut source) = ws.split();
    let (out_tx, mut out_rx) = tokio::sync::mpsc::channel::<Outbound>(256);

    let mut side = Side::Pending;

    let writer = tokio::spawn(async move {
        while let Some(out) = out_rx.recv().await {
            let msg = match out {
                Outbound::Text(t) => Message::Text(t.into()),
                Outbound::Binary(b) => Message::Binary(b.into()),
                Outbound::Close => {
                    let _ = sink.close().await;
                    break;
                }
            };
            if sink.send(msg).await.is_err() {
                break;
            }
        }
    });

    while let Some(msg) = source.next().await {
        let msg = match msg {
            Ok(m) => m,
            Err(_) => break,
        };
        match msg {
            // Ping 的 Pong 由 tungstenite 在写入时自动附带
            Message::Text(t) => {
                let t = t.to_string();
                match &side {
                    Side::Pending => {
                        side = classify(&gw, &t, out_tx.clone(), &peer).await;
                        if let Side::Pending = side {
                            break; // 分类失败已回错误,断开
                        }
                    }
                    Side::Node(name) => {
                        gw.on_node_text(name, &t).await; // 核心直投客户端
                    }
                    Side::Client(session) => {
                        let session = session.clone();
                        for out in gw.on_client_text(&session, &t).await {
                            let _ = out_tx.send(out).await;
                        }
                    }
                }
            }
            Message::Binary(b) => {
                if let Side::Node(_) = &side {
                    gw.on_node_binary(&b).await; // 核心直投客户端
                }
            }
            Message::Close(_) => break,
            _ => {}
        }
    }

    match side {
        Side::Node(name) => gw.node_disconnected(&name).await,
        Side::Client(session) => gw.client_disconnected(&session).await,
        Side::Pending => {}
    }
    // 等写泵排空挂起应答(如 pair.claim 的令牌回包),而非直接 abort。
    let _ = tokio::time::timeout(std::time::Duration::from_millis(300), writer).await;
    Ok(())
}

/// 首条消息分类:gateway.register(节点令牌)→ Node;hello(用户/设备令牌)
/// → Client;gateway.pair.claim(无凭据)→ 签发设备令牌后保持 Pending(设备
/// 拿到令牌即自行重连 hello)。
async fn classify(gw: &Arc<Gateway>, text: &str, out: OutTx, source: &str) -> Side {
    let env: Value = serde_json::from_str(text).unwrap_or(Value::Null);
    let op = env.get("op").and_then(Value::as_str).unwrap_or_default();
    let token = env
        .pointer("/payload/token")
        .and_then(Value::as_str)
        .unwrap_or_default();
    match op {
        "gateway.register" => {
            let name = env
                .pointer("/payload/name")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            let version = env
                .pointer("/payload/version")
                .and_then(Value::as_str)
                .unwrap_or("0")
                .to_owned();
            if name.is_empty() {
                let _ = out.send(Outbound::Text(err_text(Some(&env), "name 必填"))).await;
                return Side::Pending;
            }
            match gw.verify(token).await {
                Ok(claims) if claims.sub.starts_with("node:") => {
                    if claims.sub != format!("node:{name}") {
                        let _ = out
                            .send(Outbound::Text(err_text(
                                Some(&env),
                                "节点令牌与 name 不匹配",
                            )))
                            .await;
                        return Side::Pending;
                    }
                    if let Err(e) = gw.node_connected(&name, &version, out.clone()).await {
                        let _ = out.send(Outbound::Text(err_text(Some(&env), &e))).await;
                        return Side::Pending;
                    }
                    eprintln!("节点已注册: {name} ({version})");
                    let _ = out
                        .send(Outbound::Text(
                            json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"gateway.register.ok","replyTo":env.get("id").cloned().unwrap_or_default(),"payload":{"lease":30}})
                                .to_string(),
                        ))
                        .await;
                    Side::Node(name)
                }
                _ => {
                    let _ = out
                        .send(Outbound::Text(err_text(Some(&env), "节点令牌无效")))
                        .await;
                    Side::Pending
                }
            }
        }
        "gateway.pair.claim" => {
            // 未认证设备:配对码 + 设备名 → 设备令牌;设备随后自行重连 hello。
            let code = env
                .pointer("/payload/code")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let device_name = env
                .pointer("/payload/deviceName")
                .and_then(Value::as_str)
                .unwrap_or("未命名设备");
            let reply = match gw.pair_claim(source, code, device_name).await {
                Ok((token, device_id, _user)) => json!({
                    "v":1,"id":mixum_protocol::envelope::new_id(),"op":"gateway.pair.claim.ok",
                    "replyTo":env.get("id").cloned().unwrap_or_default(),
                    "payload":{"token":token,"deviceId":device_id,"deviceName":device_name}
                })
                .to_string(),
                Err(e) => err_text(Some(&env), &format!("配对失败: {e}")),
            };
            let _ = out.send(Outbound::Text(reply)).await;
            Side::Pending
        }
        // MAP v1 用 session.hello;过渡期继续接受 M1 的 hello。
        "hello" | "session.hello" => {
            let claims = match gw.verify(token).await {
                // 设备令牌(device:*)与用户令牌(user:*)都是客户端;节点
                // 令牌不得作客户端。
                Ok(c)
                    if c.sub.starts_with("user:")
                        || c.sub.starts_with("device:")
                        || c.sub == "gateway-admin" =>
                {
                    c
                }
                _ => {
                    let _ = out
                        .send(Outbound::Text(err_text(Some(&env), "用户令牌无效")))
                        .await;
                    return Side::Pending;
                }
            };
            let session = gw
                .client_connected_with_caps(&claims.sub, claims.caps.clone(), out.clone())
                .await;
            eprintln!("客户端接入: {} ({})", claims.sub, session);
            let mut hello = Envelope::new(
                &format!("{op}.ok"),
                Some(json!({
                    "sessionId": session,
                    "server": "mixum-gateway/0.1",
                    "arch": std::env::consts::ARCH,
                    "kernel": "gateway",
                    "caps": claims.caps,
                    "versions": [1],
                    "heartbeat": 30,
                })),
            );
            hello.reply_to = env.get("id").and_then(Value::as_str).unwrap_or_default().to_owned();
            let _ = out.send(Outbound::Text(hello.to_json())).await;
            Side::Client(session)
        }
        _ => {
            let _ = out
                .send(Outbound::Text(err_text(
                    None,
                    "首条消息须为 gateway.register、hello 或 gateway.pair.claim",
                )))
                .await;
            Side::Pending
        }
    }
}

fn err_text(env: Option<&Value>, message: &str) -> String {
    let id = env
        .and_then(|v| v.get("id"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if id.is_empty() {
        json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"error.err","payload":{"code":"unauthorized","message":message}}).to_string()
    } else {
        json!({"v":1,"id":mixum_protocol::envelope::new_id(),"op":"error.err","replyTo":id,"payload":{"code":"unauthorized","message":message}}).to_string()
    }
}
